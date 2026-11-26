//! `<screenshot> --edit`: a minimal doodle + crop editor.
//!
//! Controls:
//! - Left click + drag: doodle (solid filled brush strokes) in doodle mode,
//!   or drag out a crop rectangle in crop mode.
//! - Up / Down: increase / decrease the doodle brush size.
//! - Left / Right: cycle through the color palette.
//! - `c`: toggle crop mode.
//! - Enter: apply the pending crop rectangle.
//! - Escape: cancel a pending crop (or quit, if nothing is pending).
//! - `Ctrl+S`: save (overwrites the original file) and keep editing.
//! - `Ctrl+C`: copy the current image to the clipboard and keep editing.
//! - `q`: quit. Anything already written via Ctrl+S stays on disk; anything
//!   only in the in-memory canvas since the last Ctrl+S is discarded.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use image::RgbaImage;
use rayon::prelude::*;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use tiny_skia::{Color, Paint, PathBuilder, Pixmap, Rect as SkiaRect, Stroke, Transform};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

/// Doodle color palette. Index 0 (black) is the default.
const PALETTE: [[u8; 3]; 8] = [
    [0x00, 0x00, 0x00], // Black (default)
    [0xDF, 0xD8, 0xCF],
    [0xE6, 0xE6, 0xFA], // Lavender
    [0xFF, 0x00, 0x00], // Red
    [0x53, 0x34, 0x16],
    [0x60, 0x79, 0x38],
    [0xEE, 0xDF, 0xC7],
    [0xCE, 0x7F, 0x20],
];

const DEFAULT_BRUSH_RADIUS: f32 = 6.0;
const MIN_BRUSH_RADIUS: f32 = 2.0;
const MAX_BRUSH_RADIUS: f32 = 60.0;
const BRUSH_STEP: f32 = 2.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Doodle,
    Crop,
}

/// Entry point for `<screenshot> --edit`.
pub fn run(path: &Path) -> Result<()> {
    let canvas = image::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?
        .to_rgba8();

    let max_output = crate::overlay::probe_output_rects()
        .ok()
        .and_then(|rects| rects.into_iter().next())
        .map(|rect| (rect.width.max(1) as u32, rect.height.max(1) as u32))
        .unwrap_or((1920, 1080));

    let conn = Connection::connect_to_env().context("failed to connect to Wayland")?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).context("failed to init Wayland registry")?;
    let qh = event_queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("layer-shell not available")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("Wayutils-Screen-editor"),
        None,
    );
    // No anchor on either axis: wlr-layer-shell centers the surface on the
    // output for us, so we don't need to compute margins by hand.
    layer.set_anchor(Anchor::empty());
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_exclusive_zone(-1);

    let (display_w, display_h, scale) = fit_to_output(canvas.width(), canvas.height(), max_output);
    layer.set_size(display_w, display_h);
    layer.commit();

    let pool = SlotPool::new((display_w * display_h * 4).max(4) as usize, &shm)
        .context("failed to create shm pool")?;

    let display_buffer = build_display_buffer(&canvas, display_w, display_h, scale);

    let mut state = EditorState {
        registry_state: RegistryState::new(&globals),
        seat_state,
        output_state,
        shm,
        pool,
        layer,
        display_w,
        display_h,
        max_output,
        configured: false,
        exit: false,
        path: path.to_path_buf(),
        canvas,
        scale,
        display_buffer,
        mode: Mode::Doodle,
        brush_radius: DEFAULT_BRUSH_RADIUS,
        color_index: 0,
        keyboard: None,
        pointer: None,
        keyboard_focus: false,
        ctrl_held: false,
        pointer_down: false,
        last_canvas_point: None,
        cursor_display_pos: None,
        crop_start: None,
        crop_current: None,
    };

    event_queue
        .roundtrip(&mut state)
        .context("initial roundtrip failed")?;

    eprintln!(
        "Edit mode: drag to doodle | Up/Down brush size | Left/Right color | c crop | \
         Enter confirm crop | Esc cancel crop/quit | Ctrl+S save (keep editing) | \
         Ctrl+C copy to clipboard (keep editing) | q quit"
    );

    loop {
        if state.exit {
            break;
        }
        conn.flush().ok();
        if let Some(guard) = event_queue.prepare_read() {
            let _ = guard.read();
        }
        event_queue
            .dispatch_pending(&mut state)
            .context("failed to process Wayland events")?;
        thread::sleep(Duration::from_millis(8));
    }

    eprintln!("Exited the editor. (Ctrl+S saves immediately while editing; quitting does not save.)");

    Ok(())
}

