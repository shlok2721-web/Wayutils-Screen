/// Default preview width in pixels.
pub const PREVIEW_MAX_WIDTH: u32 = 280;
/// Height of the overlay control bar in pixels.
pub const CONTROL_BAR_HEIGHT: u32 = 36;
/// Number of buttons in the capture-preview control bar: Save, Copy, Edit,
/// Record, Pause/Resume, Quit. Shared by `overlay.rs` (hit-testing) and
/// `overlay/drawing.rs` (layout/icons) so they can never drift apart.
pub const CONTROL_BUTTON_COUNT: u32 = 6;
/// Border width for the selected capture region overlay.
pub const REGION_BORDER_WIDTH: u32 = 2;
/// Border color (RGBA) for the selected capture region overlay.
pub const REGION_BORDER_COLOR: [u8; 4] = [52, 152, 219, 200];
