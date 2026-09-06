#!/usr/bin/env bash
# Install verterm for the current user. No sudo, no system paths.
#
#   ./install.sh              build --release, strip, install to ~/.local/bin
#   ./install.sh --no-build   install the existing release binary
#   PREFIX=~/opt ./install.sh install to $PREFIX/bin instead
#
# Run this from a HOST shell. Inside the distrobox $HOME is the container's, so the
# binary would land where the desktop session cannot see it.
set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
BINDIR="$PREFIX/bin"
APPDIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
ICONDIR="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

build=1
[[ "${1:-}" == "--no-build" ]] && build=0

# Honour .cargo/config.toml's target-dir, which lives outside the repo.
target_dir="$(
  sed -n 's/^[[:space:]]*target-dir[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' \
    "$REPO/.cargo/config.toml" 2>/dev/null | head -1
)"
BIN="${target_dir:-$REPO/target}/release/verterm"

if (( build )); then
  command -v cargo >/dev/null || {
    echo "error: cargo not on PATH. Try: export PATH=\"\$HOME/.cargo/bin:\$PATH\"" >&2
    exit 1
  }
  echo ">> cargo build --release"
  ( cd "$REPO" && cargo build --release )
fi

[[ -x "$BIN" ]] || { echo "error: no release binary at $BIN" >&2; exit 1; }

install -Dm755 "$BIN" "$BINDIR/verterm"
# The release profile keeps debuginfo (debug = 1): ~142M -> ~23M installed.
strip "$BINDIR/verterm" 2>/dev/null || true
install -Dm644 "$REPO/verterm.desktop" "$APPDIR/verterm.desktop"
# On Wayland the window icon is resolved from the app_id (`verterm`) to this desktop entry and
# then to `Icon=`; `with_icon()` at runtime is an X11 path. Installing the themed icon is what
# actually gives the app an icon in Niri, the app launcher and the alt-tab switcher.
install -Dm644 "$REPO/assets/verterm.svg" "$ICONDIR/verterm.svg"
command -v update-desktop-database >/dev/null && update-desktop-database "$APPDIR" || true
command -v gtk-update-icon-cache >/dev/null \
  && gtk-update-icon-cache -qtf "${ICONDIR%/scalable/apps}" 2>/dev/null || true

echo ">> installed $BINDIR/verterm ($(du -h "$BINDIR/verterm" | cut -f1))"
echo ">> installed $APPDIR/verterm.desktop"
echo ">> installed $ICONDIR/verterm.svg"

case ":$PATH:" in
  *":$BINDIR:"*) ;;
  *) echo ">> NOTE: $BINDIR is not on your PATH; add it to your shell profile." ;;
esac

# Shell-integration scripts are embedded in the binary and written to
# $XDG_DATA_HOME/verterm/shell-integration on first run -- nothing to install here.
echo ">> done. Run: verterm"
