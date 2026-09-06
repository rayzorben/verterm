#!/usr/bin/env bash
# Assemble the release payload: one stripped binary plus its desktop-integration
# files, laid out under bin/ and share/ so that
#
#   tar -xzf verterm-<ver>-x86_64-linux.tar.gz -C ~/.local --strip-components=1
#
# is a complete per-user install, and every package format (deb/rpm/arch/AppImage)
# can drop the same two directories under its own /usr.
#
#   packaging/build-payload.sh <binary> <version> <homepage-url> [outdir]
#
# Prints the tarball path on stdout. Run from the repository root.
set -euo pipefail

bin="${1:?usage: build-payload.sh <binary> <version> <url> [outdir]}"
version="${2:?missing version}"
url="${3:?missing url}"
outdir="${4:-dist}"

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
prefix="verterm-${version}-x86_64-linux"

[[ -x "$bin" ]] || { echo "error: no executable at $bin" >&2; exit 1; }

mkdir -p "$outdir"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
root="$stage/$prefix"

install -Dm755 "$bin" "$root/bin/verterm"
# The release profile keeps debuginfo (debug = 1) — ~150M unstripped, ~23M after.
strip "$root/bin/verterm"

install -Dm644 "$repo/verterm.desktop" "$root/share/applications/verterm.desktop"
install -Dm644 "$repo/assets/verterm.svg" "$root/share/icons/hicolor/scalable/apps/verterm.svg"
install -Dm644 "$repo/README.md" "$root/share/doc/verterm/README.md"
install -Dm644 "$repo/config.example.toml" "$root/share/doc/verterm/config.example.toml"
# There is no LICENSE file in the tree yet; Cargo.toml declares MIT. Ship it when it
# appears. `if` rather than `[[ ... ]] && install ...` so that the guard stays safe if
# it is ever moved: under `set -e` a false && list is only fatal as the last statement
# of a script, which makes the one-liner form a trap for whoever reorders this next.
if [[ -f "$repo/LICENSE" ]]; then
  install -Dm644 "$repo/LICENSE" "$root/share/doc/verterm/LICENSE"
fi

# AppStream wants a reverse-DNS component id and wants the file named after it.
# `io.github.<owner>` is the convention for a GitHub-hosted project (it is what
# Flathub uses), and the owner is the path segment before the repository name.
owner="$(basename "$(dirname "$url")")"
# Component and developer ids must be lowercase; the display name keeps its casing.
owner_id="$(printf '%s' "$owner" | tr '[:upper:]' '[:lower:]')"
appid="io.github.$owner_id.verterm"

mkdir -p "$root/share/metainfo"
sed -e "s|@VERSION@|$version|g" \
    -e "s|@DATE@|$(date -u +%Y-%m-%d)|g" \
    -e "s|@URL@|$url|g" \
    -e "s|@APPID@|$appid|g" \
    -e "s|@OWNER@|$owner|g" \
    -e "s|@OWNER_ID@|$owner_id|g" \
    "$repo/packaging/verterm.metainfo.xml.in" > "$root/share/metainfo/$appid.metainfo.xml"

# Deterministic tarball: fixed ownership, sorted entries, no per-run mtimes.
tar -C "$stage" \
    --owner=0 --group=0 --numeric-owner --sort=name \
    --mtime="@${SOURCE_DATE_EPOCH:-0}" \
    -czf "$outdir/$prefix.tar.gz" "$prefix"

echo "$outdir/$prefix.tar.gz"