/// Builds a full display-resolution (BGRA byte order) copy of `canvas`,
/// nearest-neighbour scaled. Parallelized by row with rayon since this is
/// O(display pixels) - only used for the rare full rebuilds (initial load,
/// after a crop), never on every mouse move.
fn build_display_buffer(canvas: &RgbaImage, dst_w: u32, dst_h: u32, scale: f32) -> Vec<u8> {
    let mut buf = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    let cw = canvas.width();
    let ch = canvas.height();
    if cw == 0 || ch == 0 || dst_w == 0 || dst_h == 0 {
        return buf;
    }
    let inv_scale = 1.0 / scale.max(0.0001);
    let src = canvas.as_raw();
    let stride = (dst_w as usize) * 4;

    buf.par_chunks_mut(stride).enumerate().for_each(|(y, row)| {
        let sy = ((y as f32 * inv_scale) as u32).min(ch - 1);
        let src_row = (sy * cw * 4) as usize;
        for x in 0..dst_w as usize {
            let sx = ((x as f32 * inv_scale) as u32).min(cw - 1) as usize;
            let src_idx = src_row + sx * 4;
            let dst_idx = x * 4;
            if src_idx + 3 < src.len() && dst_idx + 3 < row.len() {
                row[dst_idx] = src[src_idx + 2]; // B
                row[dst_idx + 1] = src[src_idx + 1]; // G
                row[dst_idx + 2] = src[src_idx]; // R
                row[dst_idx + 3] = src[src_idx + 3]; // A
            }
        }
    });

    buf
}

/// Returns `(display_width, display_height, scale)` so the image fits inside
/// `max_output`, never upscaled beyond 1:1.
fn fit_to_output(img_w: u32, img_h: u32, max_output: (u32, u32)) -> (u32, u32, f32) {
    let scale = (max_output.0 as f32 / img_w.max(1) as f32)
        .min(max_output.1 as f32 / img_h.max(1) as f32)
        .min(1.0)
        .max(0.01);
    let display_w = ((img_w as f32) * scale).round().max(1.0) as u32;
    let display_h = ((img_h as f32) * scale).round().max(1.0) as u32;
    (display_w, display_h, scale)
}

struct EditorState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,

    display_w: u32,
    display_h: u32,
    max_output: (u32, u32),
    configured: bool,
    exit: bool,

    /// The file this editor was opened on; `Ctrl+S` overwrites this path.
    path: PathBuf,

    /// The actual edited image, at full (post-crop) resolution. Doodles are
    /// painted directly into this buffer.
    canvas: RgbaImage,
    /// display_px = canvas_px * scale.
    scale: f32,

    /// A persistent, pre-scaled (display-resolution, BGRA byte order) copy
    /// of `canvas`. Rebuilding this for the *whole* image on every mouse
    /// move is what made doodling laggy; instead we rebuild it once (on
    /// load/crop) and then only patch the small rectangle that a stroke
    /// actually touched, so each redraw costs O(brush size), not O(image
    /// size). `draw()` does a plain bulk copy of this buffer into the
    /// wl_shm buffer and only overlays the cursor ring / crop rect on top.
    display_buffer: Vec<u8>,

    mode: Mode,
    brush_radius: f32,
    color_index: usize,

    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard_focus: bool,
    /// Tracks whether Ctrl is currently held, so Ctrl+S/Ctrl+C can be told
    /// apart from the plain `s`/`c` letter keys.
    ctrl_held: bool,

    pointer_down: bool,
    last_canvas_point: Option<(f32, f32)>,
    cursor_display_pos: Option<(f64, f64)>,

    /// Pending crop rectangle corners, in canvas coordinates.
    crop_start: Option<(f32, f32)>,
    crop_current: Option<(f32, f32)>,
}

