use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};

use crate::constants::PREVIEW_MAX_WIDTH;

/// 使用 `--no-preview` 且未显式指定空闲时间时的默认自动完成等待时长
pub const DEFAULT_NO_PREVIEW_IDLE_DURATION: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Algorithm {
    /// Column sampling (fast, good for most cases)
    ColSample,
    /// Template matching (slower, more accurate)
    Template,
    /// Edge detection (for transparent backgrounds)
    Edge,
    /// FAST corner + HNSW index (high accuracy, from snow-shot)
    Fast,
    /// OpenCV ORB + brute-force matching + affine RANSAC
    #[default]
    #[value(name = "opencv-orb")]
    OpenCvOrb,
    /// Eight-band signature match. Ignores static sidebars and fixed header/footer bars.
    Band,
}

/// Screen capture backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Backend {
    /// Detect from the desktop session (KDE uses kwin, otherwise grim)
    #[default]
    Auto,
    /// grim / wlr-screencopy (Sway, Hyprland, niri, river, ...)
    Grim,
    /// org.kde.KWin.ScreenShot2 D-Bus interface (KDE Plasma)
    Kwin,
    /// GNOME Shell scroll helper (requires the bundled extension)
    Gnome,
}

#[derive(Parser, Debug, Clone)]
#[command(name = "Wayutils-Screen")]
#[command(about = "A scrolling screenshot tool for Wayland", long_about = None)]
pub struct Args {
    /// Output file path (default: ~/Pictures/Wayutils-Screen/Wayutils-Screen-<timestamp>.png).
    ///
    /// .png and .jpg/.jpeg pick that format. A path with no (or an unknown) extension is
    /// written as PNG to exactly that path. An existing directory gets a timestamped PNG inside it.
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Preview width in pixels
    #[arg(short = 'w', long, default_value_t = PREVIEW_MAX_WIDTH)]
    pub preview_width: u32,

    /// Copy to clipboard instead of saving to file
    #[arg(short, long)]
    pub clipboard: bool,

    /// Disable preview window (auto-saves after idle unless --auto-save-after-idle overrides the duration)
    #[arg(long)]
    pub no_preview: bool,

    /// Automatically save (or copy with -c) and exit after this idle duration without scroll updates (e.g. 2s, 1500ms, 2.5)
    #[arg(long, value_name = "DURATION", value_parser = parse_idle_duration)]
    pub auto_save_after_idle: Option<Duration>,

    /// Disable region border overlay
    #[arg(long)]
    pub no_border: bool,

    /// Stitching algorithm to use
    #[arg(short, long, value_enum, default_value_t = Algorithm::OpenCvOrb)]
    pub algorithm: Algorithm,

    /// Screen capture backend (auto detects KDE vs wlroots)
    #[arg(long, value_enum, default_value_t = Backend::Auto)]
    pub backend: Backend,

    /// Existing slurp geometry to use instead of selecting a region. Use '-' to read from stdin.
    ///
    /// When `--edit` is passed, this positional is instead read as the path to the
    /// screenshot file to open in the editor, e.g. `Wayutils-Screen shot.png --edit`.
    #[arg(value_name = "REGION", num_args = 0..=2, allow_hyphen_values = true)]
    pub region: Vec<String>,

    /// Open an existing screenshot in the doodle/crop editor instead of taking a new one.
    ///
    /// Usage: `Wayutils-Screen <screenshot> --edit`
    #[arg(long)]
    pub edit: bool,

    /// Record a video instead of taking a screenshot (requires the `ffmpeg` binary).
    #[arg(long)]
    pub record: bool,

    /// With --record, capture the whole screen instead of selecting a region.
    #[arg(long)]
    pub fullscreen: bool,

    /// With --record, scale the output video to this resolution, e.g. 1280x720
    /// (defaults to the size of the captured region).
    #[arg(long, value_name = "WxH", value_parser = parse_resolution)]
    pub resolution: Option<(u32, u32)>,

