use image::{imageops, Rgba};

use super::*;

/// 构造包含多种纹理的长截图测试画布
///
/// `width` 和 `height` 为画布尺寸；返回用于 ORB 匹配的测试图像
fn make_scroll_canvas(width: u32, height: u32) -> RgbaImage {
    let mut img = RgbaImage::from_pixel(width, height, Rgba([245, 245, 245, 255]));

    for y in (0..height).step_by(36) {
        let accent = ((y / 3) % 180) as u8;
        for x in 24..width.saturating_sub(24) {
            let stripe = if (x / 7 + y / 11) % 2 == 0 { 220 } else { 180 };
            img.put_pixel(x, y, Rgba([accent, stripe, 80, 255]));
            if y + 1 < height {
                img.put_pixel(x, y + 1, Rgba([30, 30, 30, 255]));
            }
        }
    }

    for block in 0..10 {
        let y0 = 30 + block * 80;
        let block_h = 34 + (block % 3) * 8;
        let color = [
            ((40u16 + block as u16 * 17) % 200) as u8,
            ((90u16 + block as u16 * 11) % 200) as u8,
            ((140u16 + block as u16 * 19) % 200) as u8,
            255,
        ];
        for y in y0..(y0 + block_h).min(height) {
            for x in 30..width.saturating_sub(30) {
                if x % (9 + block % 5) == 0 || y % (7 + block % 4) == 0 {
                    img.put_pixel(x, y, Rgba(color));
                }
            }
        }
    }

    for col in [42, 96, 154, 211, 268] {
        if col >= width {
            continue;
        }
        for y in 20..height.saturating_sub(20) {
            if (y / 13) % 3 != 0 {
                img.put_pixel(col, y, Rgba([20, 20, 20, 255]));
            }
        }
    }

    img
}

/// 从长画布裁剪一帧测试截图
///
/// `canvas` 为长画布，`y` 为起始行，`height` 为帧高度；返回裁剪后的截图
fn crop_frame(canvas: &RgbaImage, y: u32, height: u32) -> RgbaImage {
    imageops::crop_imm(canvas, 0, y, canvas.width(), height).to_image()
}

/// 构造低特征密度的横线测试画布
///
/// `width` 和 `height` 为画布尺寸；返回用于模板回退测试的图像
fn make_line_canvas(width: u32, height: u32) -> RgbaImage {
    let mut img = RgbaImage::from_pixel(width, height, Rgba([250, 250, 250, 255]));
    let mut y = 16u32;
    let mut band = 0u32;

    while y < height.saturating_sub(16) {
        let band_h = 6 + (band % 5) * 4;
        let gray = (40 + ((band * 29) % 160)) as u8;
        for yy in y..(y + band_h).min(height) {
            for x in 28..width.saturating_sub(28) {
                img.put_pixel(x, yy, Rgba([gray, gray, gray, 255]));
            }
        }
        y += band_h + 9 + (band % 4) * 3;
        band += 1;
    }

    img
}

/// 验证 ORB 能估算标准垂直滚动偏移；该测试无传参和返回值
#[test]
fn opencv_orb_estimates_vertical_offset() {
    let canvas = make_scroll_canvas(320, 1000);
    let first = crop_frame(&canvas, 0, 320);
    let second = crop_frame(&canvas, 84, 320);

    let estimate = estimate_orb_offset(&first, &second, 120)
        .expect("opencv estimate")
        .expect("orb match");

    assert!((estimate.dy - 84.0).abs() <= 4.0, "dy={}", estimate.dy);
    assert!(
        estimate.confidence < 3.5,
        "confidence={}",
        estimate.confidence
    );
}

