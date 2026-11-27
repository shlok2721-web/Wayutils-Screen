use std::collections::HashMap;
use std::sync::Once;

use image::{GrayImage, RgbaImage};
use opencv::core::{self, Point2f, Rect, Scalar, Vector, CV_8UC1, NORM_HAMMING};
use opencv::features2d;
use opencv::imgproc;
use opencv::opencv_has_inherent_feature_opencl;
use opencv::prelude::*;

use super::fast_match::rgba_to_gray;
use super::{
    OrbEstimate, ORB_BOTTOM_IGNORE_RATIO, ORB_MAX_DX, ORB_MAX_FEATURES, ORB_MAX_GEOMETRY_DRIFT,
    ORB_MIN_IGNORE_PX, ORB_MIN_INLIERS, ORB_MIN_KEYPOINTS, ORB_MIN_MATCHES, ORB_SIDE_IGNORE_RATIO,
    ORB_TOP_IGNORE_RATIO,
};
use crate::opencv_compat::{estimate_affine_partial_2d, RANSAC};

static OPENCV_RUNTIME_INIT: Once = Once::new();

/// 初始化 OpenCV 运行环境并关闭 OpenCL 路径
///
/// 该方法无传参和返回值，在进程内只执行一次
pub(crate) fn init_opencv_runtime() {
    OPENCV_RUNTIME_INIT.call_once(|| {
        std::env::set_var("OPENCV_OPENCL_RUNTIME", "disabled");
        opencv_has_inherent_feature_opencl! {
            if let Err(err) = core::set_use_opencl(false) {
                log::error!("【OpenCV】【关闭 OpenCL】关闭运行时失败: {err}");
            }
        }
    });
}

