//! `--record`: screen/region video recording.
//!
//! This deliberately keeps the Rust side minimal: we reuse the existing
//! screenshot [`Capturer`] backends to grab one RGBA frame at a time, and
//! hand raw frames to the external `ffmpeg` binary over a pipe for encoding.
//! No video-encoding crate is linked into the binary; `ffmpeg` is only
//! required (and only invoked) when the user actually asks to record.

use std::borrow::Cow;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use image::imageops::FilterType;
use image::RgbaImage;

use crate::capture::{read_region_from_stdin, region_from_slurp_output, select_region, Capturer};
use crate::cli::Args;
use crate::region_overlay::RegionOverlay;
use crate::types::Region;

/// Set by the SIGINT handler; polled from the capture loop.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_sig: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

/// Entry point for `--record`.
pub fn run(args: &Args) -> Result<()> {
    if which("ffmpeg").is_none() {
        bail!(
            "ffmpeg was not found in PATH; video recording needs the `ffmpeg` binary installed \
             (only used as an external process, not linked into Wayutils-Screen)"
        );
    }

    let region = resolve_record_region(args)?;
    log::info!(
        "Recording region: {},{} {}x{}",
        region.x,
        region.y,
        region.w,
        region.h
    );

    let capturer = Capturer::new(args.backend)?;
    let output_path = resolve_video_path(args.output.clone())?;
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // Visual indicator of the recorded area; best-effort only.
    let mut border = if args.no_border {
        None
    } else {
        RegionOverlay::new(region.clone()).ok()
    };

    let first_frame = capturer
        .capture(&region)
        .context("failed to capture the first frame")?;
    // libx264 (with yuv420p chroma subsampling) requires even width/height;
    // an odd capture region is rounded down by 1px rather than failing.
    let (capture_w, capture_h) = (
        round_down_even(first_frame.width()),
        round_down_even(first_frame.height()),
    );
    if capture_w == 0 || capture_h == 0 {
        bail!("captured frame is too small to record (region was {region:?})");
    }
    let (mut out_w, mut out_h) = args.resolution.unwrap_or((capture_w, capture_h));
    out_w = round_down_even(out_w).max(2);
    out_h = round_down_even(out_h).max(2);
    let fps = args.fps.max(1);

    let mut child = spawn_ffmpeg(&output_path, capture_w, capture_h, out_w, out_h, fps)?;
    let mut stdin = child
        .stdin
        .take()
        .context("failed to open ffmpeg's stdin pipe")?;

    install_sigint_handler();
    let stop_from_stdin = spawn_stdin_stop_watcher();

    eprintln!(
        "Recording {}x{} @ {}fps to {} - press Enter or Ctrl+C to stop",
        capture_w,
        capture_h,
        fps,
        output_path.display()
    );

    let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let mut frame = first_frame;
    let mut frame_count: u64 = 0;
    let started_at = Instant::now();
    let mut encode_failed = false;

    loop {
        if should_stop(&stop_from_stdin) {
            break;
        }

        let tick_start = Instant::now();
        let normalized = normalize_frame(&frame, capture_w, capture_h);
        if stdin.write_all(normalized.as_raw()).is_err() {
            log::warn!("ffmpeg stdin closed unexpectedly; stopping recording");
            encode_failed = true;
            break;
        }
        frame_count += 1;

        let elapsed = tick_start.elapsed();
        if elapsed < frame_interval {
            thread::sleep(frame_interval - elapsed);
        }

        if should_stop(&stop_from_stdin) {
            break;
        }

        frame = match capturer.capture(&region) {
            Ok(frame) => frame,
            Err(err) => {
                log::warn!("frame capture failed, stopping recording: {err}");
                break;
            }
        };
    }

    // Closing stdin signals EOF to ffmpeg so it can finalize the container
    // (write the moov atom, flush the encoder, etc.) instead of leaving a
    // broken file behind.
    drop(stdin);
    let status = child.wait().context("failed waiting for ffmpeg to exit")?;

    if let Some(ref mut overlay) = border {
        overlay.stop();
    }

    if encode_failed || !status.success() {
        bail!(
            "ffmpeg exited with an error while encoding {}",
            output_path.display()
        );
    }

    let secs = started_at.elapsed().as_secs_f64();
    eprintln!(
        "Saved ~{secs:.1}s ({frame_count} frames) to {}",
        output_path.display()
    );
    Ok(())
}

