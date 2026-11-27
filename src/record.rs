//! `--record`: screen/region video recording.
//!
//! This deliberately keeps the Rust side minimal: we reuse the existing
//! screenshot [`Capturer`] backends to grab one RGBA frame at a time, and
//! hand raw frames to the external `ffmpeg` binary over a pipe for encoding.
//! No video-encoding crate is linked into the binary; `ffmpeg` is only
//! required (and only invoked) when the user actually asks to record.
//!
//! The recording is always written to a temporary file first, so discarding
//! it never touches the real output path. The on-screen toolbar provides
//! Save, Pause/Resume, and Quit controls; Enter or Ctrl+C in the terminal
//! that launched it, which still asks save-or-discard there; or losing the
//! capture region for too long (e.g. the output got disconnected), which
//! stops it automatically. Once `ffmpeg` finishes encoding, a desktop
//! notification is sent too, when `notify-send` is available.

use std::borrow::Cow;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use image::imageops::FilterType;
use image::RgbaImage;

use crate::capture::{read_region_from_stdin, region_from_slurp_output, select_region, Capturer};
use crate::cli::Args;
use crate::config::{Config, RecordMode};
use crate::region_overlay::RegionOverlay;
use crate::types::{RecordingCommand, Region};

/// Set by the SIGINT handler; polled from the capture loop.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_sig: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

/// Entry point for `--record`: resolves the region itself (slurp/positional/
/// `--fullscreen`, with `config.toml`'s `[recording] mode` as the default).
pub fn run(args: &Args, config: &Config) -> Result<()> {
    let region = resolve_record_region(args, config)?;
    run_with_region(args, config, region)
}

/// Entry point used when a region is already known - e.g. the screenshot
/// preview's "Record" button, which records the exact region the user just
/// selected for scrolling capture without prompting `slurp` a second time.
/// `config.toml`'s `[recording] mode = "fullscreen"` still overrides this,
/// same as it would for a fresh `--record` invocation.
pub fn run_region(args: &Args, config: &Config, region: Region) -> Result<()> {
    let region = if matches!(config.record_mode, RecordMode::Fullscreen) {
        fullscreen_region().unwrap_or(region)
    } else {
        region
    };
    run_with_region(args, config, region)
}

