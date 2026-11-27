//! `<screenshot> --edit`: a minimal doodle + crop editor.
//!
//! Controls:
//! - Left click + drag: doodle (solid filled brush strokes) in doodle mode,
//!   or drag out a crop rectangle in crop mode.
//! - Up / Down: increase / decrease the doodle brush size.
//! - Left / Right: cycle through the color palette.
//! - `c`: toggle crop mode.
//! - `Ctrl+Z`: undo the last doodle/eraser stroke.
//! - `Ctrl+X`: toggle the eraser.
//! - Enter: apply the pending crop rectangle.
//! - Escape: cancel a pending crop (or quit, if nothing is pending).
//! - `Ctrl+S`: save (overwrites the original file) and exit.
//! - `Ctrl+C`: copy the current image to the clipboard and exit.
//! - `q`: quit without saving or copying.

use std::collections::{HashSet, VecDeque};
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
const MAX_UNDO_STROKES: usize = 32;
const MAX_UNDO_PIXELS: usize = 2_000_000;

/// Neutral near-black charcoal backdrop (BGRA byte order) shown around the
/// image while editing. The surface remains translucent so the desktop is
/// dimmed without being completely hidden.
const BACKDROP_BGRA: [u8; 4] = [0x17, 0x17, 0x17, 222];

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
    // Anchored to all four edges, spanning the whole output: the image
    // itself is centered within this by `compute_image_placement`, and the
    // rest is filled with a dim backdrop (see `build_display_buffer`) so the
    // desktop visibly darkens behind the editor instead of the editor
    // quietly floating over an otherwise-unchanged screen.
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_exclusive_zone(-1);

    let (surface_w, surface_h) = max_output;
    layer.set_size(surface_w, surface_h);
    layer.commit();

    let (image_w, image_h, offset_x, offset_y, scale) =
        compute_image_placement(canvas.width(), canvas.height(), surface_w, surface_h);

    let pool = SlotPool::new((surface_w * surface_h * 4).max(4) as usize, &shm)
        .context("failed to create shm pool")?;

    let original_canvas = canvas.clone();
    let display_buffer = build_display_buffer(
        &canvas, surface_w, surface_h, image_w, image_h, offset_x, offset_y, scale,
    );

    let mut state = EditorState {
        registry_state: RegistryState::new(&globals),
        seat_state,
        output_state,
        shm,
        pool,
        layer,
        display_w: surface_w,
        display_h: surface_h,
        image_w,
        image_h,
        offset_x,
        offset_y,
        max_output,
        configured: false,
        exit: false,
        saved_or_copied: false,
        path: path.to_path_buf(),
        original_canvas,
        canvas,
        current_stroke: None,
        undo_history: VecDeque::new(),
        undo_pixels: 0,
        eraser_active: false,
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
        "Edit mode: drag to draw | Ctrl+Z undo | Ctrl+X eraser | Up/Down brush size | \
         Left/Right color | c crop | Enter confirm crop | Esc cancel crop/quit | \
         Ctrl+S save & exit | Ctrl+C copy to clipboard & exit | q quit without saving"
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

    if !state.saved_or_copied {
        eprintln!("Exited without saving.");
    }

    Ok(())
}