    /// With --record, the recording frame rate (defaults to the value in
    /// config.toml's [recording] fps, or 12 if that is also unset).
    #[arg(long)]
    pub fps: Option<u32>,
}

impl Args {
    pub fn parse_args() -> Self {
        let argv: Vec<String> = std::env::args().collect();
        Args::parse_from(reorder_args(argv))
    }

    pub fn slurp_output(&self) -> Option<String> {
        if self.region.is_empty() {
            None
        } else {
            Some(self.region.join(" "))
        }
    }

    /// Returns the screenshot path to open when `--edit` is passed with a positional path.
    pub fn edit_target(&self) -> Option<PathBuf> {
        if !self.edit {
            return None;
        }
        self.region.first().map(PathBuf::from)
    }

    /// 计算当前会话生效的空闲自动完成时长
    ///
    /// 该方法无传参；返回生效的空闲等待时长，`None` 表示不启用自动完成
    pub fn effective_auto_save_idle(&self) -> Option<Duration> {
        if let Some(duration) = self.auto_save_after_idle {
            return Some(duration);
        }
        if self.no_preview {
            return Some(DEFAULT_NO_PREVIEW_IDLE_DURATION);
        }
        None
    }
}

/// 解析空闲自动完成时长参数，支持纯秒数以及 `ms`、`s`、`m` 后缀
///
/// `raw` 为命令行传入的时长字符串；成功返回正数 `Duration`，失败返回错误说明
pub fn parse_idle_duration(raw: &str) -> Result<Duration, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("duration cannot be empty".to_string());
    }

    // 1. 提取数值部分与单位换算倍率
    let (num_str, multiplier_secs) = if let Some(v) = trimmed.strip_suffix("ms") {
        (v.trim(), 0.001_f64)
    } else if let Some(v) = trimmed.strip_suffix('s') {
        (v.trim(), 1.0_f64)
    } else if let Some(v) = trimmed.strip_suffix('m') {
        (v.trim(), 60.0_f64)
    } else {
        (trimmed, 1.0_f64)
    };

    // 2. 校验数值有效性与正数约束
    let value: f64 = num_str
        .parse()
        .map_err(|_| format!("invalid duration '{raw}' (expected e.g. 2s, 1500ms, 2.5)"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(format!("duration must be greater than 0, got '{raw}'"));
    }

    let secs = value * multiplier_secs;
    if !secs.is_finite() || secs <= 0.0 {
        return Err(format!("duration out of range: '{raw}'"));
    }

    Ok(Duration::from_secs_f64(secs))
}

/// Options that consume a following value token when reordering argv.
const VALUE_OPTS: &[&str] = &[
    "-o",
    "--output",
    "-w",
    "--preview-width",
    "--auto-save-after-idle",
    "-a",
    "--algorithm",
    "--backend",
    "--resolution",
    "--fps",
];

/// Rearranges argv so recognized flags/options come before positional
/// arguments, regardless of how the user typed them.
///
/// This works around a clap quirk: the trailing `region` positional needs
/// `allow_hyphen_values` to accept negative slurp coordinates like
/// `-1920,-10`, but that same setting lets it swallow a *later* recognized
/// flag instead of letting clap parse it as an option - e.g. without this,
/// `Wayutils-Screen shot.png --edit` would leave `--edit` unset because
/// `--edit` gets captured as a second region value. Moving all recognized
/// flags (and the option values they take) ahead of the positionals sidesteps
/// that, so flags work regardless of where the user puts them on the line.
fn reorder_args(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return args;
    }

    let mut iter = args.into_iter();
    let prog = iter.next().unwrap_or_default();
    let rest: Vec<String> = iter.collect();

    let dashdash_idx = rest.iter().position(|t| t == "--");
    let (scan, tail): (&[String], Vec<String>) = match dashdash_idx {
        Some(idx) => (&rest[..idx], rest[idx..].to_vec()),
        None => (&rest[..], Vec::new()),
    };

    let mut flags = Vec::new();
    let mut positionals = Vec::new();
    let mut i = 0;
    while i < scan.len() {
        let token = &scan[i];
        if looks_like_flag(token) {
            flags.push(token.clone());
            if VALUE_OPTS.contains(&token.as_str()) && i + 1 < scan.len() {
                flags.push(scan[i + 1].clone());
                i += 2;
                continue;
            }
        } else {
            positionals.push(token.clone());
        }
        i += 1;
    }

    let mut result = Vec::with_capacity(flags.len() + positionals.len() + tail.len() + 1);
    result.push(prog);
    result.extend(flags);
    result.extend(positionals);
    result.extend(tail);
    result
}

