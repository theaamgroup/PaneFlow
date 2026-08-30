#!/usr/bin/env bash
# Fetch and verify the pinned Sparkle distribution used by PaneFlow packaging.
#
# The archive is cached outside the source tree by default. Set
# PANEFLOW_SPARKLE_CACHE_DIR to share a cache between CI steps, or set
# SPARKLE_DIST_DIR in bundle-macos.sh to use an already extracted distribution.
set -euo pipefail

SPARKLE_VERSION="2.9.6"
SPARKLE_SHA256="52bf9e88cdd972fc0c81501377a880e90d47031bd8ca5462488f843e2609e192"
SPARKLE_ARCHIVE="Sparkle-${SPARKLE_VERSION}.tar.xz"
SPARKLE_URL="https://github.com/sparkle-project/Sparkle/releases/download/${SPARKLE_VERSION}/${SPARKLE_ARCHIVE}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
CACHE_ROOT="${PANEFLOW_SPARKLE_CACHE_DIR:-${RUNNER_TEMP:-$REPO_ROOT/target}/paneflow-sparkle}"
VERSION_DIR="$CACHE_ROOT/$SPARKLE_VERSION"
ARCHIVE_PATH="$VERSION_DIR/$SPARKLE_ARCHIVE"
DIST_DIR="$VERSION_DIR/dist"

die() {
    echo "error: $*" >&2
    exit 1
}

archive_is_valid() {
    [ -f "$ARCHIVE_PATH" ] || return 1
    [ "$(shasum -a 256 "$ARCHIVE_PATH" | awk '{print $1}')" = "$SPARKLE_SHA256" ]
}

distribution_is_valid() {
    [ -d "$DIST_DIR/Sparkle.framework" ] \
        && [ -x "$DIST_DIR/bin/sign_update" ] \
        && [ -x "$DIST_DIR/bin/generate_appcast" ] \
        && [ -f "$DIST_DIR/LICENSE" ]
}

if distribution_is_valid; then
    printf '%s\n' "$DIST_DIR"
    exit 0
fi

mkdir -p "$VERSION_DIR"
if ! archive_is_valid; then
    download="$(mktemp "$VERSION_DIR/${SPARKLE_ARCHIVE}.download.XXXXXX")"
    trap 'rm -f "$download"' EXIT
    echo "Fetching Sparkle ${SPARKLE_VERSION} from ${SPARKLE_URL}" >&2
    curl --fail --location --silent --show-error "$SPARKLE_URL" --output "$download"
    actual_sha="$(shasum -a 256 "$download" | awk '{print $1}')"
    [ "$actual_sha" = "$SPARKLE_SHA256" ] || die \
        "Sparkle archive checksum mismatch (expected $SPARKLE_SHA256, got $actual_sha)"
    mv "$download" "$ARCHIVE_PATH"
    trap - EXIT
fi

staging="$(mktemp -d "$VERSION_DIR/dist.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
tar -xJf "$ARCHIVE_PATH" -C "$staging"
[ -d "$staging/Sparkle.framework" ] || die "Sparkle archive has no Sparkle.framework"
[ -x "$staging/bin/sign_update" ] || die "Sparkle archive has no executable sign_update"
[ -x "$staging/bin/generate_appcast" ] || die "Sparkle archive has no executable generate_appcast"
[ -f "$staging/LICENSE" ] || die "Sparkle archive has no LICENSE"

rm -rf "$DIST_DIR"
mv "$staging" "$DIST_DIR"
trap - EXIT
printf '%s\n' "$DIST_DIR"
