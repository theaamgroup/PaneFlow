#!/usr/bin/env bash
# Generate assets/PaneFlow.icns from a prepared PNG iconset.
#
# US-014. The `.icns` file is consumed by `scripts/bundle-macos.sh`
# (US-013), which copies it into `Contents/Resources/` of the .app bundle.
#
# build-icons.sh passes a prepared plated source directory through
# PANEFLOW_ICNS_SOURCE_DIR. Requiring it prevents a direct invocation from
# packing an unprepared iconset.
#
# macOS only: `sips` resizes and `iconutil` packs. Both ship with macOS, so
# there is no Linux packer cascade (png2icns / icnsutil / python3) any more.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"

SRC_DIR="${PANEFLOW_ICNS_SOURCE_DIR:-}"
OUT="$REPO_ROOT/assets/PaneFlow.icns"

die() {
    echo "error: $*" >&2
    exit 1
}

[ -n "$SRC_DIR" ] || die "PANEFLOW_ICNS_SOURCE_DIR is required; run scripts/build-icons.sh"

# --- Validate sources -----------------------------------------------------
# The Apple iconset spec needs 16, 32, 64, 128, 256, 512, and 1024 px. The
# five baseline sizes are required here; 64 and 1024 are derived only when the
# caller did not prepare them explicitly.
for size in 16 32 128 256 512; do
    src="$SRC_DIR/paneflow-$size.png"
    [ -f "$src" ] || die "missing source PNG: $src"
done

# --- Resize helper --------------------------------------------------------
# `sips` is macOS-native, so no extra dependency is needed.
resize_png() {
    local src="$1" dst="$2" size="$3"
    sips -Z "$size" "$src" --out "$dst" >/dev/null
}

command -v sips >/dev/null 2>&1 || die "sips not found (macOS built-in)"
command -v iconutil >/dev/null 2>&1 || die "iconutil not found (macOS built-in)"

# --- Build iconset staging dir --------------------------------------------
STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT
ICONSET="$STAGING/PaneFlow.iconset"
mkdir -p "$ICONSET"

# Prefer explicit sources prepared by build-icons.sh. Keep a derivation
# fallback for callers that provide a valid iconset without 64/1024 sources.
SRC_64="$SRC_DIR/paneflow-64.png"
if [ ! -f "$SRC_64" ]; then
    SRC_64="$STAGING/paneflow-64.png"
    resize_png "$SRC_DIR/paneflow-128.png" "$SRC_64" 64
fi
SRC_1024="$SRC_DIR/paneflow-1024.png"
if [ ! -f "$SRC_1024" ]; then
    SRC_1024="$STAGING/paneflow-1024.png"
    resize_png "$SRC_DIR/paneflow-512.png" "$SRC_1024" 1024
fi

# Apple iconset filename convention (iconutil(1)):
#   icon_<base>[@2x].png, where base ∈ {16x16, 32x32, 128x128, 256x256, 512x512}
#   and the logical pixel count is base, with @2x doubling it.
cp "$SRC_DIR/paneflow-16.png"           "$ICONSET/icon_16x16.png"
cp "$SRC_DIR/paneflow-32.png"           "$ICONSET/icon_16x16@2x.png"
cp "$SRC_DIR/paneflow-32.png"           "$ICONSET/icon_32x32.png"
cp "$SRC_64"                            "$ICONSET/icon_32x32@2x.png"
cp "$SRC_DIR/paneflow-128.png"          "$ICONSET/icon_128x128.png"
cp "$SRC_DIR/paneflow-256.png"          "$ICONSET/icon_128x128@2x.png"
cp "$SRC_DIR/paneflow-256.png"          "$ICONSET/icon_256x256.png"
cp "$SRC_DIR/paneflow-512.png"          "$ICONSET/icon_256x256@2x.png"
cp "$SRC_DIR/paneflow-512.png"          "$ICONSET/icon_512x512.png"
cp "$SRC_1024"                          "$ICONSET/icon_512x512@2x.png"

# --- Pack .icns -----------------------------------------------------------
echo "Packing via iconutil..."
iconutil -c icns "$ICONSET" -o "$OUT"

# --- Verify ---------------------------------------------------------------
[ -s "$OUT" ] || die "produced empty $OUT"
# ICNS magic header is the ASCII bytes "icns" at offset 0.
if ! head -c 4 "$OUT" | grep -q icns; then
    die "$OUT is not a valid ICNS file (missing 'icns' magic header)"
fi

echo "Generated: $OUT ($(wc -c < "$OUT") bytes)"