/// 使用 ORB 特征估算相邻截图的垂直偏移
///
/// `prev` 为上一帧，`frame` 为当前帧，`min_overlap` 为最小重叠高度；返回偏移估算或无匹配结果
pub(super) fn estimate_orb_offset(
    prev: &RgbaImage,
    frame: &RgbaImage,
    min_overlap: u32,
) -> opencv::Result<Option<OrbEstimate>> {
    init_opencv_runtime();

    if prev.width() != frame.width() || prev.height() != frame.height() {
        return Ok(None);
    }
    if prev.width() < 80 || prev.height() < 120 {
        return Ok(None);
    }

    // 1. 转换灰度图并构建特征区域掩码
    let prev_gray = rgba_to_gray(prev);
    let frame_gray = rgba_to_gray(frame);
    let prev_mat = gray_to_mat(&prev_gray)?;
    let frame_mat = gray_to_mat(&frame_gray)?;
    let mask = build_feature_mask(prev.width(), prev.height())?;

    let mut orb = features2d::ORB::create_def()?;
    orb.set_max_features(ORB_MAX_FEATURES)?;

    // 2. 提取相邻截图的 ORB 关键点与描述子
    let mut prev_keypoints = Vector::<core::KeyPoint>::new();
    let mut prev_descriptors = core::Mat::default();
    orb.detect_and_compute_def(&prev_mat, &mask, &mut prev_keypoints, &mut prev_descriptors)?;

    let mut curr_keypoints = Vector::<core::KeyPoint>::new();
    let mut curr_descriptors = core::Mat::default();
    orb.detect_and_compute_def(
        &frame_mat,
        &mask,
        &mut curr_keypoints,
        &mut curr_descriptors,
    )?;

    if prev_keypoints.len() < ORB_MIN_KEYPOINTS
        || curr_keypoints.len() < ORB_MIN_KEYPOINTS
        || prev_descriptors.empty()
        || curr_descriptors.empty()
    {
        return Ok(None);
    }

    let matcher = features2d::BFMatcher::create(NORM_HAMMING, false)?;
    let mut matches = Vector::<Vector<core::DMatch>>::new();
    matcher.knn_train_match_def(&curr_descriptors, &prev_descriptors, &mut matches, 2)?;

    let mut curr_points = Vector::<Point2f>::new();
    let mut prev_points = Vector::<Point2f>::new();
    let mut motion_votes = Vec::<(f64, f64, f64)>::new();
    let mut raw_matches = 0usize;

    // 3. 使用比率测试和垂直滚动约束筛选特征匹配
    for pair in matches.iter() {
        if pair.len() < 2 {
            continue;
        }

        let best = pair.get(0)?;
        let second = pair.get(1)?;

        if best.distance >= second.distance * 0.78 {
            continue;
        }

        let curr_pt = curr_keypoints.get(best.query_idx as usize)?.pt();
        let prev_pt = prev_keypoints.get(best.train_idx as usize)?.pt();
        let dx = (prev_pt.x - curr_pt.x) as f64;
        let dy = (prev_pt.y - curr_pt.y) as f64;

        // Keep plausible feature matches even when `dy` is nearly zero:
        // stationary matches can identify a monitor or pane that is not
        // moving while another horizontal part of the selection is moving.
        // Only non-zero motion matches are used by RANSAC below.
        if dx.abs() > ORB_MAX_DX * 2.0 {
            continue;
        }
        motion_votes.push((dy, curr_pt.x as f64, curr_pt.y as f64));
        if dy.abs() <= 1.0 {
            continue;
        }

        curr_points.push(curr_pt);
        prev_points.push(prev_pt);
        raw_matches += 1;
    }

    // A single screenshot rectangle can cross two independent displays or
    // two independently scrolling panes. In that case there is no single
    // valid vertical offset for the whole frame. Accepting whichever motion
    // happens to win RANSAC can repeatedly append the wrong monitor/pane.
    // Return a deliberately rejected estimate (very high confidence cost)
    // so the caller preserves its current anchor instead of falling through
    // to a weaker matcher that might guess a different offset.
    if has_conflicting_motion_modes(
        &motion_votes,
        prev.width() as f64,
        prev.height() as f64,
    ) {
        log::warn!(
            "【OpenCV ORB】检测到多个独立的垂直运动区域；本帧不参与拼接，避免重复追加"
        );
        return Ok(Some(OrbEstimate {
            dy: 0.0,
            confidence: f32::MAX,
        }));
    }
    if raw_matches < ORB_MIN_MATCHES
        && has_dominant_stationary_mode(
            &motion_votes,
            raw_matches,
            prev.width() as f64,
            prev.height() as f64,
        )
    {
        // A broad, well-supported zero-motion match is stronger evidence
        // than a template fallback on repetitive content. Report no movement
        // so the caller does not invent a scroll offset and duplicate pixels.
        return Ok(Some(OrbEstimate {
            dy: 0.0,
            confidence: 0.0,
        }));
    }
    if raw_matches < ORB_MIN_MATCHES {
        return Ok(None);
    }

    let mut inliers = core::Mat::default();
    // 4. 通过部分仿射变换估算垂直偏移
    let affine = estimate_affine_partial_2d(
        &curr_points,
        &prev_points,
        &mut inliers,
        RANSAC,
        3.0,
        2000,
        0.99,
        10,
    )?;

    if affine.empty() {
        return Ok(None);
    }

    let a = *affine.at_2d::<f64>(0, 0)?;
    let b = *affine.at_2d::<f64>(0, 1)?;
    let c = *affine.at_2d::<f64>(1, 0)?;
    let d = *affine.at_2d::<f64>(1, 1)?;
    let tx = *affine.at_2d::<f64>(0, 2)?;
    let ty = *affine.at_2d::<f64>(1, 2)?;

    let scale = ((a * a + c * c).sqrt() + (b * b + d * d).sqrt()) * 0.5;
    let geom_drift = (a - 1.0).abs() + (d - 1.0).abs() + b.abs() + c.abs();

    // 5. 校验水平漂移、缩放和几何形变
    // `ty` (like `dy` above) is signed: positive for a downward scroll,
    // negative for an upward one. Both magnitudes are checked against the
    // same overlap bound symmetrically.
    let max_ty = (prev.height().saturating_sub(min_overlap)) as f64;
    if tx.abs() > ORB_MAX_DX
        || ty.abs() <= 1.0
        || ty.abs() >= max_ty
        || (scale - 1.0).abs() > ORB_MAX_GEOMETRY_DRIFT
        || geom_drift > ORB_MAX_GEOMETRY_DRIFT
    {
        return Ok(None);
    }

    let mut inlier_count = 0usize;
    for row in 0..inliers.rows() {
        if *inliers.at_2d::<u8>(row, 0)? != 0 {
            inlier_count += 1;
        }
    }

    if inlier_count < ORB_MIN_INLIERS {
        return Ok(None);
    }

    let inlier_ratio = inlier_count as f32 / raw_matches as f32;
    let confidence = (1.0 - inlier_ratio) * 3.5
        + (tx.abs() as f32 / ORB_MAX_DX as f32)
        + (geom_drift as f32 * 6.0);

    Ok(Some(OrbEstimate { dy: ty, confidence }))
}