/// Builds a full surface-resolution (BGRA byte order) frame: the whole
/// `surface_w`×`surface_h` output filled with [`BACKDROP_BGRA`], with
/// `canvas` nearest-neighbour scaled to `image_w`×`image_h` and blitted in,
/// centered at `(offset_x, offset_y)`. Parallelized by row with rayon since
/// this is O(surface pixels) - only used for the rare full rebuilds (initial
/// load, after a crop, a compositor resize), never on every mouse move.
#[allow(clippy::too_many_arguments)]
fn build_display_buffer(
    canvas: &RgbaImage,
    surface_w: u32,
    surface_h: u32,
    image_w: u32,
    image_h: u32,
    offset_x: u32,
    offset_y: u32,
    scale: f32,
) -> Vec<u8> {
    let mut buf = vec![0u8; (surface_w as usize) * (surface_h as usize) * 4];
    for px in buf.chunks_exact_mut(4) {
        px.copy_from_slice(&BACKDROP_BGRA);
    }

    let cw = canvas.width();
    let ch = canvas.height();
    if cw == 0 || ch == 0 || image_w == 0 || image_h == 0 {
        return buf;
    }
    let inv_scale = 1.0 / scale.max(0.0001);
    let src = canvas.as_raw();
    let surface_stride = (surface_w as usize) * 4;
    let image_bottom = (offset_y + image_h) as usize;
    let image_right = (offset_x + image_w) as usize;

    buf.par_chunks_mut(surface_stride)
        .enumerate()
        .for_each(|(y, row)| {
            if y < offset_y as usize || y >= image_bottom {
                return;
            }
            let iy = y as u32 - offset_y;
            let sy = ((iy as f32 * inv_scale) as u32).min(ch - 1);
            let src_row = (sy * cw * 4) as usize;
            for x in offset_x as usize..image_right.min(surface_w as usize) {
                let ix = (x - offset_x as usize) as u32;
                let sx = ((ix as f32 * inv_scale) as u32).min(cw - 1) as usize;
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

/// Returns `(image_width, image_height, scale)` so the image fits inside
/// `max_output`, never upscaled beyond 1:1.
fn fit_to_output(img_w: u32, img_h: u32, max_output: (u32, u32)) -> (u32, u32, f32) {
    let scale = (max_output.0 as f32 / img_w.max(1) as f32)
        .min(max_output.1 as f32 / img_h.max(1) as f32)
        .min(1.0)
        .max(0.01);
    let image_w = ((img_w as f32) * scale).round().max(1.0) as u32;
    let image_h = ((img_h as f32) * scale).round().max(1.0) as u32;
    (image_w, image_h, scale)
}

/// Fits `canvas_w`×`canvas_h` inside the full `surface_w`×`surface_h`
/// Wayland surface (the whole output) and centers it there, returning
/// `(image_w, image_h, offset_x, offset_y, scale)`. The leftover space
/// around the image is where [`build_display_buffer`] paints the dim
/// backdrop.
fn compute_image_placement(
    canvas_w: u32,
    canvas_h: u32,
    surface_w: u32,
    surface_h: u32,
) -> (u32, u32, u32, u32, f32) {
    let (image_w, image_h, scale) = fit_to_output(canvas_w, canvas_h, (surface_w, surface_h));
    let offset_x = surface_w.saturating_sub(image_w) / 2;
    let offset_y = surface_h.saturating_sub(image_h) / 2;
    (image_w, image_h, offset_x, offset_y, scale)
}

#[derive(Clone, Copy)]
struct PixelChange {
    index: usize,
    previous: [u8; 4],
}

#[derive(Default)]
struct StrokeBuilder {
    changes: Vec<PixelChange>,
    seen: HashSet<usize>,
    exceeded_limit: bool,
}

struct UndoStroke {
    changes: Vec<PixelChange>,
}

struct EditorState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,

    /// The whole Wayland surface (the full output), not just the image -
    /// anchored to all four edges so the dim backdrop covers the real
    /// desktop all the way to its corners.
    display_w: u32,
    display_h: u32,
    /// The scaled image's own size and its centered position within the
    /// surface. Everything outside this rect is [`BACKDROP_BGRA`].
    image_w: u32,
    image_h: u32,
    offset_x: u32,
    offset_y: u32,
    max_output: (u32, u32),
    configured: bool,
    exit: bool,
    /// Set by `quick_save`/`quick_copy` so the final message on exit doesn't
    /// also claim nothing was saved.
    saved_or_copied: bool,

    /// The file this editor was opened on; `Ctrl+S` overwrites this path.
    path: PathBuf,

    /// Original pixels, kept untouched so erasing an annotation reveals the
    /// screenshot instead of damaging it.
    original_canvas: RgbaImage,
    /// Editable output image. Erasing restores pixels from `original_canvas`;
    /// no third full-resolution annotation buffer is allocated.
    canvas: RgbaImage,
    /// Current in-progress stroke and bounded undo history. History stores
    /// only pixels first touched by each stroke, never full-image snapshots.
    current_stroke: Option<StrokeBuilder>,
    undo_history: VecDeque<UndoStroke>,
    undo_pixels: usize,
    eraser_active: bool,
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
        if self.eraser_active {
            return [0, 0, 0, 0];
        }
        let c = PALETTE[self.color_index % PALETTE.len()];
        [c[0], c[1], c[2], 255]
    }

    fn to_canvas_point(&self, position: (f64, f64)) -> (f32, f32) {
        let scale = self.scale.max(0.0001);
        let local_x = position.0 as f32 - self.offset_x as f32;
        let local_y = position.1 as f32 - self.offset_y as f32;
        (
            (local_x / scale).clamp(0.0, self.canvas.width().max(1) as f32 - 1.0),
            (local_y / scale).clamp(0.0, self.canvas.height().max(1) as f32 - 1.0),
        )
    }

    /// Whether a surface-local pointer position falls on the image itself,
    /// rather than the dim backdrop around it.
    fn position_within_image(&self, position: (f64, f64)) -> bool {
        let (x, y) = position;
        x >= self.offset_x as f64
            && y >= self.offset_y as f64
            && x < (self.offset_x + self.image_w) as f64
            && y < (self.offset_y + self.image_h) as f64
    }

    fn begin_stroke(&mut self) {
        self.current_stroke = Some(StrokeBuilder::default());
    }

    fn commit_stroke(&mut self) {
        let Some(stroke) = self.current_stroke.take() else {
            return;
        };
        if stroke.exceeded_limit {
            eprintln!("This stroke exceeded the undo memory limit and cannot be undone.");
            return;
        }
        if stroke.changes.is_empty() {
            return;
        }

        let pixels = stroke.changes.len();
        while self.undo_history.len() >= MAX_UNDO_STROKES
            || self.undo_pixels.saturating_add(pixels) > MAX_UNDO_PIXELS
        {
            let Some(oldest) = self.undo_history.pop_front() else {
                break;
            };
            self.undo_pixels = self.undo_pixels.saturating_sub(oldest.changes.len());
        }
        self.undo_pixels = self.undo_pixels.saturating_add(pixels);
        self.undo_history.push_back(UndoStroke {
            changes: stroke.changes,
        });
    }

    fn undo_last_stroke(&mut self) {
        // A shortcut pressed during a drag ends that pointer stroke first;
        // otherwise later motion events could continue drawing without a
        // fresh history entry after the undo operation.
        self.pointer_down = false;
        self.last_canvas_point = None;
        if let Some(active) = self.current_stroke.as_ref() {
            let exceeded_limit = active.exceeded_limit;
            let has_changes = !active.changes.is_empty();
            self.commit_stroke();
            // Do not unexpectedly undo an older completed stroke when the
            // currently active gesture changed nothing or exceeded its cap.
            if exceeded_limit || !has_changes {
                eprintln!("Nothing to undo for the current stroke.");
                return;
            }
        }
        let Some(stroke) = self.undo_history.pop_back() else {
            eprintln!("Nothing to undo.");
            return;
        };
        self.undo_pixels = self.undo_pixels.saturating_sub(stroke.changes.len());
        if let Some((x0, y0, x1, y1)) = restore_pixel_changes(&mut self.canvas, &stroke.changes) {
            self.mark_dirty_canvas_rect(x0 as f32, y0 as f32, x1 as f32, y1 as f32);
        }
    }

    fn paint_dot(&mut self, pt: (f32, f32)) {
        if self.current_stroke.is_none() {
            self.begin_stroke();
        }
        let color = self.current_color();
        let r = self.brush_radius;
        draw_filled_circle_with_undo(
            &mut self.canvas,
            &self.original_canvas,
            pt.0,
            pt.1,
            r,
            color,
            &mut self.current_stroke,
        );
        self.mark_dirty_canvas_rect(pt.0 - r, pt.1 - r, pt.0 + r, pt.1 + r);
    }

    /// Draws a continuous stroke by stamping small circles. Only its dirty
    /// rectangle is rescaled, keeping pointer-move cost
    /// proportional to the brush/stroke segment rather than the full image.
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
            draw_filled_circle_with_undo(
                &mut self.canvas,
                &self.original_canvas,
                x,
                y,
                r,
                color,
                &mut self.current_stroke,
            );
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
            .min(self.image_w);
        let dy1 = ((y1 * scale).ceil().max(0.0) as u32)
            .saturating_add(1)
            .min(self.image_h);
        if dx1 <= dx0 || dy1 <= dy0 {
            return;
        }

        let cw = self.canvas.width();
        let ch = self.canvas.height();
        if cw == 0 || ch == 0 {
            return;
        }
        let inv_scale = 1.0 / scale;
        let surface_w = self.display_w;
        let (offset_x, offset_y) = (self.offset_x, self.offset_y);
        let src = self.canvas.as_raw();
        for y in dy0..dy1 {
            let sy = ((y as f32 * inv_scale) as u32).min(ch - 1);
            let dst_row = ((y + offset_y) as usize) * (surface_w as usize) * 4;
            let src_row = (sy * cw * 4) as usize;
            for x in dx0..dx1 {
                let sx = ((x as f32 * inv_scale) as u32).min(cw - 1);
                let src_idx = src_row + (sx * 4) as usize;
                let dst_idx = dst_row + ((x + offset_x) as usize) * 4;
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
        self.original_canvas = image::imageops::crop_imm(
            &self.original_canvas, x0, y0, x1 - x0, y1 - y0,
        )
        .to_image();
        self.canvas = cropped;
        self.current_stroke = None;
        self.undo_history.clear();
        self.undo_pixels = 0;
        self.eraser_active = false;
        self.mode = Mode::Doodle;
        self.recompute_layout(qh);
    }

    /// `Ctrl+S`: overwrites the original file with the current canvas, then
    /// exits the editor (there's nothing left to do once it's saved).
    fn quick_save(&mut self) {
        let format = crate::output::format_for_path(&self.path);
        match self.canvas.save_with_format(&self.path, format) {
            Ok(()) => {
                eprintln!("Saved to {}", self.path.display());
                self.saved_or_copied = true;
            }
            Err(err) => eprintln!("Failed to save {}: {err}", self.path.display()),
        }
        self.exit = true;
    }

    /// `Ctrl+C`: copies the current canvas to the clipboard, then exits the
    /// editor (there's nothing left to do once it's copied).
    fn quick_copy(&mut self) {
        match crate::output::copy_to_clipboard(Arc::new(self.canvas.clone())) {
            Ok(()) => {
                eprintln!("Copied to clipboard");
                self.saved_or_copied = true;
            }
            Err(err) => eprintln!("Failed to copy to clipboard: {err}"),
        }
        self.exit = true;
    }

    /// Called after a crop changes the canvas's own size. The Wayland
    /// surface itself stays at the full output size (so the dim backdrop
    /// keeps covering the whole screen) - only the image's placement within
    /// it changes.
    fn recompute_layout(&mut self, qh: &QueueHandle<Self>) {
        let (image_w, image_h, offset_x, offset_y, scale) = compute_image_placement(
            self.canvas.width(),
            self.canvas.height(),
            self.display_w,
            self.display_h,
        );
        self.image_w = image_w;
        self.image_h = image_h;
        self.offset_x = offset_x;
        self.offset_y = offset_y;
        self.scale = scale;
        self.display_buffer = build_display_buffer(
            &self.canvas,
            self.display_w,
            self.display_h,
            image_w,
            image_h,
            offset_x,
            offset_y,
            scale,
        );
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
                    draw_crop_rect(
                        canvas_buf,
                        self.display_w,
                        self.display_h,
                        a,
                        b,
                        self.scale,
                        self.offset_x as f32,
                        self.offset_y as f32,
                    );
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

/// Restores a stroke's previous pixel values and returns its dirty bounds.
/// The returned upper bounds are exclusive, matching `mark_dirty_canvas_rect`.
fn restore_pixel_changes(
    canvas: &mut RgbaImage,
    changes: &[PixelChange],
) -> Option<(u32, u32, u32, u32)> {
    let width = canvas.width() as usize;
    if width == 0 {
        return None;
    }

    let raw = canvas.as_mut();
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    for change in changes {
        let pixel = change.index / 4;
        let x = (pixel % width) as u32;
        let y = (pixel / width) as u32;
        if change.index.saturating_add(4) <= raw.len() {
            raw[change.index..change.index + 4].copy_from_slice(&change.previous);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }

    (min_x != u32::MAX).then_some((min_x, min_y, max_x + 1, max_y + 1))
}

/// Paints a circle into the editable image. When `color` is transparent, the
/// pixel is restored from the untouched source image (the eraser); this keeps
/// erasing non-destructive without allocating another full-size layer.
/// Each touched pixel's pre-stroke value is recorded once for bounded undo.
fn draw_filled_circle_with_undo(
    img: &mut RgbaImage,
    original: &RgbaImage,
    cx: f32,
    cy: f32,
    radius: f32,
    color: [u8; 4],
    stroke: &mut Option<StrokeBuilder>,
) {
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

    let width = img.width() as usize;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            if dx * dx + dy * dy > r2 {
                continue;
            }
            let x = x as u32;
            let y = y as u32;
            let index = (y as usize * width + x as usize) * 4;
            let previous = img.get_pixel(x, y).0;
            let replacement = if color[3] == 0 {
                original.get_pixel(x, y).0
            } else {
                color
            };
            // Do not allocate undo entries for pixels that the operation
            // would leave unchanged (especially erasing untouched areas).
            if previous == replacement {
                continue;
            }
            if let Some(active) = stroke.as_mut() {
                if !active.exceeded_limit && active.seen.insert(index) {
                    if active.changes.len() >= MAX_UNDO_PIXELS {
                        active.changes.clear();
                        active.seen.clear();
                        active.exceeded_limit = true;
                    } else {
                        active.changes.push(PixelChange { index, previous });
                    }
                }
            }
            img.put_pixel(x, y, image::Rgba(replacement));
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
/// display coordinates via `scale` then shifted by `(offset_x, offset_y)`
/// (the image's centered position within the full-output surface). Only
/// allocates a pixmap sized to the rectangle's own bounding box (plus a
/// little stroke padding), not the whole display surface, so dragging a crop
/// box stays cheap too.
#[allow(clippy::too_many_arguments)]
fn draw_crop_rect(
    dst: &mut [u8],
    width: u32,
    height: u32,
    a: (f32, f32),
    b: (f32, f32),
    scale: f32,
    offset_x: f32,
    offset_y: f32,
) {
    if width == 0 || height == 0 {
        return;
    }

    let stroke_w = 2.0f32;
    let pad = stroke_w + 2.0;
    let x0 = (a.0.min(b.0) * scale + offset_x).max(0.0);
    let y0 = (a.1.min(b.1) * scale + offset_y).max(0.0);
    let x1 = (a.0.max(b.0) * scale + offset_x).min(width as f32);
    let y1 = (a.1.max(b.1) * scale + offset_y).min(height as f32);

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

        // The compositor may configure the fullscreen-anchored surface to a
        // size other than the output rect we probed (or, on the very first
        // configure, may not yet match the size `run()` assumed before any
        // roundtrip). `display_buffer` is a cache sized for a *specific*
        // surface size, so if we only updated `display_w`/`display_h` here
        // without rebuilding it, `draw()` would find a length mismatch, log
        // a warning and blank the surface - which is exactly why the
        // screenshot wouldn't show up. Re-fit the image and rebuild the
        // cache for whatever size the compositor actually assigned.
        if assigned_size != (self.display_w, self.display_h) || !self.configured {
            let (surface_w, surface_h) = (assigned_size.0.max(1), assigned_size.1.max(1));
            let (image_w, image_h, offset_x, offset_y, scale) =
                compute_image_placement(self.canvas.width(), self.canvas.height(), surface_w, surface_h);
            self.display_w = surface_w;
            self.display_h = surface_h;
            self.image_w = image_w;
            self.image_h = image_h;
            self.offset_x = offset_x;
            self.offset_y = offset_y;
            self.scale = scale;
            self.display_buffer = build_display_buffer(
                &self.canvas, surface_w, surface_h, image_w, image_h, offset_x, offset_y, scale,
            );
            // Tell the compositor our actual size back, in case it differs
            // from what it just assigned us.
            self.layer.set_size(surface_w, surface_h);
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
        let is_z = matches!(keysym, Keysym::z | Keysym::Z);
        let is_x = matches!(keysym, Keysym::x | Keysym::X);

        if self.ctrl_held && is_s {
            self.commit_stroke();
            self.quick_save();
        } else if self.ctrl_held && is_c {
            self.commit_stroke();
            self.quick_copy();
        } else if self.ctrl_held && is_z {
            self.undo_last_stroke();
            self.request_redraw(qh);
        } else if self.ctrl_held && is_x {
            self.eraser_active = !self.eraser_active;
            eprintln!(
                "{}",
                if self.eraser_active { "Eraser enabled" } else { "Pen enabled" }
            );
            self.request_redraw(qh);
        } else if is_c {
            self.commit_stroke();
            self.eraser_active = false;
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
                    // Do not begin a stroke on the dark backdrop and then
                    // accidentally paint when the pointer enters the image.
                    self.pointer_down = self.position_within_image(event.position);
                    self.last_canvas_point = None;
                    if self.pointer_down {
                        let canvas_pt = self.to_canvas_point(event.position);
                        match self.mode {
                            Mode::Doodle => {
                                self.begin_stroke();
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
                }
                PointerEventKind::Release { .. } => {
                    self.pointer_down = false;
                    self.last_canvas_point = None;
                    self.commit_stroke();
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


#[cfg(test)]
mod editor_tests {
    use super::{draw_filled_circle_with_undo, restore_pixel_changes, StrokeBuilder};
    use image::{Rgba, RgbaImage};

    #[test]
    fn eraser_restores_original_pixels() {
        let original = RgbaImage::from_pixel(16, 16, Rgba([20, 40, 60, 255]));
        let mut canvas = original.clone();
        let mut stroke = Some(StrokeBuilder::default());
        draw_filled_circle_with_undo(
            &mut canvas, &original, 8.0, 8.0, 4.0, [200, 100, 50, 255], &mut stroke,
        );
        draw_filled_circle_with_undo(
            &mut canvas, &original, 8.0, 8.0, 2.0, [0, 0, 0, 0], &mut stroke,
        );
        assert_eq!(canvas.get_pixel(8, 8).0, original.get_pixel(8, 8).0);
        assert_eq!(canvas.get_pixel(4, 8).0, [200, 100, 50, 255]);
    }

    #[test]
    fn undo_restores_previous_pixels_and_reports_dirty_bounds() {
        let original = RgbaImage::from_pixel(16, 16, Rgba([20, 40, 60, 255]));
        let mut canvas = original.clone();
        let mut stroke = Some(StrokeBuilder::default());
        draw_filled_circle_with_undo(
            &mut canvas, &original, 8.0, 8.0, 3.0, [240, 10, 20, 255], &mut stroke,
        );
        let changes = stroke.expect("stroke history").changes;
        let bounds = restore_pixel_changes(&mut canvas, &changes);
        assert_eq!(bounds, Some((5, 5, 11, 11)));
        assert_eq!(canvas, original);
    }

    #[test]
    fn undoing_an_erase_restores_the_doodle_then_undoing_doodle_restores_source() {
        let original = RgbaImage::from_pixel(16, 16, Rgba([20, 40, 60, 255]));
        let mut canvas = original.clone();
        let mut draw_stroke = Some(StrokeBuilder::default());
        draw_filled_circle_with_undo(
            &mut canvas, &original, 8.0, 8.0, 4.0, [200, 100, 50, 255], &mut draw_stroke,
        );
        let draw_changes = draw_stroke.expect("draw stroke").changes;

        let mut erase_stroke = Some(StrokeBuilder::default());
        draw_filled_circle_with_undo(
            &mut canvas, &original, 8.0, 8.0, 2.0, [0, 0, 0, 0], &mut erase_stroke,
        );
        let erase_changes = erase_stroke.expect("erase stroke").changes;
        assert_eq!(canvas.get_pixel(8, 8).0, original.get_pixel(8, 8).0);

        restore_pixel_changes(&mut canvas, &erase_changes);
        assert_eq!(canvas.get_pixel(8, 8).0, [200, 100, 50, 255]);
        restore_pixel_changes(&mut canvas, &draw_changes);
        assert_eq!(canvas, original);
    }

    #[test]
    fn erasing_untouched_pixels_does_not_create_undo_entries() {
        let original = RgbaImage::from_pixel(16, 16, Rgba([20, 40, 60, 255]));
        let mut canvas = original.clone();
        let mut stroke = Some(StrokeBuilder::default());
        draw_filled_circle_with_undo(
            &mut canvas, &original, 8.0, 8.0, 4.0, [0, 0, 0, 0], &mut stroke,
        );
        assert!(stroke.expect("active stroke").changes.is_empty());
        assert_eq!(canvas, original);
    }

    #[test]
    fn stroke_history_records_each_pixel_only_once() {
        let original = RgbaImage::from_pixel(16, 16, Rgba([0, 0, 0, 255]));
        let mut canvas = original.clone();
        let mut stroke = Some(StrokeBuilder::default());
        draw_filled_circle_with_undo(
            &mut canvas, &original, 8.0, 8.0, 4.0, [255, 0, 0, 255], &mut stroke,
        );
        draw_filled_circle_with_undo(
            &mut canvas, &original, 8.0, 8.0, 4.0, [0, 255, 0, 255], &mut stroke,
        );
        let active = stroke.expect("active stroke");
        assert_eq!(active.changes.len(), active.seen.len());
        assert!(!active.changes.is_empty());
        assert!(active.changes.iter().all(|change| change.previous == [0, 0, 0, 255]));
    }
}