fn run_with_region(args: &Args, config: &Config, region: Region) -> Result<()> {
    if which("ffmpeg").is_none() {
        bail!(
            "ffmpeg was not found in PATH; video recording needs the `ffmpeg` binary installed \
             (only used as an external process, not linked into Wayutils-Screen)"
        );
    }

    log::info!(
        "Recording region: {},{} {}x{}",
        region.x,
        region.y,
        region.w,
        region.h
    );

    let capturer = Capturer::new(args.backend)?;
    let final_path = resolve_video_path(args.output.clone(), config.video_dir.as_deref())?;
    if let Some(parent) = final_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // Record to a scratch file first: if the user discards the recording
    // afterwards, the real output path is never touched.
    let temp_path = temp_recording_path(&final_path);

    // Visual indicator of the recorded area; best-effort only. When it's
    // available, its border strip is also clickable: click it, then Ctrl+S
    // stops and saves, Ctrl+Q stops and discards - no terminal required.
    let (mut border, recording_rx) = if args.no_border {
        (None, None)
    } else {
        match RegionOverlay::new_for_recording(region.clone()) {
            Ok((overlay, rx)) => (Some(overlay), Some(rx)),
            Err(err) => {
                log::warn!("recording border overlay unavailable: {err}");
                (None, None)
            }
        }
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
    let configured_resolution = args.resolution.or(config.record_resolution);
    let (mut out_w, mut out_h) = configured_resolution.unwrap_or((capture_w, capture_h));
    out_w = round_down_even(out_w).max(2);
    out_h = round_down_even(out_h).max(2);
    let fps = args.fps.or(config.record_fps).unwrap_or(12).max(1);
    let capture_audio = config.record_audio;

    let mut child = spawn_ffmpeg(
        &temp_path,
        capture_w,
        capture_h,
        out_w,
        out_h,
        fps,
        capture_audio,
    )?;
    let mut stdin = child
        .stdin
        .take()
        .context("failed to open ffmpeg's stdin pipe")?;

    install_sigint_handler();
    let stop_from_stdin = spawn_stdin_stop_watcher();

    eprintln!(
        "Recording {}x{} @ {}fps{} - use the on-screen Save / Pause / Quit controls, or press Enter/Ctrl+C here to stop and be asked",
        capture_w,
        capture_h,
        fps,
        if capture_audio { " (+ system audio)" } else { "" }
    );

    let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let mut frame = first_frame;
    let mut frame_count: u64 = 0;
    let started_at = Instant::now();
    let mut encode_failed = false;
    let mut capture_lost = false;
    let mut paused = false;
    // `Some(decision)` once the border's Ctrl+S/Ctrl+Q has already answered
    // the save/discard question - set below whenever `poll_stop` sees one of
    // those come in. Stopping via Enter/Ctrl+C or losing the capture region
    // leaves this `None`, which means "still ask" (see `prompt_save_or_discard`).
    let mut explicit_decision: Option<SaveDecision> = None;

    // A single failed grab (e.g. a brief hiccup from the compositor mid
    // workspace-switch animation) shouldn't end the whole recording: we
    // keep re-showing the last good frame and keep retrying for up to
    // roughly two seconds' worth of frames before giving up for real.
    let max_consecutive_capture_failures = (fps * 2).max(10);
    let mut consecutive_capture_failures: u32 = 0;

    loop {
        if let Some(decision) = poll_stop(&stop_from_stdin, recording_rx.as_ref(), &mut child, &mut paused) {
            explicit_decision = decision;
            break;
        }
        if paused {
            thread::sleep(Duration::from_millis(20));
            continue;
        }

        let tick_start = Instant::now();
        let normalized = normalize_frame(&frame, capture_w, capture_h);
        if stdin.write_all(normalized.as_raw()).is_err() {
            // A broken pipe right as we're already stopping (Ctrl+C/Enter)
            // just means ffmpeg got there first - not a real failure.
            if !should_stop(&stop_from_stdin) {
                log::warn!("ffmpeg stdin closed unexpectedly; stopping recording");
                encode_failed = true;
            }
            break;
        }
        frame_count += 1;

        let elapsed = tick_start.elapsed();
        if elapsed < frame_interval {
            thread::sleep(frame_interval - elapsed);
        }

        if let Some(decision) = poll_stop(&stop_from_stdin, recording_rx.as_ref(), &mut child, &mut paused) {
            explicit_decision = decision;
            break;
        }
        if paused {
            continue;
        }

        match capturer.capture(&region) {
            Ok(next) => {
                frame = next;
                consecutive_capture_failures = 0;
            }
            Err(err) => {
                consecutive_capture_failures += 1;
                log::warn!(
                    "frame capture failed ({consecutive_capture_failures}/{max_consecutive_capture_failures}): {err}"
                );
                if consecutive_capture_failures >= max_consecutive_capture_failures {
                    // Visible, not just logged: a log::warn! is invisible
                    // with the default log level, which is exactly how this
                    // used to fail silently - the recording would stop but
                    // nothing told you until you pressed Enter/Ctrl+C
                    // yourself and only then saw it finish immediately.
                    eprintln!(
                        "Lost the capture region for {consecutive_capture_failures} frames in a \
                         row ({err}); stopping the recording."
                    );
                    capture_lost = true;
                    break;
                }
                // Otherwise: keep the previous frame on screen and try
                // again next tick instead of ending the recording.
            }
        }
    }

    // Stop the on-screen selection rectangle the instant we've decided to
    // stop capturing, rather than after waiting on ffmpeg to finish
    // encoding below - the whole point is that it shouldn't look like
    // recording is still happening once it isn't.
    // A paused ffmpeg process must be continued before closing its stdin;
    // otherwise wait() below would block forever on a stopped child.
    if paused {
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGCONT); }
        paused = false;
    }
    if let Some(ref mut overlay) = border {
        overlay.stop();
    }
    if capture_lost {
        notify(
            "Wayutils-Screen",
            "Recording stopped: lost the capture region (e.g. output disconnected or a protocol hiccup)",
        );
    }

    // Closing stdin signals EOF to ffmpeg so it can finalize the container
    // (write the moov atom, flush the encoder, etc.) instead of leaving a
    // broken file behind.
    drop(stdin);
    let status = child.wait().context("failed waiting for ffmpeg to exit")?;

    if encode_failed || !status.success() {
        let _ = std::fs::remove_file(&temp_path);
        notify("Wayutils-Screen", "Recording failed while encoding");
        bail!("ffmpeg exited with an error while encoding the recording");
    }

    let secs = started_at.elapsed().as_secs_f64();
    notify(
        "Wayutils-Screen",
        &format!("Finished encoding ({secs:.1}s, {frame_count} frames)"),
    );

    let decision =
        explicit_decision.unwrap_or_else(|| prompt_save_or_discard(&temp_path, &final_path));
    match decision {
        SaveDecision::Save => {
            move_file(&temp_path, &final_path)?;
            eprintln!(
                "Saved ~{secs:.1}s ({frame_count} frames) to {}",
                final_path.display()
            );
            notify(
                "Wayutils-Screen",
                &format!("Recording saved to {}", final_path.display()),
            );
        }
        SaveDecision::Discard => {
            let _ = std::fs::remove_file(&temp_path);
            eprintln!("Recording discarded.");
            notify("Wayutils-Screen", "Recording discarded");
        }
    }

    Ok(())
}

