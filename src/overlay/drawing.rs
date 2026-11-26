use std::sync::OnceLock;

use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Transform};

use crate::constants::CONTROL_BAR_HEIGHT;
use crate::types::PreviewImage;

const CONTROL_BUTTON_COUNT: u32 = 4;
const ICON_SIZE: u32 = 24;
const COLOR_BUTTON: [u8; 4] = [228, 226, 255, 244];
const COLOR_BUTTON_HOVER: [u8; 4] = [250, 249, 255, 255];

struct EmbeddedIcon {
    size: u32,
    /// Premultiplied RGBA pixels, ready for compositing into a tiny-skia pixmap.
    pixels: Vec<u8>,
}

/// The SVG artwork is converted to PNG at 128px in `assets/icons/` and embedded
/// at compile time. The decoded 24px bitmaps are cached for the lifetime of the
/// process so drawing the preview does not repeatedly decode image data.
fn toolbar_icons() -> &'static [Option<EmbeddedIcon>; 4] {
    static ICONS: OnceLock<[Option<EmbeddedIcon>; 4]> = OnceLock::new();
    ICONS.get_or_init(|| [
        load_embedded_icon(include_bytes!("../../assets/icons/save.png")),
        load_embedded_icon(include_bytes!("../../assets/icons/layers.png")),
        load_embedded_icon(include_bytes!("../../assets/icons/orb.png")),
        load_embedded_icon(include_bytes!("../../assets/icons/logout.png")),
    ])
}

fn load_embedded_icon(bytes: &[u8]) -> Option<EmbeddedIcon> {
    let decoded = image::load_from_memory(bytes).ok()?.to_rgba8();
    let resized = image::imageops::resize(
        &decoded,
        ICON_SIZE,
        ICON_SIZE,
        image::imageops::FilterType::Lanczos3,
    );
    let mut pixels = resized.into_raw();

    // tiny-skia pixmaps use premultiplied alpha. Convert the decoded PNG's
    // straight-alpha channels once, when the cached icon is initialized.
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = pixel[3] as u16;
        pixel[0] = ((pixel[0] as u16 * alpha + 127) / 255) as u8;
        pixel[1] = ((pixel[1] as u16 * alpha + 127) / 255) as u8;
        pixel[2] = ((pixel[2] as u16 * alpha + 127) / 255) as u8;
    }

    Some(EmbeddedIcon {
        size: ICON_SIZE,
        pixels,
    })
}

/// Draws the control bar (Save/Copy/Pause/Cancel) at the bottom.
pub(super) fn draw_control_bar(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    bar_y: u32,
    _paused: bool,
    hover_button: Option<u32>,
) {
    let bar_height = CONTROL_BAR_HEIGHT.min(height.saturating_sub(bar_y));
    if width == 0 || bar_height == 0 {
        return;
    }

    let mut pixmap = match Pixmap::new(width, bar_height) {
        Some(p) => p,
        None => return,
    };

    let segment = (width / CONTROL_BUTTON_COUNT).max(1);
    let padding = 3.0;
    let radius = 8.0;
    let icons = toolbar_icons();

    for index in 0..CONTROL_BUTTON_COUNT {
        let start_x = index * segment;
        let end_x = if index == CONTROL_BUTTON_COUNT - 1 {
            width
        } else {
            (index + 1) * segment
        };
        let button_width = end_x.saturating_sub(start_x);
        if button_width == 0 {
            continue;
        }

        let color = if hover_button == Some(index) {
            COLOR_BUTTON_HOVER
        } else {
            COLOR_BUTTON
        };

        let x = start_x as f32 + padding;
        let y = padding;
        let w = button_width as f32 - padding * 2.0;
        let h = bar_height as f32 - padding * 2.0;

        if let Some(path) = rounded_rect_path(x, y, w, h, radius) {
            let mut paint = Paint::default();
            paint.set_color(Color::from_rgba8(color[0], color[1], color[2], color[3]));
            paint.anti_alias = true;
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }

        if let Some(Some(icon)) = icons.get(index as usize) {
            let icon_x = start_x + button_width.saturating_sub(icon.size) / 2;
            let icon_y = bar_height.saturating_sub(icon.size) / 2;
            draw_icon_to_pixmap(&mut pixmap, icon, icon_x, icon_y);
        }
    }

    let pixmap_data = pixmap.data();
    for y in 0..bar_height {
        let dst_y = bar_y + y;
        if dst_y >= height {
            break;
        }
        let src_row = (y * width * 4) as usize;
        let dst_row = (dst_y * width * 4) as usize;
        for x in 0..width {
            let src_idx = src_row + (x * 4) as usize;
            let dst_idx = dst_row + (x * 4) as usize;
            if src_idx + 3 < pixmap_data.len() && dst_idx + 3 < canvas.len() {
                let a = pixmap_data[src_idx + 3];
                if a > 0 {
                    canvas[dst_idx] = pixmap_data[src_idx + 2];
                    canvas[dst_idx + 1] = pixmap_data[src_idx + 1];
                    canvas[dst_idx + 2] = pixmap_data[src_idx];
                    canvas[dst_idx + 3] = a;
                }
            }
        }
    }
}

