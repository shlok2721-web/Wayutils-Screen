use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, ImageFormat, RgbaImage};

/// Saves the stitched image to an explicit path or a timestamped default path.
///
/// 显式路径可以没有扩展名（如 `-o /tmp/focus`），此时按 PNG 编码并写入该确切路径；
/// 路径是已存在的目录时，在其中生成带时间戳的文件。
pub fn save_image(image: Arc<RgbaImage>, output_path: Option<PathBuf>) -> Result<PathBuf> {
    let path = match output_path {
        Some(p) => {
            let p = if p.is_dir() {
                p.join(timestamped_filename())
            } else {
                p
            };
            if let Some(parent) = p.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            p
        }
        None => {
            let output_dir = default_output_dir();
            std::fs::create_dir_all(&output_dir)?;
            output_dir.join(timestamped_filename())
        }
    };
    image
        .save_with_format(&path, format_for_path(&path))
        .with_context(|| format!("failed to write image to {}", path.display()))?;
    Ok(path)
}

/// 由扩展名推断图片格式；无扩展名或不支持的扩展名一律回退为 PNG
pub(crate) fn format_for_path(path: &Path) -> ImageFormat {
    match ImageFormat::from_path(path) {
        Ok(fmt @ (ImageFormat::Png | ImageFormat::Jpeg)) => fmt,
        _ => ImageFormat::Png,
    }
}

fn timestamped_filename() -> String {
    format!("Wayutils-Screen-{}.png", Local::now().format("%Y%m%d-%H%M%S"))
}

/// Copies the stitched image to clipboard using `wl-copy` or `xclip`.
pub fn copy_to_clipboard(image: Arc<RgbaImage>) -> Result<()> {
    let png_bytes = encode_png(&image)?;
    if command_exists("wl-copy") {
        let mut child = Command::new("wl-copy")
            .arg("--type")
            .arg("image/png")
            .stdin(Stdio::piped())
            .spawn()
            .context("failed to spawn wl-copy")?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(&png_bytes)?;
        }
        let status = child.wait()?;
        if !status.success() {
            bail!("wl-copy failed");
        }
        return Ok(());
    }

    if command_exists("xclip") {
        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "image/png", "-i"])
            .stdin(Stdio::piped())
            .spawn()
            .context("failed to spawn xclip")?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(&png_bytes)?;
        }
        let status = child.wait()?;
        if !status.success() {
            bail!("xclip failed");
        }
        return Ok(());
    }

    bail!("no clipboard tool found (wl-copy/xclip)")
}

fn encode_png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let encoder = PngEncoder::new(&mut data);
    encoder.write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        ExtendedColorType::Rgba8,
    )?;
    Ok(data)
}

fn command_exists(cmd: &str) -> bool {
    if let Some(paths) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&paths) {
            let full = path.join(cmd);
            if full.is_file() {
                return true;
            }
        }
    }
    false
}

fn default_output_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join("Pictures")
        .join("Wayutils-Screen")
}

/// Finds the most recently modified screenshot in the default screenshots
/// folder, so `--edit` with no explicit path edits the screenshot you just
/// took instead of silently falling through to a brand new capture.
pub(crate) fn latest_screenshot_path() -> Result<PathBuf> {
    let dir = default_output_dir();
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;

    let entries = std::fs::read_dir(&dir)
        .with_context(|| format!("failed to read screenshots folder {}", dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_image = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("png") || ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg"))
            .unwrap_or(false);
        if !is_image {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if best.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
            best = Some((modified, path));
        }
    }

    best.map(|(_, path)| path).ok_or_else(|| {
        anyhow!(
            "--edit: no path given and no screenshots found in {} - pass one explicitly, \
             e.g. `Wayutils-Screen <screenshot> --edit`",
            dir.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use image::{ImageFormat, RgbaImage};

    use super::{format_for_path, save_image};

    #[test]
    fn extensionless_and_unknown_extensions_fall_back_to_png() {
        assert_eq!(format_for_path(Path::new("/tmp/focus")), ImageFormat::Png);
        assert_eq!(format_for_path(Path::new("/tmp/a.shot")), ImageFormat::Png);
        assert_eq!(format_for_path(Path::new("/tmp/a.png")), ImageFormat::Png);
        assert_eq!(format_for_path(Path::new("/tmp/a.JPG")), ImageFormat::Jpeg);
    }

    #[test]
    fn saves_extensionless_path_as_png_at_exact_path() {
        let dir = std::env::temp_dir().join(format!("wss-test-{}", std::process::id()));
        let target = dir.join("focus");
        let out = save_image(Arc::new(RgbaImage::new(4, 4)), Some(target.clone())).unwrap();

        assert_eq!(out, target);
        let bytes = std::fs::read(&target).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_directory_gets_timestamped_file() {
        let dir = std::env::temp_dir().join(format!("wss-test-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = save_image(Arc::new(RgbaImage::new(4, 4)), Some(dir.clone())).unwrap();

        assert_eq!(out.parent(), Some(dir.as_path()));
        assert!(out.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