enum SaveDecision {
    Save,
    Discard,
}

/// Shows the save/discard choice after encoding finishes. Mirrors the `s`
/// save / `q` quit convention used by the screenshot session's own
/// control-bar/terminal fallback. In a non-interactive context (no TTY,
/// piped stdin, or a closed/errored stream) the recording is kept rather
/// than silently thrown away.
fn prompt_save_or_discard(temp_path: &Path, final_path: &Path) -> SaveDecision {
    if !io::stdin().is_terminal() {
        log::info!(
            "non-interactive session: keeping the recording at {}",
            final_path.display()
        );
        return SaveDecision::Save;
    }

    eprintln!(
        "Recording stopped ({}). Save it to {}? [s = save / q or d = discard, then Enter] ",
        temp_path.display(),
        final_path.display()
    );

    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => SaveDecision::Save, // stdin closed/unreadable: don't lose the recording
        Ok(_) => match line.trim() {
            "q" | "Q" | "d" | "D" => SaveDecision::Discard,
            _ => SaveDecision::Save,
        },
    }
}

/// Moves the temp recording into place, falling back to copy+remove when the
/// temp file and destination are on different filesystems (`rename(2)` can't
/// cross mount points).
fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)
        .with_context(|| format!("failed to move recording to {}", to.display()))?;
    let _ = std::fs::remove_file(from);
    Ok(())
}

/// Best-effort desktop notification; silently does nothing if `notify-send`
/// isn't installed (not every Wayland desktop ships it).
fn notify(summary: &str, body: &str) {
    if which("notify-send").is_none() {
        return;
    }
    if let Err(err) = Command::new("notify-send").arg(summary).arg(body).status() {
        log::debug!("notify-send failed: {err}");
    }
}

fn temp_recording_path(final_path: &Path) -> PathBuf {
    let ext = final_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mp4");
    std::env::temp_dir().join(format!(
        "wayutils-screen-recording-{}.{ext}",
        std::process::id()
    ))
}

/// Rounds a dimension down to the nearest even number (libx264 + yuv420p
/// require both width and height to be divisible by 2).
fn round_down_even(v: u32) -> u32 {
    v & !1
}

fn should_stop(stop_from_stdin: &Arc<AtomicBool>) -> bool {
    STOP_REQUESTED.load(Ordering::SeqCst) || stop_from_stdin.load(Ordering::SeqCst)
}

