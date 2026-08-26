#!/usr/bin/env bash
# Generate PaneFlow's macOS icon assets from the plated master PNG.
#
# Inputs (in assets/icons/master/):
#   paneflow-icon-macos-1024.png        required plated macOS artwork
#   paneflow-icon-template-1024.png     optional macOS menubar Template image (black silhouette + alpha)
#
# Outputs:
#   assets/PaneFlow.icns                                     consumed by scripts/bundle-macos.sh
#   src-app/assets/icons/paneflow.png                        runtime-embedded GPUI window icon (rust-embed)
#   assets/icons/paneflowTemplate{,@2x}.png                  macOS menubar templates (only if template master exists)
#
# This fork is macOS only. Do not emit Linux hicolor PNGs, a Windows .ico,
# or anything under packaging/wix/.
#
# Idempotent and deterministic. Run after editing a master, then commit the regenerated outputs.
#
# Backward compatible: when no macOS master PNG is present the script logs a
# warning and exits 0, keeping the committed icons in place.
set -euo pipefail

# Serialise ImageMagick's coder-module loading. The intermittent SIGABRT
# documented on `run_magick` below is a thread race in IM7's module
# registry during first-load of a coder/delegate: two worker threads
# initialise the same module concurrently and abort. Pinning IM to a
# single thread makes module init deterministic and serial. It only
# affects parallelism inside one invocation (icon resizes are tiny, so
# the wall-clock cost is nil) and never changes a single output pixel.
export MAGICK_THREAD_LIMIT=1
export OMP_NUM_THREADS=1

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"

MASTER_DIR="$REPO_ROOT/assets/icons/master"
OUT_ICONS_DIR="$REPO_ROOT/assets/icons"
OUT_ICNS="$REPO_ROOT/assets/PaneFlow.icns"
OUT_RUNTIME_ICON="$REPO_ROOT/src-app/assets/icons/paneflow.png"

log()  { printf '%s\n' "$*" >&2; }
warn() { log "warning: $*"; }
die()  { log "error: $*"; exit 1; }

# Resolve a master by stem: accept .png (preferred), .jpg, or .jpeg so that
# raw Nano Banana / Midjourney / DALL-E exports (which default to JPG) can
# be dropped in without manual conversion. ImageMagick reads either format
# transparently and writes PNG on the output side.
resolve_master() {
    local stem="$1" path
    for ext in png jpg jpeg; do
        path="$MASTER_DIR/${stem}.${ext}"
        if [ -f "$path" ]; then
            printf '%s' "$path"
            return 0
        fi
    done
    return 1
}

MASTER_MACOS="$(resolve_master "paneflow-icon-macos-1024" || true)"
MASTER_TEMPLATE="$(resolve_master "paneflow-icon-template-1024" || true)"

# --- Graceful no-op when no macOS master is present ----------------------
# Apr 2026 baseline shipped committed PNGs directly without a master pipeline.
# This guard lets a checkout without the master keep the committed icons.
if [ -z "$MASTER_MACOS" ]; then
    warn "no macOS master found at $MASTER_DIR/paneflow-icon-macos-1024.{png,jpg,jpeg}"
    warn "keeping existing committed icons. To regenerate, drop a 1024x1024 macOS master in that directory and re-run."
    exit 0
fi

# --- Resolve ImageMagick before writing any output -----------------------
# The plated .icns path draws a rounded mask ImageMagick owns. Resolve and
# validate the tool once up front to avoid partial outputs.
IM_BIN=""
if command -v magick >/dev/null 2>&1; then
    IM_BIN="magick"
elif command -v convert >/dev/null 2>&1 \
    && convert -version 2>&1 | grep -qi "ImageMagick"; then
    IM_BIN="convert"
else
    die "need ImageMagick 6 or 7 to regenerate the icon set"
fi

# macOS geometry: legacy .icns bundles still need a plated fallback. Keep
# the traditional 824/1024 body and rounded mask isolated to that output.
MACOS_BODY_PCT=8047
MACOS_MASK_RADIUS_PCT=2237

