#!/usr/bin/env bash
#
# install.sh - builds Wayutils-Screen and installs it for the current user.
#
# Installs (no root/sudo required):
#   ~/.local/bin/Wayutils-Screen
#   ~/.local/share/applications/Wayutils-Screen.desktop
#   ~/.local/share/icons/hicolor/scalable/apps/wayutils-screen.svg
#   ~/.config/Wayutils-Screen/config.toml   (only if it doesn't already exist)
#
# Usage:
#   ./install.sh            # cargo build --release, then install
#   ./install.sh --no-build # skip the build; install the binary already at
#                            # target/release/Wayutils-Screen
#
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
cd "$SCRIPT_DIR"

BIN_NAME="Wayutils-Screen"
BIN_SRC="target/release/${BIN_NAME}"
BIN_DST_DIR="${HOME}/.local/bin"
DESKTOP_DST_DIR="${HOME}/.local/share/applications"
ICON_DST_DIR="${HOME}/.local/share/icons/hicolor/scalable/apps"
CONFIG_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/Wayutils-Screen"

SKIP_BUILD=0
for arg in "$@"; do
  case "$arg" in
    --no-build) SKIP_BUILD=1 ;;
    -h|--help)
      sed -n '2,15p' "$0"
      exit 0
      ;;
    *)
      echo "install.sh: unknown argument: $arg" >&2
      exit 1
      ;;
  esac
done

if [ "$SKIP_BUILD" -eq 0 ]; then
  if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo was not found in PATH. Install Rust (see README.md's" >&2
    echo "'Build from Source' section), then re-run ./install.sh." >&2
    exit 1
  fi
  echo "==> Building ${BIN_NAME} (cargo build --release)..."
  cargo build --release --locked
fi

if [ ! -x "$BIN_SRC" ]; then
  echo "error: ${BIN_SRC} not found. Run without --no-build, or build it yourself first." >&2
  exit 1
fi

echo "==> Installing binary to ${BIN_DST_DIR}/${BIN_NAME}"
install -Dm755 "$BIN_SRC" "${BIN_DST_DIR}/${BIN_NAME}"

ICON_SRC="assets/wayutils-screen.svg"
if [ -f "$ICON_SRC" ]; then
  echo "==> Installing icon to ${ICON_DST_DIR}/wayutils-screen.svg"
  install -Dm644 "$ICON_SRC" "${ICON_DST_DIR}/wayutils-screen.svg"
else
  echo "warning: ${ICON_SRC} not found; skipping icon install" >&2
fi

echo "==> Installing desktop entry to ${DESKTOP_DST_DIR}/${BIN_NAME}.desktop"
mkdir -p "$DESKTOP_DST_DIR"
# Exec uses the absolute install path rather than a bare command name, since
# not every desktop environment puts ~/.local/bin on the PATH used to launch
# .desktop entries.
sed "s|^Exec=.*|Exec=${BIN_DST_DIR}/${BIN_NAME}|" "${BIN_NAME}.desktop" \
  > "${DESKTOP_DST_DIR}/${BIN_NAME}.desktop"
chmod 644 "${DESKTOP_DST_DIR}/${BIN_NAME}.desktop"

if [ ! -f "${CONFIG_DIR}/config.toml" ]; then
  echo "==> Writing default config to ${CONFIG_DIR}/config.toml"
  mkdir -p "$CONFIG_DIR"
  cat > "${CONFIG_DIR}/config.toml" <<'EOF'
# Wayutils-Screen configuration.
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
EOF
else
  echo "==> Keeping existing config at ${CONFIG_DIR}/config.toml"
fi

case ":$PATH:" in
  *":${BIN_DST_DIR}:"*) ;;
  *)
    echo
    echo "note: ${BIN_DST_DIR} is not on your PATH. Add it (e.g. in ~/.bashrc" \
         "or ~/.profile) to run '${BIN_NAME}' directly from a terminal:"
    echo "    export PATH=\"${BIN_DST_DIR}:\$PATH\""
    ;;
esac

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$DESKTOP_DST_DIR" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -f "${HOME}/.local/share/icons/hicolor" >/dev/null 2>&1 || true
fi

echo
echo "==> Done. Launch from your application menu, or run:"
echo "    ${BIN_DST_DIR}/${BIN_NAME}"
echo "Edit ${CONFIG_DIR}/config.toml to set default save paths, recording"
echo "resolution, mode (region/fullscreen) and audio capture."
echo "To remove everything this script installed, run ./uninstall.sh"