/// A token counts as a flag if it starts with `-` and isn't a bare `-`
/// (stdin region marker) or a negative-number-looking slurp coordinate like
/// `-1920,-10` (no real flag here starts with a digit right after the dash).
fn looks_like_flag(token: &str) -> bool {
    let mut chars = token.chars();
    if chars.next() != Some('-') {
        return false;
    }
    match chars.next() {
        Some(c) => !c.is_ascii_digit(),
        None => false,
    }
}

/// Parses a `WIDTHxHEIGHT` resolution string, e.g. `1280x720`.
pub fn parse_resolution(raw: &str) -> Result<(u32, u32), String> {
    let (w_str, h_str) = raw
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("invalid resolution '{raw}' (expected e.g. 1280x720)"))?;
    let w: u32 = w_str
        .trim()
        .parse()
        .map_err(|_| format!("invalid width in resolution '{raw}'"))?;
    let h: u32 = h_str
        .trim()
        .parse()
        .map_err(|_| format!("invalid height in resolution '{raw}'"))?;
    if w == 0 || h == 0 {
        return Err(format!("resolution must be greater than 0, got '{raw}'"));
    }
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Algorithm, Args};

    #[test]
    fn algorithm_enum_and_cli_share_the_same_default() {
        let args = Args::try_parse_from(["Wayutils-Screen"]).unwrap();

        assert_eq!(Algorithm::default(), Algorithm::OpenCvOrb);
        assert_eq!(args.algorithm, Algorithm::OpenCvOrb);
    }

    #[test]
    fn parses_quoted_slurp_region() {
        let args = Args::try_parse_from(["Wayutils-Screen", "10,20 300x400"]).unwrap();

        assert_eq!(args.slurp_output().as_deref(), Some("10,20 300x400"));
    }

    #[test]
    fn parses_unquoted_slurp_region_parts() {
        let args = Args::try_parse_from(["Wayutils-Screen", "10,20", "300x400"]).unwrap();

        assert_eq!(args.slurp_output().as_deref(), Some("10,20 300x400"));
    }

    #[test]
    fn parses_stdin_region_marker() {
        let args = Args::try_parse_from(["Wayutils-Screen", "-"]).unwrap();

        assert_eq!(args.slurp_output().as_deref(), Some("-"));
    }

    #[test]
    fn parses_negative_slurp_coordinates() {
        let args = Args::try_parse_from(["Wayutils-Screen", "-1920,-10", "800x600"]).unwrap();

        assert_eq!(args.slurp_output().as_deref(), Some("-1920,-10 800x600"));
    }

    #[test]
    fn keeps_existing_flags_with_region() {
        let args = Args::try_parse_from(["Wayutils-Screen", "-c", "10,20", "300x400"]).unwrap();

        assert!(args.clipboard);
        assert_eq!(args.slurp_output().as_deref(), Some("10,20 300x400"));
    }

    #[test]
    fn no_preview_enables_default_idle_auto_save() {
        let args = Args::try_parse_from(["Wayutils-Screen", "--no-preview"]).unwrap();

        assert_eq!(
            args.effective_auto_save_idle(),
            Some(super::DEFAULT_NO_PREVIEW_IDLE_DURATION)
        );
    }

    #[test]
    fn interactive_mode_disables_idle_auto_save_by_default() {
        let args = Args::try_parse_from(["Wayutils-Screen"]).unwrap();

        assert_eq!(args.effective_auto_save_idle(), None);
    }

    #[test]
    fn parses_auto_save_after_idle_durations() {
        use std::time::Duration;

        let args_sec =
            Args::try_parse_from(["Wayutils-Screen", "--auto-save-after-idle", "3s"]).unwrap();
        assert_eq!(args_sec.effective_auto_save_idle(), Some(Duration::from_secs(3)));

        let args_ms =
            Args::try_parse_from(["Wayutils-Screen", "--auto-save-after-idle", "1500ms"]).unwrap();
        assert_eq!(
            args_ms.effective_auto_save_idle(),
            Some(Duration::from_millis(1500))
        );

        let args_num =
            Args::try_parse_from(["Wayutils-Screen", "--auto-save-after-idle", "2.5"]).unwrap();
        assert_eq!(
            args_num.effective_auto_save_idle(),
            Some(Duration::from_millis(2500))
        );

        let args_min =
            Args::try_parse_from(["Wayutils-Screen", "--auto-save-after-idle", "1m"]).unwrap();
        assert_eq!(
            args_min.effective_auto_save_idle(),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn rejects_invalid_auto_save_after_idle() {
        assert!(Args::try_parse_from(["Wayutils-Screen", "--auto-save-after-idle", "0"]).is_err());
        assert!(Args::try_parse_from(["Wayutils-Screen", "--auto-save-after-idle", "-1s"]).is_err());
        assert!(Args::try_parse_from(["Wayutils-Screen", "--auto-save-after-idle", "abc"]).is_err());
    }

    #[test]
    fn edit_flag_reads_positional_as_file_path() {
        // `--edit` after the positional, as documented in the CLI's own usage
        // string (`Wayutils-Screen <screenshot> --edit`) and exactly how
        // `parse_args()` invokes clap, via `reorder_args`.
        let argv = super::reorder_args(
            ["Wayutils-Screen", "shot.png", "--edit"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        let args = Args::try_parse_from(argv).unwrap();
        assert!(args.edit);
        assert_eq!(args.edit_target(), Some(std::path::PathBuf::from("shot.png")));
    }

    #[test]
    fn edit_flag_also_works_before_positional() {
        let argv = super::reorder_args(
            ["Wayutils-Screen", "--edit", "shot.png"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        let args = Args::try_parse_from(argv).unwrap();
        assert!(args.edit);
        assert_eq!(args.edit_target(), Some(std::path::PathBuf::from("shot.png")));
    }

    #[test]
    fn record_flags_work_after_region_positional() {
        let argv = super::reorder_args(
            ["Wayutils-Screen", "10,20", "300x400", "--record", "--fullscreen"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        let args = Args::try_parse_from(argv).unwrap();
        assert!(args.record);
        assert!(args.fullscreen);
        assert_eq!(args.slurp_output().as_deref(), Some("10,20 300x400"));
    }

    #[test]
    fn reorder_keeps_negative_slurp_coordinates_positional() {
        let argv = super::reorder_args(
            ["Wayutils-Screen", "-1920,-10", "800x600", "-c"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        let args = Args::try_parse_from(argv).unwrap();
        assert!(args.clipboard);
        assert_eq!(args.slurp_output().as_deref(), Some("-1920,-10 800x600"));
    }

    #[test]
    fn edit_target_is_none_without_edit_flag() {
        let args = Args::try_parse_from(["Wayutils-Screen", "shot.png"]).unwrap();
        assert_eq!(args.edit_target(), None);
    }

    #[test]
    fn parses_record_resolution() {
        let args =
            Args::try_parse_from(["Wayutils-Screen", "--record", "--resolution", "1280x720"])
                .unwrap();
        assert!(args.record);
        assert_eq!(args.resolution, Some((1280, 720)));
    }

    #[test]
    fn rejects_invalid_resolution() {
        assert!(Args::try_parse_from(["Wayutils-Screen", "--record", "--resolution", "bad"])
            .is_err());
        assert!(Args::try_parse_from(["Wayutils-Screen", "--record", "--resolution", "0x0"])
            .is_err());
    }
}