impl EditorState {
    fn current_color(&self) -> [u8; 4] {
        let c = PALETTE[self.color_index % PALETTE.len()];
        [c[0], c[1], c[2], 255]
    }

    fn to_canvas_point(&self, position: (f64, f64)) -> (f32, f32) {
        let scale = self.scale.max(0.0001);
        (
            (position.0 as f32 / scale).clamp(0.0, self.canvas.width().max(1) as f32 - 1.0),
            (position.1 as f32 / scale).clamp(0.0, self.canvas.height().max(1) as f32 - 1.0),
        )
    }

    fn paint_dot(&mut self, pt: (f32, f32)) {
        let color = self.current_color();
        let r = self.brush_radius;
        draw_filled_circle(&mut self.canvas, pt.0, pt.1, r, color);
        self.mark_dirty_canvas_rect(pt.0 - r, pt.1 - r, pt.0 + r, pt.1 + r);
    }

    /// Draws a continuous stroke from the last recorded point to `pt`,
    /// stamping circles along the way so fast mouse movement still leaves an
    /// unbroken line (same sensitivity as the raw cursor movement). Only the
    /// bounding box actually touched by this segment is patched into the
    /// display buffer - the rest of the image isn't re-scanned at all.
    fn paint_stroke_to(&mut self, pt: (f32, f32)) {
        let Some(last) = self.last_canvas_point else {
            self.paint_dot(pt);
            self.last_canvas_point = Some(pt);
            return;
        };
        let dx = pt.0 - last.0;
        let dy = pt.1 - last.1;
        let dist = (dx * dx + dy * dy).sqrt();
        let step = (self.brush_radius * 0.5).max(1.0);
        let steps = (dist / step).ceil().max(1.0) as u32;
        let color = self.current_color();
        let r = self.brush_radius;
        let min_x = last.0.min(pt.0) - r;
        let min_y = last.1.min(pt.1) - r;
        let max_x = last.0.max(pt.0) + r;
        let max_y = last.1.max(pt.1) + r;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let x = last.0 + dx * t;
            let y = last.1 + dy * t;
            draw_filled_circle(&mut self.canvas, x, y, r, color);
        }
        self.last_canvas_point = Some(pt);
        self.mark_dirty_canvas_rect(min_x, min_y, max_x, max_y);
    }

    /// Re-scales just the given canvas-space rectangle into `display_buffer`,
    /// instead of rebuilding the whole scaled image.
    fn mark_dirty_canvas_rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        let scale = self.scale.max(0.0001);
        let dx0 = (x0 * scale).floor().max(0.0) as u32;
        let dy0 = (y0 * scale).floor().max(0.0) as u32;
        let dx1 = ((x1 * scale).ceil().max(0.0) as u32)
            .saturating_add(1)
            .min(self.display_w);
        let dy1 = ((y1 * scale).ceil().max(0.0) as u32)
            .saturating_add(1)
            .min(self.display_h);
        if dx1 <= dx0 || dy1 <= dy0 {
            return;
        }

        let cw = self.canvas.width();
        let ch = self.canvas.height();
        if cw == 0 || ch == 0 {
            return;
        }
        let inv_scale = 1.0 / scale;
        let dst_w = self.display_w;
        let src = self.canvas.as_raw();
        for y in dy0..dy1 {
            let sy = ((y as f32 * inv_scale) as u32).min(ch - 1);
            let dst_row = (y as usize) * (dst_w as usize) * 4;
            let src_row = (sy * cw * 4) as usize;
            for x in dx0..dx1 {
                let sx = ((x as f32 * inv_scale) as u32).min(cw - 1);
                let src_idx = src_row + (sx * 4) as usize;
                let dst_idx = dst_row + (x as usize) * 4;
                if src_idx + 3 < src.len() && dst_idx + 3 < self.display_buffer.len() {
                    self.display_buffer[dst_idx] = src[src_idx + 2];
                    self.display_buffer[dst_idx + 1] = src[src_idx + 1];
                    self.display_buffer[dst_idx + 2] = src[src_idx];
                    self.display_buffer[dst_idx + 3] = src[src_idx + 3];
                }
            }
        }
    }

    fn apply_pending_crop(&mut self, qh: &QueueHandle<Self>) {
        let (Some(a), Some(b)) = (self.crop_start, self.crop_current) else {
            return;
        };
        let x0 = a.0.min(b.0).max(0.0) as u32;
        let y0 = a.1.min(b.1).max(0.0) as u32;
        let x1 = (a.0.max(b.0)).min(self.canvas.width() as f32) as u32;
        let y1 = (a.1.max(b.1)).min(self.canvas.height() as f32) as u32;
        self.crop_start = None;
        self.crop_current = None;
        if x1 <= x0 || y1 <= y0 {
            self.request_redraw(qh);
            return;
        }
        let cropped = image::imageops::crop_imm(&self.canvas, x0, y0, x1 - x0, y1 - y0).to_image();
        self.canvas = cropped;
        self.mode = Mode::Doodle;
        self.recompute_layout(qh);
    }

    /// `Ctrl+S`: overwrites the original file with the current canvas,
    /// without exiting the editor so doodling/cropping can continue.
    fn quick_save(&mut self) {
        let format = crate::output::format_for_path(&self.path);
        match self.canvas.save_with_format(&self.path, format) {
            Ok(()) => eprintln!("Saved to {}", self.path.display()),
            Err(err) => eprintln!("Failed to save {}: {err}", self.path.display()),
        }
    }

    /// `Ctrl+C`: copies the current canvas to the clipboard, without exiting
    /// the editor so doodling/cropping can continue.
    fn quick_copy(&mut self) {
        match crate::output::copy_to_clipboard(Arc::new(self.canvas.clone())) {
            Ok(()) => eprintln!("Copied to clipboard"),
            Err(err) => eprintln!("Failed to copy to clipboard: {err}"),
        }
    }

    fn recompute_layout(&mut self, qh: &QueueHandle<Self>) {
        let (display_w, display_h, scale) =
            fit_to_output(self.canvas.width(), self.canvas.height(), self.max_output);
        self.display_w = display_w;
        self.display_h = display_h;
        self.scale = scale;
        // The canvas size changed (crop), so the whole display buffer needs
        // rebuilding - this happens once per crop, not per mouse move.
        self.display_buffer = build_display_buffer(&self.canvas, display_w, display_h, scale);
        self.layer.set_size(display_w, display_h);
        self.layer.commit();
        self.request_redraw(qh);
    }

    fn request_redraw(&mut self, qh: &QueueHandle<Self>) {
        if self.configured {
            self.draw(qh);
        } else {
            self.layer.commit();
        }
    }

    fn draw(&mut self, _qh: &QueueHandle<Self>) {
        if self.display_w == 0 || self.display_h == 0 {
            return;
        }

        let needed = (self.display_w * self.display_h * 4) as usize;
        if self.pool.len() < needed {
            if let Err(err) = self.pool.resize(needed) {
                log::warn!("editor pool resize failed: {err}");
                return;
            }
        }

        let stride = self.display_w as i32 * 4;
        let (buffer, canvas_buf) = match self.pool.create_buffer(
            self.display_w as i32,
            self.display_h as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) {
            Ok(result) => result,
            Err(err) => {
                log::warn!("editor buffer create failed: {err}");
                return;
            }
        };

        // Cheap bulk copy of the already-scaled buffer (kept up to date
        // incrementally by paint_dot/paint_stroke_to/recompute_layout)
        // instead of re-scanning the whole source image every redraw.
        if canvas_buf.len() == self.display_buffer.len() {
            canvas_buf.copy_from_slice(&self.display_buffer);
        } else {
            log::warn!("editor display buffer size mismatch; redraw skipped this frame");
            canvas_buf.fill(0);
        }

        match self.mode {
            Mode::Doodle => {
                if let Some(pos) = self.cursor_display_pos {
                    draw_brush_ring(
                        canvas_buf,
                        self.display_w,
                        self.display_h,
                        pos,
                        self.brush_radius * self.scale,
                    );
                }
            }
            Mode::Crop => {
                if let (Some(a), Some(b)) = (self.crop_start, self.crop_current) {
                    draw_crop_rect(canvas_buf, self.display_w, self.display_h, a, b, self.scale);
                }
            }
        }

        self.layer
            .wl_surface()
            .damage_buffer(0, 0, self.display_w as i32, self.display_h as i32);
        if let Err(err) = buffer.attach_to(self.layer.wl_surface()) {
            log::warn!("editor buffer attach failed: {err}");
            return;
        }
        self.layer.commit();
    }
}