/// 验证坏帧不会替换 ORB 锚点；该测试无传参和返回值
#[test]
fn opencv_orb_keeps_anchor_after_bad_frame() {
    let canvas = make_scroll_canvas(320, 1000);
    let first = crop_frame(&canvas, 0, 320);
    let shifted = crop_frame(&canvas, 96, 320);
    let bad = RgbaImage::from_pixel(320, 320, Rgba([255, 255, 255, 255]));

    let mut stitcher = Stitcher::new(MatchConfig {
        min_overlap: 120,
        accept_diff: 3.5,
        min_append: 10,
        approx_diff: 1.0,
        algorithm: Algorithm::OpenCvOrb,
        match_width: 320,
    });

    assert!(matches!(
        stitcher.push_frame(first),
        StitchOutcome::FirstFrame
    ));
    assert!(matches!(stitcher.push_frame(bad), StitchOutcome::NoMatch));

    match stitcher.push_frame(shifted) {
        StitchOutcome::Appended { added } => assert!((92..=100).contains(&added), "{added}"),
        _ => panic!("expected appended after bad frame"),
    }
}

/// 验证低特征截图会回退到模板匹配；该测试无传参和返回值
#[test]
fn opencv_orb_falls_back_to_template_on_low_feature_frames() {
    let canvas = make_line_canvas(320, 1000);
    let first = crop_frame(&canvas, 0, 320);
    let second = crop_frame(&canvas, 72, 320);

    let mut stitcher = Stitcher::new(MatchConfig {
        min_overlap: 120,
        accept_diff: 3.5,
        min_append: 10,
        approx_diff: 1.0,
        algorithm: Algorithm::OpenCvOrb,
        match_width: 320,
    });

    assert!(matches!(
        stitcher.push_frame(first),
        StitchOutcome::FirstFrame
    ));

    match stitcher.push_frame(second) {
        StitchOutcome::Appended { added } => assert!((68..=76).contains(&added), "{added}"),
        _ => panic!("expected appended via template fallback"),
    }
}

/// 验证降低最小重叠高度后可以处理较大滚动跨度；该测试无传参和返回值
#[test]
fn opencv_orb_relaxed_overlap_handles_large_jump() {
    let canvas = make_scroll_canvas(320, 1200);
    let first = crop_frame(&canvas, 0, 320);
    let second = crop_frame(&canvas, 208, 320);

    let mut stitcher = Stitcher::new(MatchConfig {
        min_overlap: 120,
        accept_diff: 3.5,
        min_append: 10,
        approx_diff: 1.0,
        algorithm: Algorithm::OpenCvOrb,
        match_width: 320,
    });

    assert!(matches!(
        stitcher.push_frame(first),
        StitchOutcome::FirstFrame
    ));

    match stitcher.push_frame(second) {
        StitchOutcome::Appended { added } => assert!((202..=214).contains(&added), "{added}"),
        _ => panic!("expected appended via relaxed overlap"),
    }
}

/// 验证 ORB 能估算反向（向上）滚动偏移；该测试无传参和返回值
#[test]
fn opencv_orb_estimates_upward_scroll_offset() {
    let canvas = make_scroll_canvas(320, 1000);
    // `second` is cropped from *above* `first`, simulating an upward scroll.
    let first = crop_frame(&canvas, 84, 320);
    let second = crop_frame(&canvas, 0, 320);

    let estimate = estimate_orb_offset(&first, &second, 120)
        .expect("opencv estimate")
        .expect("orb match");

    assert!((estimate.dy + 84.0).abs() <= 4.0, "dy={}", estimate.dy);
}

