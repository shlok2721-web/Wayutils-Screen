#!/usr/bin/env bash
#
# uninstall.sh - removes everything ./install.sh put in place for the
# current user: the binary, desktop entry and icon. Leaves
# ~/.config/Wayutils-Screen/config.toml and any saved screenshots/recordings
# alone unless you pass --purge-config.
#
# Usage:
#   ./uninstall.sh
#   ./uninstall.sh --purge-config   # also delete config.toml and its folder
#
set -euo pipefail

BIN_NAME="Wayutils-Screen"
BIN_DST="${HOME}/.local/bin/${BIN_NAME}"
DESKTOP_DST="${HOME}/.local/share/applications/${BIN_NAME}.desktop"
ICON_DST="${HOME}/.local/share/icons/hicolor/scalable/apps/wayutils-screen.svg"
CONFIG_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/Wayutils-Screen"

PURGE_CONFIG=0
for arg in "$@"; do
  case "$arg" in
    --purge-config) PURGE_CONFIG=1 ;;
    -h|--help)
      sed -n '2,10p' "$0"
      exit 0
      ;;
    *)
      echo "uninstall.sh: unknown argument: $arg" >&2
      exit 1
      ;;
  esac
done

remove() {
  local path="$1"
  if [ -e "$path" ] || [ -L "$path" ]; then
    rm -f "$path"
    echo "removed $path"
  fi
}

remove "$BIN_DST"
remove "$DESKTOP_DST"
remove "$ICON_DST"

if [ "$PURGE_CONFIG" -eq 1 ]; then
  if [ -d "$CONFIG_DIR" ]; then
    rm -rf "$CONFIG_DIR"
    echo "removed $CONFIG_DIR"
  fi
else
  if [ -d "$CONFIG_DIR" ]; then
    echo "kept $CONFIG_DIR (re-run with --purge-config to remove it too)"
  fi
fi

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "${HOME}/.local/share/applications" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -f "${HOME}/.local/share/icons/hicolor" >/dev/null 2>&1 || true
fi

echo "Done."