/// Paints a solid (non-anti-aliased) filled circle directly into the image.
fn draw_filled_circle(img: &mut RgbaImage, cx: f32, cy: f32, radius: f32, color: [u8; 4]) {
    let r = radius.max(1.0);
    let r2 = r * r;
    if img.width() == 0 || img.height() == 0 {
        return;
    }
    let min_x = (cx - r).floor().max(0.0) as i64;
    let max_x = (cx + r).ceil().min(img.width() as f32 - 1.0) as i64;
    let min_y = (cy - r).floor().max(0.0) as i64;
    let max_y = (cy + r).ceil().min(img.height() as f32 - 1.0) as i64;
    if min_x > max_x || min_y > max_y {
        return;
    }
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            if dx * dx + dy * dy <= r2 {
                img.put_pixel(x as u32, y as u32, image::Rgba(color));
            }
        }
    }
}

/// Alpha-composites a (premultiplied) tiny-skia pixmap over the wl_shm
/// buffer at `(origin_x, origin_y)`.
fn composite_pixmap(dst: &mut [u8], dst_w: u32, dst_h: u32, src: &Pixmap, origin_x: f64, origin_y: f64) {
    let src_data = src.data();
    let sw = src.width();
    let sh = src.height();
    for sy in 0..sh {
        let dy = origin_y.round() as i64 + sy as i64;
        if dy < 0 || dy >= dst_h as i64 {
            continue;
        }
        for sx in 0..sw {
            let dx = origin_x.round() as i64 + sx as i64;
            if dx < 0 || dx >= dst_w as i64 {
                continue;
            }
            let sidx = ((sy * sw + sx) * 4) as usize;
            let a = src_data[sidx + 3];
            if a == 0 {
                continue;
            }
            let didx = ((dy as u32 * dst_w + dx as u32) * 4) as usize;
            if didx + 3 >= dst.len() {
                continue;
            }
            let (sr, sg, sb) = (src_data[sidx], src_data[sidx + 1], src_data[sidx + 2]);
            let (db, dg, dr) = (dst[didx], dst[didx + 1], dst[didx + 2]);
            let inv_af = 1.0 - (a as f32 / 255.0);
            // src_data is premultiplied, so the "over" composite is a plain sum.
            dst[didx] = (sb as f32 + db as f32 * inv_af).round().min(255.0) as u8;
            dst[didx + 1] = (sg as f32 + dg as f32 * inv_af).round().min(255.0) as u8;
            dst[didx + 2] = (sr as f32 + dr as f32 * inv_af).round().min(255.0) as u8;
            dst[didx + 3] = 255;
        }
    }
}

