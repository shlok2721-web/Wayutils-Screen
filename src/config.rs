//! User configuration file (`~/.config/Wayutils-Screen/config.toml`).
//!
//! This intentionally does not pull in a TOML/serde dependency: the schema is
//! small and flat, so a a few dozen lines of hand-written parsing keeps the
//! dependency tree (and therefore build times) unchanged. Any line that the
//! parser doesn't understand is ignored with a warning rather than treated as
//! a hard error, so a typo in the file never prevents the program from
//! starting - it just falls back to defaults for that one setting.

use std::fmt;
use std::path::{Path, PathBuf};

/// Default recording mode: honour the caller's region selection, or always
/// record the whole (primary) output regardless of what was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecordMode {
    #[default]
    Region,
    Fullscreen,
}

impl fmt::Display for RecordMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordMode::Region => write!(f, "region"),
            RecordMode::Fullscreen => write!(f, "fullscreen"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Default directory screenshots are saved into when `-o/--output` is
    /// not given. Overrides the built-in `~/Pictures/Wayutils-Screen`.
    pub image_dir: Option<PathBuf>,
    /// Default directory recordings are saved into when `-o/--output` is
    /// not given. Overrides the built-in `~/Videos/Wayutils-Screen`.
    pub video_dir: Option<PathBuf>,
    /// Reserved for a future standalone-audio output; currently audio is
    /// muxed directly into the video file, but the directory is still
    /// accepted/parsed so the `[paths]` section stays self-describing.
    pub audio_dir: Option<PathBuf>,
    /// Default `--record` mode when neither `--fullscreen` nor an explicit
    /// region is forced by the command line. When set to `fullscreen`, this
    /// wins over a manually selected region unless `--fullscreen`/region are
    /// not relevant (i.e. this makes fullscreen the sticky default).
    pub record_mode: RecordMode,
    /// Default `--resolution` for `--record`, used when the flag is omitted.
    pub record_resolution: Option<(u32, u32)>,
    /// Default `--fps` for `--record`, used when the flag is omitted.
    pub record_fps: Option<u32>,
    /// Capture system audio (via PulseAudio/PipeWire's `pulse` ffmpeg input)
    /// and mux it into the recorded video.
    pub record_audio: bool,
}

impl Config {
    /// Loads the config file, creating a commented default template on first
    /// run so the file is self-documenting. Never fails: a missing or
    /// unreadable/unparsable file just yields defaults.
    pub fn load() -> Self {
        let path = match config_path() {
            Some(p) => p,
            None => return Self::default(),
        };

        if !path.exists() {
            if let Err(err) = write_default_template(&path) {
                log::info!("could not create default config at {}: {err}", path.display());
            }
            return Self::default();
        }

        match std::fs::read_to_string(&path) {
            Ok(text) => parse(&text),
            Err(err) => {
                log::warn!("failed to read config {}: {err}", path.display());
                Self::default()
            }
        }
    }
}

/// `$XDG_CONFIG_HOME/Wayutils-Screen/config.toml`, falling back to
/// `~/.config/Wayutils-Screen/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("Wayutils-Screen").join("config.toml"))
}

fn write_default_template(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, DEFAULT_TEMPLATE)
}