/// 验证 Stitcher 在向上滚动时会把新内容拼接到顶部而不是丢弃；该测试无传参和返回值
#[test]
fn stitcher_prepends_content_on_upward_scroll() {
    let canvas = make_scroll_canvas(320, 1000);
    // Start lower in the canvas, then scroll upward (decreasing y).
    let first = crop_frame(&canvas, 300, 320);
    let second = crop_frame(&canvas, 204, 320);

    let mut stitcher = Stitcher::new(MatchConfig {
        min_overlap: 120,
        accept_diff: 3.5,
        min_append: 10,
        approx_diff: 1.0,
        algorithm: Algorithm::OpenCvOrb,
        match_width: 320,
    });

    assert!(matches!(
        stitcher.push_frame(first),
        StitchOutcome::FirstFrame
    ));

    let before_height = stitcher.full_image().expect("full image").height();

    match stitcher.push_frame(second) {
        StitchOutcome::Appended { added } => assert!((88..=100).contains(&added), "{added}"),
        other => panic!("expected content prepended after upward scroll, got {other:?}"),
    }

    let full = stitcher.full_image().expect("full image");
    assert!(full.height() > before_height);

    // The newly revealed top strip should match the canvas content that
    // used to sit above `first`, not a duplicate of what was already there.
    let expected_top = crop_frame(&canvas, 204, 10);
    let actual_top = crop_frame(&full, 0, 10);
    let mut diff_sum: u64 = 0;
    for (a, b) in actual_top.pixels().zip(expected_top.pixels()) {
        diff_sum += (a[0] as i64 - b[0] as i64).unsigned_abs();
    }
    let avg_diff = diff_sum as f64 / (actual_top.pixels().len() as f64);
    assert!(avg_diff < 20.0, "avg_diff={avg_diff}");
}

/// Reproduces the reported bug: scroll down, scroll back up to revisit an
/// already-captured area, then scroll back down past it. The revisit must
/// not duplicate content already in the image, and resuming downward must
/// only append the genuinely new tail, not re-add the revisited region.
#[test]
fn stitcher_does_not_duplicate_content_on_revisit() {
    let canvas = make_scroll_canvas(320, 1000);
    let frame1 = crop_frame(&canvas, 0, 320); // rows 0-320
    let frame2 = crop_frame(&canvas, 96, 320); // rows 96-416 (scrolled down)
    let frame3 = crop_frame(&canvas, 192, 320); // rows 192-512 (scrolled down again)
    let frame4 = crop_frame(&canvas, 96, 320); // rows 96-416 again (scrolled back UP - a revisit)
    let frame5 = crop_frame(&canvas, 300, 320); // rows 300-620 (scrolled back down, past the old bottom)

    let mut stitcher = Stitcher::new(MatchConfig {
        min_overlap: 120,
        accept_diff: 3.5,
        min_append: 10,
        approx_diff: 1.0,
        algorithm: Algorithm::OpenCvOrb,
        match_width: 320,
    });

    assert!(matches!(
        stitcher.push_frame(frame1),
        StitchOutcome::FirstFrame
    ));
    assert!(matches!(
        stitcher.push_frame(frame2),
        StitchOutcome::Appended { .. }
    ));
    assert!(matches!(
        stitcher.push_frame(frame3),
        StitchOutcome::Appended { .. }
    ));

    let height_before_revisit = stitcher.full_image().expect("full image").height();

    // Scrolling back up to ground that's already captured must not grow the
    // image at all - this is the bug: it used to re-prepend a duplicate.
    match stitcher.push_frame(frame4) {
        StitchOutcome::NoProgress => {}
        other => panic!("expected no growth on revisit, got {other:?}"),
    }
    let height_after_revisit = stitcher.full_image().expect("full image").height();
    assert_eq!(
        height_before_revisit, height_after_revisit,
        "revisiting an already-captured area must not change the image height"
    );

    // Scrolling back down past the previous bottom should append only the
    // newly revealed tail (roughly 108px), not re-add the revisited region.
    match stitcher.push_frame(frame5) {
        StitchOutcome::Appended { added } => assert!((96..=120).contains(&added), "{added}"),
        other => panic!("expected appended for the genuinely new tail, got {other:?}"),
    }
    let final_height = stitcher.full_image().expect("full image").height();
    assert!(
        final_height < height_after_revisit + 320,
        "final height {final_height} suggests frame5 was fully re-added instead of just its new tail"
    );
}