/// Draws the doodle cursor: a hollow circle (no fill) with a thin white
/// border whose thickness grows with the brush size, matching the radius
/// the next stroke will paint at.
fn draw_brush_ring(dst: &mut [u8], width: u32, height: u32, center: (f64, f64), display_radius: f32) {
    let r = display_radius.max(2.0);
    let border = ((r / 5.0) + 1.0).clamp(1.0, 6.0);
    let pad = r + border + 2.0;
    let size = (pad * 2.0).ceil().max(1.0) as u32;
    let Some(mut pixmap) = Pixmap::new(size, size) else {
        return;
    };

    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(255, 255, 255, 235));
    paint.anti_alias = true;

    let mut pb = PathBuilder::new();
    pb.push_circle(pad, pad, r);
    if let Some(path) = pb.finish() {
        let stroke = Stroke {
            width: border,
            ..Default::default()
        };
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    let origin_x = center.0 - pad as f64;
    let origin_y = center.1 - pad as f64;
    composite_pixmap(dst, width, height, &pixmap, origin_x, origin_y);
}

/// Draws the pending crop rectangle outline, in canvas coordinates mapped to
/// display coordinates via `scale`. Only allocates a pixmap sized to the
/// rectangle's own bounding box (plus a little stroke padding), not the
/// whole display surface, so dragging a crop box stays cheap too.
fn draw_crop_rect(dst: &mut [u8], width: u32, height: u32, a: (f32, f32), b: (f32, f32), scale: f32) {
    if width == 0 || height == 0 {
        return;
    }

    let stroke_w = 2.0f32;
    let pad = stroke_w + 2.0;
    let x0 = (a.0.min(b.0) * scale).max(0.0);
    let y0 = (a.1.min(b.1) * scale).max(0.0);
    let x1 = (a.0.max(b.0) * scale).min(width as f32);
    let y1 = (a.1.max(b.1) * scale).min(height as f32);

    let px0 = (x0 - pad).max(0.0);
    let py0 = (y0 - pad).max(0.0);
    let px1 = (x1 + pad).min(width as f32);
    let py1 = (y1 + pad).min(height as f32);
    let pw = (px1 - px0).ceil().max(1.0) as u32;
    let ph = (py1 - py0).ceil().max(1.0) as u32;

    let Some(mut pixmap) = Pixmap::new(pw, ph) else {
        return;
    };

    // Rectangle corners translated into the small pixmap's local space.
    if let Some(rect) = SkiaRect::from_ltrb(x0 - px0, y0 - py0, x1 - px0, y1 - py0) {
        let mut pb = PathBuilder::new();
        pb.push_rect(rect);
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(Color::from_rgba8(255, 255, 255, 230));
            paint.anti_alias = true;
            let stroke = Stroke {
                width: stroke_w,
                ..Default::default()
            };
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }

    composite_pixmap(dst, width, height, &pixmap, px0 as f64, py0 as f64);
}