# Run a `magick` (or `convert`) invocation with up to 6 attempts.
# ImageMagick 7.1.2-23 (the current Homebrew bottle on macos-14-arm64)
# has an intermittent SIGABRT (exit 134) during coder-module loading --
# the same script, with the same master PNG, will succeed one run and
# crash the next. A cheap retry is worth the safety.
#
# The first arg picks the IM binary (`magick` for IM7, `convert` for
# IM6); remaining args are passed verbatim. Caller is responsible for
# the if/elif branch; this helper only adds the retry. `if run_magick`
# is set-e-safe because failure inside an `if` test is suppressed.
run_magick() {
    local bin="$1"; shift
    local attempt=0
    local max=6
    while : ; do
        if "$bin" "$@"; then
            return 0
        fi
        attempt=$((attempt + 1))
        if [ "$attempt" -ge "$max" ]; then
            warn "$bin failed after $max attempts"
            return 1
        fi
        # Escalating backoff (1s, 2s, 3s, ...) gives any transient
        # module-loader / temp-file contention more room between tries
        # than a flat 1s without ballooning total wall-clock.
        warn "$bin transient failure (attempt $attempt/$max); retrying in ${attempt}s"
        sleep "$attempt"
    done
}

resize_png() {
    local src="$1" dst="$2" size="$3"
    run_magick "$IM_BIN" "$src" -filter Lanczos -resize "${size}x${size}" -strip "$dst"
}

resize_macos_png() {
    local src="$1" dst="$2" size="$3"
    local body=$(( size * MACOS_BODY_PCT / 10000 ))
    [ "$body" -lt 1 ] && body=1
    local radius=$(( body * MACOS_MASK_RADIUS_PCT / 10000 ))
    local edge=$(( body - 1 ))
    # 3-element pipeline in a single invocation (fast, no temp files):
    #   1. resize while preserving or creating alpha;
    #   2. draw the legacy macOS rounded mask at the body size;
    #   3. intersect alpha, then center the plated body on the icon canvas.
    run_magick "$IM_BIN" \
        \( "$src" -filter Lanczos -resize "${body}x${body}" -alpha On \) \
        \( -size "${body}x${body}" xc:none -fill white \
            -draw "roundrectangle 0,0 ${edge},${edge} ${radius},${radius}" \) \
        -compose DstIn -composite \
        +repage -compose Over -background none -gravity center \
        -extent "${size}x${size}" \
        -strip "PNG32:$dst"
}

TMP_ASSETS="$(mktemp -d)"
trap 'rm -rf "$TMP_ASSETS"' EXIT

# --- macOS .icns ---------------------------------------------------------
# Generate a dedicated plated iconset, then delegate packing to the existing
# iconutil/png2icns/icnsutil/python3 fallback chain in generate-icns.sh.
TMP_MACOS="$TMP_ASSETS/macos"
mkdir -p "$TMP_MACOS"
for size in 16 32 64 128 256 512 1024; do
    resize_macos_png "$MASTER_MACOS" "$TMP_MACOS/paneflow-${size}.png" "$size"
done
log "  $OUT_ICNS  (via generate-icns.sh)"
PANEFLOW_ICNS_SOURCE_DIR="$TMP_MACOS" bash "$SCRIPT_DIR/generate-icns.sh" >&2

# Runtime-embedded GPUI window icon -- rust-embed picks this up at compile
# time for the title-bar / about pane uses. 128px is enough today. Sourced
# from the macOS master; the old portable Linux/Windows mark is gone.
mkdir -p "$(dirname "$OUT_RUNTIME_ICON")"
log "  $OUT_RUNTIME_ICON"
resize_macos_png "$MASTER_MACOS" "$OUT_RUNTIME_ICON" 128

# --- macOS menubar Template PNGs (optional) ------------------------------
# AppKit auto-tints images whose filename ends in `Template.png` /
# `Template@2x.png`. The template master MUST be a black silhouette on alpha
# (no chrome render, no color). We only emit these if a template master is
# placed -- the existing release flow does not consume them yet.
if [ -f "$MASTER_TEMPLATE" ]; then
    mkdir -p "$OUT_ICONS_DIR"
    log "  $OUT_ICONS_DIR/paneflowTemplate.png + @2x"
    resize_png "$MASTER_TEMPLATE" "$OUT_ICONS_DIR/paneflowTemplate.png"    22
    resize_png "$MASTER_TEMPLATE" "$OUT_ICONS_DIR/paneflowTemplate@2x.png" 44
fi

log ""
log "macOS icon source: $(basename "$MASTER_MACOS")"
[ -f "$MASTER_TEMPLATE" ] || log  "no template master  -- skipping menubar Template PNGs"