/// Very small TOML-subset parser: `[section]` headers, `key = value` lines,
/// `#` comments. Values are either `"quoted strings"`, bare `true`/`false`,
/// bare integers, or a bare `WxH` resolution token. Good enough for this
/// flat, known schema - not a general TOML implementation.
fn parse(text: &str) -> Config {
    let mut cfg = Config::default();
    let mut section = String::new();

    for (lineno, raw_line) in text.lines().enumerate() {
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            log::warn!("config.toml:{}: ignoring unrecognized line: {raw_line}", lineno + 1);
            continue;
        };
        let key = key.trim();
        let value = unquote(value.trim());

        match (section.as_str(), key) {
            ("paths", "image_dir") => cfg.image_dir = Some(expand_home(&value)),
            ("paths", "video_dir") => cfg.video_dir = Some(expand_home(&value)),
            ("paths", "audio_dir") => cfg.audio_dir = Some(expand_home(&value)),
            ("recording", "mode") => {
                cfg.record_mode = match value.to_ascii_lowercase().as_str() {
                    "fullscreen" | "full" | "full-screen" => RecordMode::Fullscreen,
                    "region" | "selection" => RecordMode::Region,
                    other => {
                        log::warn!(
                            "config.toml:{}: unknown recording.mode '{other}', expected 'region' or 'fullscreen'",
                            lineno + 1
                        );
                        RecordMode::Region
                    }
                };
            }
            ("recording", "resolution") => match parse_resolution(&value) {
                Some(res) => cfg.record_resolution = Some(res),
                None => log::warn!(
                    "config.toml:{}: invalid recording.resolution '{value}' (expected e.g. 1920x1080)",
                    lineno + 1
                ),
            },
            ("recording", "fps") => match value.parse::<u32>() {
                Ok(fps) if fps > 0 => cfg.record_fps = Some(fps),
                _ => log::warn!("config.toml:{}: invalid recording.fps '{value}'", lineno + 1),
            },
            ("recording", "audio") => match value.to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => cfg.record_audio = true,
                "false" | "no" | "0" => cfg.record_audio = false,
                other => log::warn!("config.toml:{}: invalid recording.audio '{other}'", lineno + 1),
            },
            ("", _) => log::warn!(
                "config.toml:{}: key '{key}' outside of any [section]; ignoring",
                lineno + 1
            ),
            (other_section, other_key) => log::warn!(
                "config.toml:{}: ignoring unknown setting '{other_key}' in [{other_section}]",
                lineno + 1
            ),
        }
    }

    cfg
}

fn strip_comment(line: &str) -> &str {
    // No quoted strings in this schema contain '#', so a naive split is safe.
    match line.find('#') {
        Some(idx) => &line[..idx],
        None => line,
    }
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_resolution(raw: &str) -> Option<(u32, u32)> {
    let (w, h) = raw.split_once(['x', 'X'])?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    if w == 0 || h == 0 {
        None
    } else {
        Some((w, h))
    }
}

/// Expands a leading `~` to `$HOME`, the only shell-ism this tiny parser
/// supports (good enough for path settings in a config file).
fn expand_home(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if raw == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(raw)
}

const DEFAULT_TEMPLATE: &str = r#"# Wayutils-Screen configuration.
#
# All settings are optional - delete or comment out (#) anything you don't
# want to override; the built-in defaults apply instead. Command-line flags
# (e.g. `-o`, `--resolution`, `--fullscreen`) always win over this file.

[paths]
# Where screenshots are saved when -o/--output is not given.
# image_dir = "~/Pictures/Wayutils-Screen"

# Where recordings are saved when -o/--output is not given.
# video_dir = "~/Videos/Wayutils-Screen"

# Reserved for a future standalone-audio export.
# audio_dir = "~/Videos/Wayutils-Screen"

[recording]
# "region" (honour the selected/slurp region) or "fullscreen" (always record
# the whole primary output, even if a region was selected).
mode = "region"

# Default --resolution for --record, e.g. "1920x1080". Leave commented out to
# use the captured region's own size.
# resolution = "1920x1080"

# Default --fps for --record.
# fps = 12

# Capture system audio (via PulseAudio/PipeWire) into recordings.
audio = false
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_paths_and_recording_section() {
        let text = r#"
            [paths]
            image_dir = "~/Pictures/Shots"
            video_dir = "/data/videos"

            [recording]
            mode = "fullscreen"
            resolution = "1280x720"
            fps = 30
            audio = true
        "#;
        let cfg = parse(text);
        assert_eq!(cfg.video_dir, Some(PathBuf::from("/data/videos")));
        assert_eq!(cfg.record_mode, RecordMode::Fullscreen);
        assert_eq!(cfg.record_resolution, Some((1280, 720)));
        assert_eq!(cfg.record_fps, Some(30));
        assert!(cfg.record_audio);
    }

    #[test]
    fn unknown_or_malformed_lines_are_ignored_not_fatal() {
        let text = r#"
            [recording]
            mode = "sideways"
            resolution = "nope"
            fps = -3
            bogus_key = 1

            [typo_section]
            whatever = true
        "#;
        let cfg = parse(text);
        assert_eq!(cfg.record_mode, RecordMode::Region);
        assert_eq!(cfg.record_resolution, None);
        assert_eq!(cfg.record_fps, None);
    }

    #[test]
    fn defaults_when_file_absent() {
        let cfg = Config::default();
        assert_eq!(cfg.record_mode, RecordMode::Region);
        assert!(!cfg.record_audio);
        assert_eq!(cfg.image_dir, None);
    }
}