/// Alpha-composites an embedded premultiplied RGBA icon over the toolbar.
fn draw_icon_to_pixmap(pixmap: &mut Pixmap, icon: &EmbeddedIcon, x: u32, y: u32) {
    let width = pixmap.width();
    let height = pixmap.height();
    let data = pixmap.data_mut();

    for sy in 0..icon.size {
        let dy = y + sy;
        if dy >= height {
            continue;
        }
        for sx in 0..icon.size {
            let dx = x + sx;
            if dx >= width {
                continue;
            }

            let src_idx = ((sy * icon.size + sx) * 4) as usize;
            let dst_idx = ((dy * width + dx) * 4) as usize;
            if src_idx + 3 >= icon.pixels.len() || dst_idx + 3 >= data.len() {
                continue;
            }

            let alpha = icon.pixels[src_idx + 3] as u32;
            if alpha == 0 {
                continue;
            }
            let inverse_alpha = 255 - alpha;
            for channel in 0..3 {
                let src = icon.pixels[src_idx + channel] as u32;
                let dst = data[dst_idx + channel] as u32;
                data[dst_idx + channel] =
                    (src + (dst * inverse_alpha + 127) / 255).min(255) as u8;
            }
            let dst_alpha = data[dst_idx + 3] as u32;
            data[dst_idx + 3] =
                (alpha + (dst_alpha * inverse_alpha + 127) / 255).min(255) as u8;
        }
    }
}

fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = r.min(w / 2.0).min(h / 2.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

pub(super) fn blit_preview_bottom(
    canvas: &mut [u8],
    canvas_width: u32,
    available_height: u32,
    preview: &PreviewImage,
) {
    let bytes_per_row = canvas_width.saturating_mul(4) as usize;
    if bytes_per_row == 0 || available_height == 0 {
        return;
    }

    let max_cols = preview.width.min(canvas_width);

    let (src_start_y, display_height) = if preview.height <= available_height {
        (0, preview.height)
    } else {
        (preview.height - available_height, available_height)
    };

    for y in 0..display_height {
        let src_y = src_start_y + y;
        let src_row = (src_y * preview.width * 4) as usize;
        let dst_row = (y * canvas_width * 4) as usize;
        for x in 0..max_cols {
            let src_idx = src_row + (x * 4) as usize;
            let dst_idx = dst_row + (x * 4) as usize;
            if src_idx + 3 < preview.pixels.len() && dst_idx + 3 < canvas.len() {
                canvas[dst_idx] = preview.pixels[src_idx + 2];
                canvas[dst_idx + 1] = preview.pixels[src_idx + 1];
                canvas[dst_idx + 2] = preview.pixels[src_idx];
                canvas[dst_idx + 3] = preview.pixels[src_idx + 3];
            }
        }
    }
}

#[allow(dead_code)]
fn blit_preview(canvas: &mut [u8], canvas_width: u32, preview: &PreviewImage, offset_y: u32) {
    let bytes_per_row = canvas_width.saturating_mul(4) as usize;
    if bytes_per_row == 0 {
        return;
    }
    let canvas_height = (canvas.len() / bytes_per_row) as u32;
    if offset_y >= canvas_height {
        return;
    }

    let max_rows = (canvas_height - offset_y).min(preview.height);
    let max_cols = preview.width.min(canvas_width);

    for y in 0..max_rows {
        let src_row = (y * preview.width * 4) as usize;
        let dst_row = ((offset_y + y) * canvas_width * 4) as usize;
        for x in 0..max_cols {
            let src_idx = src_row + (x * 4) as usize;
            let dst_idx = dst_row + (x * 4) as usize;
            canvas[dst_idx] = preview.pixels[src_idx + 2];
            canvas[dst_idx + 1] = preview.pixels[src_idx + 1];
            canvas[dst_idx + 2] = preview.pixels[src_idx];
            canvas[dst_idx + 3] = preview.pixels[src_idx + 3];
        }
    }
}

fn glyph_rows(ch: char) -> Option<[u8; 7]> {
    match ch {
        'C' => Some([
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ]),
        'P' => Some([
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ]),
        'S' => Some([
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ]),
        'X' => Some([
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ]),
        _ => None,
    }
}
