<div align="center">

<h1>Wayutils-Screen</h1>

<p><strong>A scrolling screenshot tool for Wayland.</strong></p>

<p>
Capture a screen region as you scroll, then stitch the frames into one long image.<br>
Includes a live preview, image editor and optional video recording.
</p>

<p>
<img alt="Platform" src="https://img.shields.io/badge/platform-Linux%20x86--64-1f2328?style=flat-square">
<img alt="Display server" src="https://img.shields.io/badge/display-Wayland-1f2328?style=flat-square">
<img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-1f2328?style=flat-square">
<img alt="License" src="https://img.shields.io/badge/license-see%20LICENSE-1f2328?style=flat-square">
</p>

<p>
<a href="#features">Features</a> &nbsp;·&nbsp;
<a href="#download-and-install">Download</a> &nbsp;·&nbsp;
<a href="#usage">Usage</a> &nbsp;·&nbsp;
<a href="#command-line-reference">CLI reference</a> &nbsp;·&nbsp;
<a href="#build-from-source">Build</a>
</p>

</div>

---

## Overview

Wayutils-Screen captures a selected region of a Wayland desktop while its contents are scrolled. It aligns successive frames and combines them into a single tall screenshot.

The application includes a preview and control overlay, several stitching algorithms, an editor for existing screenshots, and a video-recording mode. Capture is handled through desktop-specific backends: `grim` for wlroots compositors, KWin's screenshot D-Bus interface for KDE Plasma, and a bundled GNOME Shell extension for GNOME.

Wayutils-Screen is designed for **Wayland sessions**. It is not an X11 screenshot utility.

<table>
<tr>
<td width="33%" valign="top">

**Scrolling capture**<br>
Select a region once, scroll through the content, and combine the captured frames into one image.

</td>
<td width="33%" valign="top">

**Flexible capture**<br>
Choose the capture backend and stitching algorithm, or let the application detect the desktop environment.

</td>
<td width="33%" valign="top">

**More than screenshots**<br>
Copy results to the clipboard, edit existing images, or record a region as video.

</td>
</tr>
</table>

---

## Features

### Scrolling screenshots

- Select a screen region interactively, or provide existing `slurp` geometry.
- Capture successive frames while the selected content is scrolled.
- Stitch frames into one long screenshot.
- Show a live preview and control strip where the desktop supports the overlay.
- Save as PNG or JPEG, or copy the finished image to the clipboard.
- Disable the preview and automatically finish after a period without scroll updates.
- Optionally disable the region-border overlay.

### Stitching algorithms

Six stitching strategies are available:

| Algorithm | Method | Intended use |
|---|---|---|
| `opencv-orb` | OpenCV ORB features, brute-force matching and affine RANSAC | Default feature-based matching |
| `fast` | FAST corners with an HNSW index | High-accuracy matching |
| `template` | Template matching | Slower matching for suitable content |
| `col-sample` | Column sampling | Fast general-purpose stitching |
| `edge` | Edge detection | Content with transparent backgrounds |
| `band` | Eight-band signature matching | Ignores static sidebars and fixed header/footer bars |

The default algorithm is `opencv-orb`. Results depend on the content being captured; if one method does not align a page correctly, try another.

### Capture backends

| Backend | Desktop environment | Capture method |
|---|---|---|
| `auto` | Automatic detection | Uses KWin on KDE, otherwise the `grim` backend |
| `grim` | Sway, Hyprland, niri, river and other wlroots compositors | `grim` / wlr-screencopy |
| `kwin` | KDE Plasma | `org.kde.KWin.ScreenShot2` D-Bus interface |
| `gnome` | GNOME Shell | Bundled GNOME Shell scrolling-capture extension |

### Preview and controls

When the preview overlay is available, its control bar has six buttons:

- **Save** — write the stitched image to the selected output path.
- **Copy** — copy the image to the clipboard.
- **Edit** — finish the capture and open the result directly in the built-in editor.
- **Record** — finish the capture and immediately start recording the same region as a video.
- **Pause / Resume** — temporarily stop or continue capture.
- **Quit** — stop the current capture without saving.

Keyboard shortcuts work the same overlay: **Ctrl+S** saves, **Ctrl+C** copies, **E** jumps to the editor, **R** jumps to recording, **Space** pauses/resumes, and **Q** quits.

If the preview cannot be created, fallback controls may be provided by the GNOME helper or an interactive terminal (type `s`, `c`, `p` or `q` and press Enter).

### Image editor

Open a screenshot in the built-in doodle and crop editor:

- The rest of the screen dims behind the image while you edit, so it's visually obvious you're in a modal edit session rather than looking at an unchanged desktop.
- Draw over the image with the mouse.
- Change brush size and doodle colour.
- Switch between doodle and crop modes with **C**.
- Apply a selected crop rectangle with **Enter**.
- **Ctrl+S** saves to the original file and exits.
- **Ctrl+C** copies the current image to the clipboard and exits.
- **Q** quits without saving or copying.
- Open the most recently saved screenshot by running `--edit` without a path.

### Video recording

- Record a selected screen region, the full screen, or (from the preview's **Record** button) the exact region a scrolling capture just used.
- Set the output frame rate and resolution, or leave both to `config.toml`'s `[recording]` defaults.
- Optionally capture system audio (`[recording] audio = true` in `config.toml`) and mux it into the video.
- Encode MP4 video through the external `ffmpeg` executable.
- Stop recording with Enter or Ctrl+C. Once `ffmpeg` finishes encoding, a desktop notification is sent (when `notify-send` is available) and you're asked whether to save or discard the result — the recording is written to a scratch file first, so discarding never touches the real output path.
- A brief capture hiccup (e.g. a workspace-switch animation) no longer silently ends the recording — it keeps retrying for a couple of seconds before giving up, and if it does have to stop early you get a clear on-screen message and notification instead of the selection rectangle just lingering with no explanation.

Video recording is optional and requires `ffmpeg`; it is not linked into the application.

### Configuration file

Settings that rarely change can live in `~/.config/Wayutils-Screen/config.toml` (or `$XDG_CONFIG_HOME/Wayutils-Screen/config.toml`), created with a commented-out template the first time the app runs. Command-line flags always override it.

```toml
[paths]
image_dir = "~/Pictures/Wayutils-Screen"   # default save location for screenshots
video_dir = "~/Videos/Wayutils-Screen"     # default save location for recordings

[recording]
mode = "region"          # "region" or "fullscreen" - fullscreen wins even over a
                          # manually selected region, e.g. from the preview's Record button
resolution = "1920x1080" # default --resolution when --record omits it
fps = 12                 # default --fps when --record omits it
audio = false             # capture system audio (PulseAudio/PipeWire) into recordings
```

---

## Download and Install

### Arch Linux x86_64 binary

Download **`Wayutils-Screen-archlinux-x86_64.zip`** from the [GitHub Releases](../../releases) page.

The archive contains:

```text
Wayutils-Screen
Wayutils-Screen.desktop
wayutils-screen.svg
README.md
LICENSE
PACKAGE-NOTICE.txt
gnome-extension/
```

The archive name includes the platform and architecture. The executable inside is simply named **`Wayutils-Screen`**.

Extract the archive:

```bash
bsdtar -xf Wayutils-Screen-archlinux-x86_64.zip
```

Install the executable, desktop launcher and application icon:

```bash
sudo install -Dm755 Wayutils-Screen /usr/local/bin/Wayutils-Screen
sudo install -Dm644 Wayutils-Screen.desktop \
  /usr/local/share/applications/Wayutils-Screen.desktop
sudo install -Dm644 wayutils-screen.svg \
  /usr/local/share/icons/hicolor/scalable/apps/wayutils-screen.svg
```

Refresh the desktop and icon caches if your desktop environment does not show the launcher immediately. You can then start the application from the application menu or run:

```bash
Wayutils-Screen
```

### Runtime requirements

The supplied release is a dynamically linked **Arch Linux x86_64** executable. It is not a self-contained portable binary.

The release build requires the matching system libraries, including:

- GLIBC 2.43 or newer.
- OpenCV 5.0.1 shared libraries (`libopencv_*.so.501`).
- `libxkbcommon`.
- The standard C/C++ runtime libraries.

On a current Arch Linux installation, install the runtime libraries with:

```bash
sudo pacman -S --needed glibc gcc-libs libxkbcommon opencv
```

The binary may not run on older Arch installations or other distributions whose GLIBC or OpenCV shared-library ABI is older or incompatible. The executable is built for x86_64.

### Desktop capture tools

For wlroots-based desktops such as Hyprland, Sway, niri and river:

```bash
sudo pacman -S --needed grim slurp
```

- `slurp` selects the capture region when geometry is not supplied.
- `grim` captures frames through the wlroots screenshot protocol.

KDE Plasma uses KWin's `org.kde.KWin.ScreenShot2` D-Bus interface. GNOME uses the bundled extension described below.

### Optional tools

| Package / executable | Purpose |
|---|---|
| `wl-clipboard` (`wl-copy`) | Wayland clipboard support |
| `xclip` | Clipboard fallback when `wl-copy` is unavailable |
| `ffmpeg` | Required only for `--record`; encodes MP4 using `libx264` |

Install the optional tools on Arch Linux as needed:

```bash
sudo pacman -S --needed wl-clipboard ffmpeg
```

---

## GNOME Shell Extension

The GNOME capture backend requires the bundled extension:

`Wayutils-Screen-scroll-helper@snemc.org`

From the extracted release archive, install it system-wide:

```bash
sudo install -d \
  /usr/share/gnome-shell/extensions/Wayutils-Screen-scroll-helper@snemc.org

sudo install -m644 \
  gnome-extension/Wayutils-Screen-scroll-helper@snemc.org/metadata.json \
  /usr/share/gnome-shell/extensions/Wayutils-Screen-scroll-helper@snemc.org/metadata.json

sudo install -m644 \
  gnome-extension/Wayutils-Screen-scroll-helper@snemc.org/extension.js \
  /usr/share/gnome-shell/extensions/Wayutils-Screen-scroll-helper@snemc.org/extension.js

gnome-extensions enable Wayutils-Screen-scroll-helper@snemc.org
```

Log out and back in if GNOME does not load the extension immediately. The extension must be enabled when using `--backend gnome` or automatic capture on GNOME.

---

## Usage

### Start a scrolling screenshot

```bash
Wayutils-Screen
```

Select a region, then scroll the page or application content. The preview updates as frames are captured. Use the preview controls to save, copy, pause/resume or cancel.

### Choose an output file

```bash
Wayutils-Screen --output "$HOME/Pictures/long-page.png"
```

PNG is used by default. A `.jpg` or `.jpeg` extension selects JPEG. An existing directory receives a timestamped PNG.

### Copy to the clipboard

```bash
Wayutils-Screen --clipboard
```

### Supply a region directly

Pass existing `slurp` geometry instead of opening the region selector:

```bash
Wayutils-Screen '120,180 900x700'
```

Geometry can also be read from standard input:

```bash
printf '%s\n' '120,180 900x700' | Wayutils-Screen -
```

### Run without the preview

```bash
Wayutils-Screen --no-preview
```

Without an explicit duration, the application automatically saves (or copies when `--clipboard` is active) after two seconds without scroll updates.

Set a different idle timeout:

```bash
Wayutils-Screen --no-preview --auto-save-after-idle 1500ms
```

### Select a backend or algorithm

```bash
Wayutils-Screen --backend grim --algorithm opencv-orb
```

For KDE Plasma:

```bash
Wayutils-Screen --backend kwin
```

For GNOME:

```bash
Wayutils-Screen --backend gnome
```

### Edit an existing screenshot

```bash
Wayutils-Screen screenshot.png --edit
```

Open the most recently saved screenshot:

```bash
Wayutils-Screen --edit
```

### Record video

Record a selected region:

```bash
Wayutils-Screen --record
```

Record the full screen at 1280×720 and 30 FPS:

```bash
Wayutils-Screen --record --fullscreen --resolution 1280x720 --fps 30
```

Press **Enter** in an interactive terminal or **Ctrl+C** to stop recording. By default, recordings are written under `~/Videos/Wayutils-Screen/`.

---

## Command-Line Reference

```text
Usage: Wayutils-Screen [OPTIONS] [REGION]...

Arguments:
  [REGION]...
      Existing slurp geometry to use instead of selecting a region.
      Use '-' to read from stdin.
      With --edit, this positional argument is the screenshot path.

Options:
  -o, --output <OUTPUT>
      Output file path (default: ~/Pictures/Wayutils-Screen/Wayutils-Screen-<timestamp>.png).
      .png and .jpg/.jpeg select the format. An unknown or missing extension
      is written as PNG to exactly that path. An existing directory receives
      a timestamped PNG.

  -w, --preview-width <PREVIEW_WIDTH>
      Preview width in pixels [default: 280].

  -c, --clipboard
      Copy to clipboard instead of saving to a file.

      --no-preview
      Disable the preview window. The session auto-saves after the idle timeout.

      --auto-save-after-idle <DURATION>
      Finish after this duration without scroll updates (e.g. 2s, 1500ms, 2.5).

      --no-border
      Disable the region border overlay.

  -a, --algorithm <ALGORITHM>
      Stitching algorithm [default: opencv-orb].
      Values: col-sample, template, edge, fast, opencv-orb, band.

      --backend <BACKEND>
      Capture backend [default: auto].
      Values: auto, grim, kwin, gnome.

      --edit
      Open an existing screenshot in the doodle/crop editor.
      Usage: Wayutils-Screen <screenshot> --edit
      Without a path, opens the most recently saved screenshot.

      --record
      Record a video instead of taking a stitched screenshot (requires ffmpeg).

      --fullscreen
      With --record, capture the whole screen instead of selecting a region.
      config.toml's [recording] mode = "fullscreen" has the same effect by
      default, and wins even over a manually selected region.

      --resolution <WxH>
      With --record, scale the output video to this resolution (e.g. 1280x720).
      Falls back to config.toml's [recording] resolution, then to the
      captured region's own size.

      --fps <FPS>
      With --record, set the recording frame rate. Falls back to
      config.toml's [recording] fps, then to 12.

  -h, --help
      Print help.
```

For the exact options supported by the installed executable, run:

```bash
Wayutils-Screen --help
```

---

## Editor Controls

| Input | Action |
|---|---|
| Left mouse button + drag | Draw, or select a crop rectangle in crop mode |
| `Up` / `Down` | Increase / decrease brush size |
| `Left` / `Right` | Previous / next doodle colour |
| `c` | Toggle doodle/crop mode |
| `Enter` | Apply the pending crop rectangle |
| `Esc` | Cancel the pending crop, or quit if no crop is pending |
| `s` | Save edits to the original file and quit |
| `q` | Quit without saving |

## Preview Fallback Controls

If the layer-shell preview cannot be created, the application may use GNOME extension controls or terminal input instead.

In an interactive terminal, enter one command per line:

| Key | Action |
|---|---|
| `s` | Save the stitched image |
| `c` | Copy the image to the clipboard |
| `p` | Pause or resume capture |
| `q` | Cancel capture |

With `--no-preview`, the idle timeout is enabled by default (two seconds) unless another duration is supplied.

---

## Build from Source

### Build dependencies on Arch Linux

```bash
sudo pacman -S --needed base-devel clang git libxkbcommon opencv pkgconf rust
```

The project uses Rust and the OpenCV 5 Rust bindings, together with the system OpenCV development libraries.

### Build and test

Clone the repository, then run:

```bash
cargo test --locked
cargo build --release --locked
```

### Install for the current user

`./install.sh` builds the release binary (unless given `--no-build`) and installs it for the current user - no `sudo` required:

```bash
./install.sh
```

This installs:

| What | Where |
|---|---|
| Executable | `~/.local/bin/Wayutils-Screen` |
| Desktop launcher | `~/.local/share/applications/Wayutils-Screen.desktop` |
| Application icon | `~/.local/share/icons/hicolor/scalable/apps/wayutils-screen.svg` |
| Default config (only if missing) | `~/.config/Wayutils-Screen/config.toml` |

If `~/.local/bin` isn't already on your `PATH`, the script prints the line to add to your shell profile. Launch from your application menu afterwards, or run `Wayutils-Screen` directly.

To remove everything `install.sh` put in place:

```bash
./uninstall.sh                 # keeps config.toml and any saved screenshots/recordings
./uninstall.sh --purge-config  # also deletes ~/.config/Wayutils-Screen
```

The release GitHub Actions workflow builds in an up-to-date Arch Linux environment and publishes:

`Wayutils-Screen-archlinux-x86_64.zip`

The archive uses the platform suffix in its filename; the executable inside remains `Wayutils-Screen`.

### Project layout

```text
assets/
  wayutils-screen.svg       Application launcher icon
  icons/                    Toolbar SVG artwork and embedded PNGs (incl. edit/record)

packaging/
  gnome-extension/          GNOME Shell scrolling-capture helper

scripts/
  package-deb.sh            Debian package builder
  update-aur-bin.sh         AUR binary package metadata updater

src/
  capture/                  Desktop capture backends
  overlay/                  Preview overlay and controls
  stitch/                   Frame-alignment implementations
  cli.rs                    Command-line arguments
  config.rs                 ~/.config/Wayutils-Screen/config.toml loader
  editor.rs                 Doodle and crop editor
  record.rs                 Video recording through ffmpeg

install.sh                  User-local install (./install.sh)
uninstall.sh                Removes what install.sh installed
```

Toolbar PNG assets are embedded into the executable at compile time. The original SVG artwork remains in `assets/icons/`. The application icon is installed through the standard `hicolor` icon theme.

---

## Current Limitations and Compatibility

- Wayland sessions are supported; X11-only desktops are not the target environment.
- The supplied binary is built for Arch Linux x86_64 and depends on the system GLIBC and OpenCV shared-library ABI.
- The GNOME backend requires the bundled GNOME Shell extension to be installed and enabled.
- Video recording requires `ffmpeg`.
- Clipboard support requires `wl-copy` or the supported `xclip` fallback.
- Capture and stitching quality vary with page movement, animations, fixed elements and the selected algorithm.

---

## License

See [`LICENSE`](LICENSE) for the project's license terms.

<div align="center">
<br>
<sub>Wayutils-Screen — scrolling screenshots for Wayland.</sub>
</div>