/// Polls every way the recording can be asked to stop. Returns `None` while
/// recording should continue. Returns `Some(Some(decision))` once the
/// border's Ctrl+S/Ctrl+Q has already answered the save/discard question -
/// or `Some(None)` when the stop instead came from Enter, Ctrl+C, so the
/// save/discard question still needs asking (see `prompt_save_or_discard`).
fn poll_stop(
    stop_from_stdin: &Arc<AtomicBool>,
    recording_rx: Option<&mpsc::Receiver<RecordingCommand>>,
    child: &mut Child,
    paused: &mut bool,
) -> Option<Option<SaveDecision>> {
    if let Some(rx) = recording_rx {
        loop {
            match rx.try_recv() {
                Ok(RecordingCommand::Save) => return Some(Some(SaveDecision::Save)),
                Ok(RecordingCommand::Discard) => return Some(Some(SaveDecision::Discard)),
                Ok(RecordingCommand::TogglePause) => {
                    let signal = if *paused { libc::SIGCONT } else { libc::SIGSTOP };
                    if unsafe { libc::kill(child.id() as libc::pid_t, signal) } == 0 {
                        *paused = !*paused;
                    } else {
                        log::warn!("failed to {} recording process: {}", if *paused { "resume" } else { "pause" }, io::Error::last_os_error());
                        *paused = false;
                    }
                }
                Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
    }
    if should_stop(stop_from_stdin) {
        return Some(None);
    }
    None
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
    output_path: &Path,
    capture_w: u32,
    capture_h: u32,
    out_w: u32,
    out_h: u32,
    fps: u32,
    capture_audio: bool,
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
    if capture_audio {
        // PulseAudio's "default" source also works against PipeWire desktops
        // through pipewire-pulse, so this covers both common setups without
        // branching on which sound server is running.
        cmd.args([
            "-f",
            "pulse",
            "-use_wallclock_as_timestamps",
            "1",
            "-i",
            "default",
        ]);
    }
    if out_w != capture_w || out_h != capture_h {
        cmd.args(["-vf", &format!("scale={out_w}:{out_h}")]);
    }
    cmd.args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"]);
    if capture_audio {
        cmd.args(["-c:a", "aac", "-b:a", "128k", "-shortest"]);
    }
    cmd.args(["-movflags", "+faststart"]);
    cmd.arg(output_path);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::inherit());
    // Its own process group: the terminal delivers Ctrl+C's SIGINT to the
    // whole foreground process group, so without this ffmpeg receives the
    // signal directly (at the same moment we do) and - by ffmpeg's own
    // design - reports a non-zero "Exiting normally, received signal 2"
    // exit code even when the file it wrote is perfectly valid. Isolating
    // it means only *we* get the signal; we then stop feeding frames and
    // close its stdin ourselves, giving ffmpeg the same clean EOF-triggered
    // shutdown (exit code 0) as pressing Enter already did.
    cmd.process_group(0);
    cmd.spawn().context("failed to spawn ffmpeg")
}

/// Resolves the region to record: the whole (primary) screen with
/// `--fullscreen` or `config.toml`'s `[recording] mode = "fullscreen"`,
/// otherwise the usual slurp-geometry/positional/interactive selection
/// shared with the screenshot flow.
fn resolve_record_region(args: &Args, config: &Config) -> Result<Region> {
    if args.fullscreen || matches!(config.record_mode, RecordMode::Fullscreen) {
        return fullscreen_region()
            .ok_or_else(|| anyhow!("no outputs detected for fullscreen recording"));
    }

    match args.slurp_output() {
        Some(raw) if raw.trim() == "-" => {
            read_region_from_stdin().context("failed to read slurp selection from stdin")
        }
        Some(raw) => region_from_slurp_output(&raw).context("invalid slurp selection"),
        None => select_region().context("slurp selection failed"),
    }
}

fn fullscreen_region() -> Option<Region> {
    let outputs = crate::overlay::probe_output_rects().unwrap_or_default();
    let output = outputs.first()?;
    Some(Region {
        raw: format!(
            "{},{} {}x{}",
            output.x, output.y, output.width, output.height
        ),
        x: output.x,
        y: output.y,
        w: output.width.max(0) as u32,
        h: output.height.max(0) as u32,
    })
}

/// Resolves the final `.mp4` output path, mirroring `output::save_image`'s
/// handling of directories/missing extensions but for video.
///
/// `configured_dir` is the `[paths] video_dir` from `config.toml`, used only
/// when `output_path` is `None`.
fn resolve_video_path(output_path: Option<PathBuf>, configured_dir: Option<&Path>) -> Result<PathBuf> {
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
        None => {
            let dir = configured_dir
                .map(Path::to_path_buf)
                .unwrap_or_else(default_video_output_dir);
            dir.join(timestamped_video_filename())
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_video_path_prefers_configured_dir_over_builtin_default() {
        let configured = PathBuf::from("/tmp/wss-test-video-dir");
        let path = resolve_video_path(None, Some(&configured)).unwrap();
        assert_eq!(path.parent(), Some(configured.as_path()));
    }

    #[test]
    fn resolve_video_path_explicit_output_wins_over_config() {
        let configured = PathBuf::from("/tmp/wss-test-video-dir");
        let explicit = PathBuf::from("/tmp/explicit-output.mp4");
        let path = resolve_video_path(Some(explicit.clone()), Some(&configured)).unwrap();
        assert_eq!(path, explicit);
    }
}