/// Returns true when stationary descriptor matches dominate the frame and
/// cover a broad two-dimensional area. This prevents a template fallback from
/// inventing motion for an otherwise unchanged frame, while avoiding a small
/// fixed header being mistaken for a fully stationary page.
fn has_dominant_stationary_mode(
    votes: &[(f64, f64, f64)],
    moving_matches: usize,
    frame_width: f64,
    frame_height: f64,
) -> bool {
    if frame_width <= 0.0 || frame_height <= 0.0 {
        return false;
    }

    let stationary: Vec<(f64, f64)> = votes
        .iter()
        .filter_map(|&(dy, x, y)| (dy.abs() <= 1.0).then_some((x, y)))
        .collect();
    if stationary.len() < ORB_MIN_INLIERS
        || stationary.len() < moving_matches.saturating_mul(2)
    {
        return false;
    }

    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for (x, y) in stationary {
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }

    (max_x - min_x) >= frame_width * 0.15
        && (max_y - min_y) >= frame_height * 0.15
}

/// Detects two well-supported, spatially separated vertical-motion modes.
/// `votes` contains `(vertical displacement, current-frame x, y)` for
/// plausible descriptor matches, including stationary matches. This catches
/// independent horizontal or vertical display regions before one global
/// offset is estimated.
fn has_conflicting_motion_modes(
    votes: &[(f64, f64, f64)],
    frame_width: f64,
    frame_height: f64,
) -> bool {
    if votes.len() < ORB_MIN_INLIERS || frame_width <= 0.0 || frame_height <= 0.0 {
        return false;
    }

    // Eight-pixel bins absorb normal keypoint localization jitter while
    // keeping genuinely different scroll offsets separate.
    let mut bins: HashMap<i64, Vec<(f64, f64)>> = HashMap::new();
    for &(dy, x, y) in votes {
        let bin = (dy / 8.0).round() as i64;
        bins.entry(bin).or_default().push((x, y));
    }

    // Keep each mode's spatial extent as well as its centre. A fixed web-page
    // header and the scrolling body can have different dy values, but the
    // header is a narrow horizontal band; it must not look like a second
    // independently moving monitor/pane.
    let mut modes: Vec<(usize, f64, f64, f64, f64)> = bins
        .into_values()
        .filter_map(|points| {
            let count = points.len();
            if count < ORB_MIN_INLIERS / 2 {
                return None;
            }
            let (mut sum_x, mut sum_y) = (0.0, 0.0);
            let (mut min_x, mut max_x) = (f64::INFINITY, f64::NEG_INFINITY);
            let (mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY);
            for &(x, y) in &points {
                sum_x += x;
                sum_y += y;
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
            Some((
                count,
                sum_x / count as f64,
                sum_y / count as f64,
                max_x - min_x,
                max_y - min_y,
            ))
        })
        .collect();
    modes.sort_unstable_by(|a, b| b.0.cmp(&a.0));

    let Some(&(largest_count, largest_x, largest_y, largest_span_x, largest_span_y)) = modes.first() else {
        return false;
    };
    let Some(&(second_count, second_x, second_y, second_span_x, second_span_y)) = modes.get(1) else {
        return false;
    };

    let separated_horizontally =
        (largest_x - second_x).abs() >= frame_width * 0.20
            && largest_span_x >= frame_width * 0.10
            && second_span_x >= frame_width * 0.10;
    let separated_vertically =
        (largest_y - second_y).abs() >= frame_height * 0.20
            && largest_span_y >= frame_height * 0.15
            && second_span_y >= frame_height * 0.15;

    second_count >= ORB_MIN_INLIERS / 2
        && second_count.saturating_mul(4) >= largest_count
        && (separated_horizontally || separated_vertically)
}

#[cfg(test)]
mod motion_mode_tests {
    use super::{has_conflicting_motion_modes, has_dominant_stationary_mode};

    #[test]
    fn detects_two_independent_scroll_areas_on_separate_sides() {
        let mut votes = Vec::new();
        for i in 0..40 {
            votes.push((120.0 + (i % 3) as f64, 100.0 + (i * 17 % 700) as f64, 300.0));
            votes.push((260.0 + (i % 3) as f64, 1100.0 + (i * 17 % 700) as f64, 300.0));
        }
        assert!(has_conflicting_motion_modes(&votes, 1920.0, 1080.0));
    }

    #[test]
    fn detects_stationary_monitor_beside_scrolling_monitor() {
        let mut votes = Vec::new();
        for i in 0..40 {
            votes.push((0.0, 100.0 + (i * 17 % 700) as f64, 300.0));
            votes.push((160.0 + (i % 3) as f64, 1100.0 + (i * 17 % 700) as f64, 300.0));
        }
        assert!(has_conflicting_motion_modes(&votes, 1920.0, 1080.0));
    }

    #[test]
    fn detects_independent_regions_stacked_vertically() {
        let mut votes = Vec::new();
        for i in 0..40 {
            votes.push((0.0, 900.0, 80.0 + (i * 13 % 430) as f64));
            votes.push((140.0 + (i % 3) as f64, 900.0, 650.0 + (i * 13 % 430) as f64));
        }
        assert!(has_conflicting_motion_modes(&votes, 1920.0, 1080.0));
    }

    #[test]
    fn does_not_confuse_fixed_header_with_a_second_display() {
        let mut votes = Vec::new();
        for i in 0..40 {
            votes.push((0.0, (i * 47 % 1800) as f64, 24.0 + (i % 4) as f64));
            votes.push((160.0 + (i % 3) as f64, (i * 43 % 1800) as f64, 250.0 + (i * 19 % 700) as f64));
        }
        assert!(!has_conflicting_motion_modes(&votes, 1920.0, 1080.0));
    }

    #[test]
    fn accepts_one_scroll_area_with_normal_match_jitter() {
        let votes: Vec<_> = (0..80)
            .map(|i| (
                180.0 + (i % 5) as f64,
                (i * 19 % 1900) as f64,
                (i * 13 % 1050) as f64,
            ))
            .collect();
        assert!(!has_conflicting_motion_modes(&votes, 1920.0, 1080.0));
    }

    #[test]
    fn recognizes_a_broad_static_frame_instead_of_using_template_fallback() {
        let votes: Vec<_> = (0..80)
            .map(|i| (0.0, (i * 31 % 1900) as f64, (i * 17 % 1050) as f64))
            .collect();
        assert!(has_dominant_stationary_mode(&votes, 2, 1920.0, 1080.0));
    }

    #[test]
    fn does_not_treat_a_narrow_fixed_header_as_a_static_frame() {
        let votes: Vec<_> = (0..60)
            .map(|i| (0.0, (i * 31 % 1900) as f64, (i % 5 * 2) as f64))
            .collect();
        assert!(!has_dominant_stationary_mode(&votes, 20, 1920.0, 1080.0));
    }

    #[test]
    fn ignores_small_secondary_motion_cluster() {
        let mut votes: Vec<_> = (0..60)
            .map(|i| (180.0, (i * 20 % 900) as f64, 300.0))
            .collect();
        votes.extend((0..8).map(|i| (300.0, 1200.0 + i as f64, 300.0)));
        assert!(!has_conflicting_motion_modes(&votes, 1920.0, 1080.0));
    }
}

/// 将灰度图复制到 OpenCV 矩阵
///
/// `gray` 为灰度图；返回 `CV_8UC1` 矩阵
fn gray_to_mat(gray: &GrayImage) -> opencv::Result<core::Mat> {
    let rows = gray.height() as i32;
    let cols = gray.width() as i32;
    let mut mat = core::Mat::new_rows_cols_with_default(rows, cols, CV_8UC1, Scalar::all(0.0))?;

    for y in 0..rows {
        for x in 0..cols {
            *mat.at_2d_mut::<u8>(y, x)? = gray.get_pixel(x as u32, y as u32)[0];
        }
    }

    Ok(mat)
}

/// 构建忽略固定边缘区域的 ORB 特征掩码
///
/// `width` 和 `height` 为截图尺寸；返回单通道掩码矩阵
fn build_feature_mask(width: u32, height: u32) -> opencv::Result<core::Mat> {
    let mut mask = core::Mat::new_rows_cols_with_default(
        height as i32,
        width as i32,
        CV_8UC1,
        Scalar::all(0.0),
    )?;

    let (roi_x, roi_y, roi_w, roi_h) = content_roi(width, height);
    let rect = Rect::new(roi_x as i32, roi_y as i32, roi_w as i32, roi_h as i32);

    imgproc::rectangle(&mut mask, rect, Scalar::all(255.0), -1, imgproc::LINE_8, 0)?;

    Ok(mask)
}

/// 计算排除固定边缘后的内容区域
///
/// `width` 和 `height` 为截图尺寸；返回内容区域的横坐标、纵坐标、宽度和高度
pub(super) fn content_roi(width: u32, height: u32) -> (u32, u32, u32, u32) {
    let side = ((width as f32 * ORB_SIDE_IGNORE_RATIO) as u32).max(ORB_MIN_IGNORE_PX);
    let top = ((height as f32 * ORB_TOP_IGNORE_RATIO) as u32).max(ORB_MIN_IGNORE_PX);
    let bottom = ((height as f32 * ORB_BOTTOM_IGNORE_RATIO) as u32).max(ORB_MIN_IGNORE_PX);
    let x = side.min(width.saturating_sub(1));
    let y = top.min(height.saturating_sub(1));
    let roi_w = width.saturating_sub(x.saturating_mul(2)).max(1);
    let roi_h = height.saturating_sub(y).saturating_sub(bottom).max(1);
    (x, y, roi_w, roi_h)
}