/// Rounds a dimension down to the nearest even number (libx264 + yuv420p
/// require both width and height to be divisible by 2).
fn round_down_even(v: u32) -> u32 {
    v & !1
}

fn should_stop(stop_from_stdin: &Arc<AtomicBool>) -> bool {
    STOP_REQUESTED.load(Ordering::SeqCst) || stop_from_stdin.load(Ordering::SeqCst)
}

/// Ensures every frame fed to ffmpeg matches the locked-in capture size,
/// resizing on the rare occasion a backend returns a slightly different size.
fn normalize_frame(frame: &RgbaImage, w: u32, h: u32) -> Cow<'_, RgbaImage> {
    if frame.width() == w && frame.height() == h {
        Cow::Borrowed(frame)
    } else {
        Cow::Owned(image::imageops::resize(frame, w, h, FilterType::Triangle))
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_ffmpeg(
    output_path: &PathBuf,
    capture_w: u32,
    capture_h: u32,
    out_w: u32,
    out_h: u32,
    fps: u32,
) -> Result<Child> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-loglevel",
        "error",
        "-f",
        "rawvideo",
        "-pixel_format",
        "rgba",
        "-video_size",
        &format!("{capture_w}x{capture_h}"),
        "-framerate",
        &fps.to_string(),
        "-i",
        "-",
    ]);
    if out_w != capture_w || out_h != capture_h {
        cmd.args(["-vf", &format!("scale={out_w}:{out_h}")]);
    }
    cmd.args([
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-movflags",
        "+faststart",
    ]);
    cmd.arg(output_path);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::inherit());
    cmd.spawn().context("failed to spawn ffmpeg")
}

/// Resolves the region to record: the whole (primary) screen with
/// `--fullscreen`, otherwise the usual slurp-geometry/positional/interactive
/// selection shared with the screenshot flow.
fn resolve_record_region(args: &Args) -> Result<Region> {
    if args.fullscreen {
        let outputs = crate::overlay::probe_output_rects().unwrap_or_default();
        let output = outputs
            .first()
            .ok_or_else(|| anyhow!("no outputs detected for --fullscreen"))?;
        return Ok(Region {
            raw: format!(
                "{},{} {}x{}",
                output.x, output.y, output.width, output.height
            ),
            x: output.x,
            y: output.y,
            w: output.width.max(0) as u32,
            h: output.height.max(0) as u32,
        });
    }

    match args.slurp_output() {
        Some(raw) if raw.trim() == "-" => {
            read_region_from_stdin().context("failed to read slurp selection from stdin")
        }
        Some(raw) => region_from_slurp_output(&raw).context("invalid slurp selection"),
        None => select_region().context("slurp selection failed"),
    }
}

/// Resolves the final `.mp4` output path, mirroring `output::save_image`'s
/// handling of directories/missing extensions but for video.
fn resolve_video_path(output_path: Option<PathBuf>) -> Result<PathBuf> {
    let path = match output_path {
        Some(p) => {
            if p.is_dir() {
                p.join(timestamped_video_filename())
            } else if p.extension().is_none() {
                p.with_extension("mp4")
            } else {
                p
            }
        }
        None => default_video_output_dir().join(timestamped_video_filename()),
    };
    Ok(path)
}

fn default_video_output_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join("Videos")
        .join("Wayutils-Screen")
}

fn timestamped_video_filename() -> String {
    format!("Wayutils-Screen-{}.mp4", Local::now().format("%Y%m%d-%H%M%S"))
}

fn which(cmd: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(cmd))
            .find(|full| full.is_file())
    })
}

/// Installs a minimal SIGINT handler that only flips an atomic flag (the one
/// thing that is safe to do from a signal handler), so Ctrl+C stops the
/// capture loop gracefully and lets ffmpeg finalize the file instead of the
/// whole process group dying mid-write.
fn install_sigint_handler() {
    unsafe {
        libc::signal(libc::SIGINT, handle_sigint as usize);
    }
}

/// In an interactive terminal, pressing Enter also stops the recording.
fn spawn_stdin_stop_watcher() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    if io::stdin().is_terminal() {
        let flag_clone = flag.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 1];
            let _ = io::stdin().read(&mut buf);
            flag_clone.store(true, Ordering::SeqCst);
        });
    }
    flag
}