impl CompositorHandler for EditorState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for EditorState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {}

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for EditorState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let assigned_size = if configure.new_size.0 != 0 && configure.new_size.1 != 0 {
            (configure.new_size.0, configure.new_size.1)
        } else {
            (self.display_w, self.display_h)
        };

        // Some compositors configure an unanchored overlay surface to a size
        // other than the one we requested (or, on the very first configure,
        // may not yet match the size `run()` computed before any roundtrip).
        // `display_buffer` is a cache pre-scaled to a *specific* display
        // size, so if we only updated `display_w`/`display_h` here without
        // rebuilding it, `draw()` would find a length mismatch, log a
        // warning and blank the surface - which is exactly why the
        // screenshot wouldn't show up. Re-fit and rebuild the cache for
        // whatever size the compositor actually assigned.
        if assigned_size != (self.display_w, self.display_h) || !self.configured {
            let (display_w, display_h, scale) =
                fit_to_output(self.canvas.width(), self.canvas.height(), assigned_size);
            self.display_w = display_w;
            self.display_h = display_h;
            self.scale = scale;
            self.display_buffer = build_display_buffer(&self.canvas, display_w, display_h, scale);
            // Tell the compositor our actual (aspect-correct) size back, in
            // case it differs from what it just assigned us.
            self.layer.set_size(display_w, display_h);
        }

        self.configured = true;
        self.draw(qh);
    }
}

impl SeatHandler for EditorState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            if let Ok(keyboard) = self.seat_state.get_keyboard(qh, &seat, None) {
                self.keyboard = Some(keyboard);
            }
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            if let Ok(pointer) = self.seat_state.get_pointer(qh, &seat) {
                self.pointer = Some(pointer);
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for EditorState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        if self.layer.wl_surface() == surface {
            self.keyboard_focus = true;
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if self.layer.wl_surface() == surface {
            self.keyboard_focus = false;
        }
    }

    fn press_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if !self.keyboard_focus {
            return;
        }

        let keysym = event.keysym;
        if keysym == Keysym::Escape {
            if self.crop_start.is_some() || self.crop_current.is_some() {
                self.crop_start = None;
                self.crop_current = None;
                self.request_redraw(qh);
            } else {
                self.exit = true;
            }
            return;
        } else if keysym == Keysym::Return {
            self.apply_pending_crop(qh);
            return;
        } else if keysym == Keysym::Up {
            self.brush_radius = (self.brush_radius + BRUSH_STEP).min(MAX_BRUSH_RADIUS);
            self.request_redraw(qh);
            return;
        } else if keysym == Keysym::Down {
            self.brush_radius = (self.brush_radius - BRUSH_STEP).max(MIN_BRUSH_RADIUS);
            self.request_redraw(qh);
            return;
        } else if keysym == Keysym::Left {
            self.color_index = (self.color_index + PALETTE.len() - 1) % PALETTE.len();
            self.request_redraw(qh);
            return;
        } else if keysym == Keysym::Right {
            self.color_index = (self.color_index + 1) % PALETTE.len();
            self.request_redraw(qh);
            return;
        }

        // Keysyms for letter keys are unaffected by the Ctrl modifier in xkb
        // (only Shift changes which letter keysym is reported), so matching
        // on the keysym plus `self.ctrl_held` reliably tells Ctrl+C/Ctrl+S
        // apart from the plain `c`/`s` keys, regardless of what
        // `event.utf8` happens to contain for a control chord.
        let is_c = matches!(keysym, Keysym::c | Keysym::C);
        let is_s = matches!(keysym, Keysym::s | Keysym::S);
        let is_q = matches!(keysym, Keysym::q | Keysym::Q);

        if self.ctrl_held && is_s {
            self.quick_save();
        } else if self.ctrl_held && is_c {
            self.quick_copy();
        } else if is_c {
            self.mode = match self.mode {
                Mode::Doodle => Mode::Crop,
                Mode::Crop => Mode::Doodle,
            };
            self.crop_start = None;
            self.crop_current = None;
            self.request_redraw(qh);
        } else if is_q {
            self.exit = true;
        }
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: u32,
    ) {
        self.ctrl_held = modifiers.ctrl;
    }
}

impl PointerHandler for EditorState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            match event.kind {
                PointerEventKind::Press { .. } => {
                    self.pointer_down = true;
                    let canvas_pt = self.to_canvas_point(event.position);
                    match self.mode {
                        Mode::Doodle => {
                            self.paint_dot(canvas_pt);
                            self.last_canvas_point = Some(canvas_pt);
                        }
                        Mode::Crop => {
                            self.crop_start = Some(canvas_pt);
                            self.crop_current = Some(canvas_pt);
                        }
                    }
                    self.request_redraw(qh);
                }
                PointerEventKind::Release { .. } => {
                    self.pointer_down = false;
                    self.last_canvas_point = None;
                }
                PointerEventKind::Motion { .. } => {
                    self.cursor_display_pos = Some(event.position);
                    if self.pointer_down {
                        let canvas_pt = self.to_canvas_point(event.position);
                        match self.mode {
                            Mode::Doodle => self.paint_stroke_to(canvas_pt),
                            Mode::Crop => self.crop_current = Some(canvas_pt),
                        }
                    }
                    self.request_redraw(qh);
                }
                PointerEventKind::Leave { .. } => {
                    self.cursor_display_pos = None;
                    self.request_redraw(qh);
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for EditorState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(EditorState);
delegate_output!(EditorState);
delegate_shm!(EditorState);
delegate_seat!(EditorState);
delegate_keyboard!(EditorState);
delegate_pointer!(EditorState);
delegate_layer!(EditorState);
delegate_registry!(EditorState);

impl ProvidesRegistryState for EditorState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers!(OutputState, SeatState);
}
