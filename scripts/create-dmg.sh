#!/usr/bin/env bash
# Package a signed+notarized .app bundle into a drag-to-Applications .dmg.
#
# US-016. Run this on a macOS runner AFTER `scripts/sign-macos.sh` and
# `scripts/notarize-macos.sh` have produced a signed, stapled bundle at
# `dist/PaneFlow.app`.
#
# Output filename convention:
#   dist/paneflow-<version>-<arch>-apple-darwin.dmg
#
# The `-apple-darwin` suffix is a naming contract shared by this script,
# `release.yml`, the Sparkle appcast, and the runbook.
#
# Implementation: follows the Zed bundle-mac pattern
# (zed-industries/zed `script/bundle-mac:260` upstream) - a single
# `hdiutil create -srcfolder ... -format UDZO` invocation that auto-sizes
# the image from the source folder and produces the final zlib-compressed
# image in one pass. Earlier revisions of this script ran a UDRW staging
# image + osascript Finder layout + UDZO convert pipeline. On macos-14
# hosted runners that pipeline failed with `hdiutil: create failed - No
# space left on device` despite tens of GB free on the host volume:
# `hdiutil create -size <fixed>m` pre-allocates a virtual device whose
# ENOSPC is internal to the image, not the runner. Letting hdiutil
# auto-size from -srcfolder bypasses the failure mode entirely.
#
# Trade-off: no custom Finder window layout (icon positions, background
# image). The DMG still presents the standard /Applications symlink for
# drag-to-install. Cosmetic layout can be re-introduced later by bringing
# back a UDRW staging stage WITHOUT a fixed -size flag - but the unstyled
# DMG ships and notarizes today, which is the priority.
#
# `hdiutil create`/`verify`/`attach` also share a second failure mode:
# a wedged `diskimages-helper` returns EAGAIN ("Resource temporarily
# unavailable") and `verify` reports the file as an unrecognizable image
# even when create exited 0 (issue #547, v0.6.1). The create+verify pair
# and the later attach are retried a bounded number of times; the
# codesign / stapler / spctl verdicts after a successful mount are not.
#
# Usage:
#   scripts/create-dmg.sh --version 0.2.0 --arch aarch64
#   scripts/create-dmg.sh --version 0.2.0 --arch x86_64 --app path/to/X.app
#
# Sourced by scripts/create-dmg.test.sh (CREATE_DMG_LIB=1) so retry/verify
# can be exercised without a signed .app.
set -euo pipefail

# Tests set HDIUTIL_RETRY_SLEEP_SEC=0. Production keeps a short backoff
# so a dying diskimages-helper can exit before the next attempt.
: "${HDIUTIL_RETRY_ATTEMPTS:=3}"
: "${HDIUTIL_RETRY_SLEEP_SEC:=2}"

usage() {
    cat >&2 <<EOF
Usage: $0 --version <ver> --arch {aarch64|x86_64} [--app <path>]
EOF
}

die() {
    echo "error: $*" >&2
    exit 1
}

# Log leftover attachments (do not detach foreign volumes) and wait.
settle_diskimages_helper() {
    local attempt="${1:-}"
    echo "Settling diskimages-helper${attempt:+ after attempt ${attempt}}..." >&2
    hdiutil info >&2 || true
    sleep "${HDIUTIL_RETRY_SLEEP_SEC}"
}

create_udzo_dmg() {
    local staging="$1"
    local volname="$2"
    local dest="$3"
    hdiutil create \
        -volname "$volname" \
        -srcfolder "$staging" \
        -ov \
        -format UDZO \
        "$dest" >/dev/null
}

verify_dmg() {
    hdiutil verify "$1" >/dev/null
}

# Retry hdiutil verify alone. Used by tests to feed a truncated/non-image
# file at the verify step; production retries verify only as part of the
# create+verify pair below.
hdiutil_verify_with_retry() {
    local image="$1"
    local attempt=1
    local max="${HDIUTIL_RETRY_ATTEMPTS}"
    while [ "$attempt" -le "$max" ]; do
        if verify_dmg "$image"; then
            return 0
        fi
        echo "warning: hdiutil verify failed (attempt ${attempt}/${max})" >&2
        if [ "$attempt" -eq "$max" ]; then
            return 1
        fi
        settle_diskimages_helper "$attempt"
        attempt=$((attempt + 1))
    done
    return 1
}

# One create + one verify per attempt. A failed attempt deletes the
# partial image before retrying so `-ov` is not the only stale-file guard.
# Returns 0 on a verified image; 1 after exhausting retries (dest removed).
create_and_verify_dmg() {
    local staging="$1"
    local volname="$2"
    local dest="$3"
    local attempt=1
    local max="${HDIUTIL_RETRY_ATTEMPTS}"
    while [ "$attempt" -le "$max" ]; do
        rm -f "$dest"
        echo "Creating $dest (source: $(du -sh "$staging" | awk '{print $1}'))..."
        if create_udzo_dmg "$staging" "$volname" "$dest" && verify_dmg "$dest"; then
            return 0
        fi
        echo "warning: hdiutil create/verify failed (attempt ${attempt}/${max})" >&2
        rm -f "$dest"
        if [ "$attempt" -eq "$max" ]; then
            return 1
        fi
        settle_diskimages_helper "$attempt"
        attempt=$((attempt + 1))
    done
    return 1
}

# First device (/dev/diskN) in hdiutil attach's stdout. attach prints a
# checksum line (`expected   CRC32 $...`) before the device table, so the
# first line's first field is not the device.
attach_output_device() {
    printf '%s\n' "$1" | awk '$1 ~ /^\/dev\/disk/ {print $1; exit}'
}

# True when a filesystem is mounted at the given directory.
mounted_at() {
    mount | grep -qF " on $1 ("
}

# Detach whatever a failed `hdiutil attach` left behind: every /dev/diskN
# it printed before failing, and anything still mounted at the mountpoint.
# Without this the retry attaches the image a second time and macOS mounts
# it beside the first one (`PaneFlow 1`), so verification either dies on a
# missing path or inspects a stale volume from an earlier run.
detach_attach_leftovers() {
    local mountpoint="$1"
    local output="${2:-}"
    local dev seen=""
    while read -r dev _; do
        case "$dev" in
            /dev/disk*)
                case " $seen " in *" $dev "*) continue ;; esac
                seen="$seen $dev"
                echo "Detaching leftover attachment $dev..." >&2
                hdiutil detach "$dev" -force >/dev/null 2>&1 || true
                ;;
        esac
    done <<< "$output"
    if [ -n "$mountpoint" ] && mounted_at "$mountpoint"; then
        echo "Detaching volume still mounted at $mountpoint..." >&2
        hdiutil detach "$mountpoint" -force >/dev/null 2>&1 || true
    fi
}

# Attach $1 read-only at the explicit mountpoint $2 (created if missing).
# Sets _HDIUTIL_ATTACH_OUTPUT to hdiutil attach's stdout on success. A
# failed attempt is detached before the retry (see detach_attach_leftovers).
hdiutil_attach_with_retry() {
    local image="$1"
    local mountpoint="${2:-}"
    local attempt=1
    local max="${HDIUTIL_RETRY_ATTEMPTS}"
    _HDIUTIL_ATTACH_OUTPUT=""
    [ -n "$mountpoint" ] || die "hdiutil_attach_with_retry needs a mountpoint"
    mkdir -p "$mountpoint"
    while [ "$attempt" -le "$max" ]; do
        # The assignment keeps attach's partial stdout even when it fails,
        # which is how the device of a half-finished attempt is recovered.
        if _HDIUTIL_ATTACH_OUTPUT="$(hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$mountpoint" "$image")"; then
            return 0
        fi
        echo "warning: hdiutil attach failed (attempt ${attempt}/${max})" >&2
        detach_attach_leftovers "$mountpoint" "$_HDIUTIL_ATTACH_OUTPUT"
        _HDIUTIL_ATTACH_OUTPUT=""
        if [ "$attempt" -eq "$max" ]; then
            return 1
        fi
        settle_diskimages_helper "$attempt"
        attempt=$((attempt + 1))
    done
    return 1
}

# EXIT trap for create_dmg_main. Detaches the verification volume if it is
# still attached, then removes the temp mountpoint and staging directories.
# The globals are expanded at process exit, after main's locals are gone.
cleanup_create_dmg() {
    if [ -n "${VERIFY_PT:-}" ]; then
        detach_attach_leftovers "$VERIFY_PT" "${_HDIUTIL_ATTACH_OUTPUT:-}"
    fi
    [ -z "${MOUNT_ROOT:-}" ] || rm -rf "$MOUNT_ROOT"
    [ -z "${STAGING:-}" ] || rm -rf "$STAGING"
}

create_dmg_main() {
    local VERSION=""
    local ARCH=""
    local APP="dist/PaneFlow.app"

    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version)  [ "$#" -ge 2 ] || die "--version requires an argument"; VERSION="$2"; shift 2 ;;
            --arch)     [ "$#" -ge 2 ] || die "--arch requires an argument";    ARCH="$2";    shift 2 ;;
            --app)      [ "$#" -ge 2 ] || die "--app requires an argument";     APP="$2";     shift 2 ;;
            -h|--help)  usage; exit 0 ;;
            *)          usage; die "unknown argument: $1" ;;
        esac
    done

    # --- Validate inputs ------------------------------------------------------
    [ -n "$VERSION" ] || { usage; die "--version is required"; }
    [ -n "$ARCH" ]    || { usage; die "--arch is required"; }
    case "$ARCH" in
        aarch64|x86_64) ;;
        *) die "--arch must be 'aarch64' or 'x86_64' (got '$ARCH')" ;;
    esac
    [ -d "$APP" ] || die "bundle not found: $APP"

    # hdiutil is macOS-native and has no portable equivalent. Fail loudly on
    # other OSes rather than producing a broken DMG.
    command -v hdiutil >/dev/null 2>&1 || die "hdiutil not found (this script only runs on macOS)"

    # STAGING, MOUNT_ROOT, VERIFY_PT and _HDIUTIL_ATTACH_OUTPUT are
    # intentionally global: the EXIT trap expands them at process exit,
    # after this function's locals would already be gone.
    SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
    REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"

    VOLNAME="PaneFlow"
    FINAL_DMG="$REPO_ROOT/dist/paneflow-${VERSION}-${ARCH}-apple-darwin.dmg"

    # --- Prepare staging dir -------------------------------------------------
    # Every DMG run starts from a clean staging directory so stale symlinks or
    # leftover `.Trash-*` files can't leak into the image.
    STAGING="$(mktemp -d)"
    # The verification volume mounts under its own temp directory, never
    # /Volumes/$VOLNAME: an explicit mountpoint cannot collide with a stale
    # volume from an earlier run or with a `PaneFlow 1` from a retried attach.
    MOUNT_ROOT="$(mktemp -d)"
    VERIFY_PT="$MOUNT_ROOT/$VOLNAME"
    trap cleanup_create_dmg EXIT

    # The enclosed app bundle - `cp -R` preserves the embedded code signature
    # and extended attributes (notarization ticket). `ditto` would also work
    # but `cp -R` keeps the dependency surface minimal.
    cp -R "$APP" "$STAGING/"
    BUNDLE_NAME="$(basename "$APP")"

    # Drag target - a symlink to /Applications gives the familiar macOS
    # "drag here to install" UX. `ln -s /Applications` creates an absolute
    # symlink that resolves against /Applications when the DMG is mounted
    # on a user's Mac (the symlink target is a string, not a resolved inode).
    ln -s /Applications "$STAGING/Applications"

    # --- Build the final compressed DMG --------------------------------------
    # Single-pass: -srcfolder + -format UDZO produces the final zlib-compressed
    # read-only image directly. -ov overwrites any leftover .dmg from a
    # previous run (idempotent re-runs).
    #
    # Flags deliberately omitted vs. earlier revisions:
    #   -size <N>m       - caused ENOSPC inside hdiutil's virtual device
    #                      when the fixed allocation could not fit content +
    #                      filesystem overhead. Auto-sizing avoids this.
    #   -fs HFS+         - UDZO defaults to HFS+ for backwards compatibility;
    #                      passing it explicitly was redundant.
    #   -fsargs '-c …'   - pre-grew the HFS+ catalog/attributes/extents
    #                      B-trees, competing with payload for fixed-size
    #                      budget. Default newfs_hfs sizing is correct.
    mkdir -p "$(dirname "$FINAL_DMG")"

    if ! create_and_verify_dmg "$STAGING" "$VOLNAME" "$FINAL_DMG"; then
        die "hdiutil create/verify failed after ${HDIUTIL_RETRY_ATTEMPTS} attempts; not shipping an unverified image"
    fi

    # --- Verify -------------------------------------------------------------
    # AC3: codesign inside the DMG must still verify. Mount the final image
    # read-only and run codesign against the embedded .app - any signature
    # drift (e.g., from a buggy hdiutil that rewrote extended attributes)
    # would surface here, not at Gatekeeper time on a user's Mac.
    # Retry covers a wedged diskimages-helper, not the checks below.
    if ! hdiutil_attach_with_retry "$FINAL_DMG" "$VERIFY_PT"; then
        die "hdiutil attach failed after ${HDIUTIL_RETRY_ATTEMPTS} attempts; not shipping an unverified image"
    fi
    local VERIFY_DEV
    VERIFY_DEV="$(attach_output_device "$_HDIUTIL_ATTACH_OUTPUT")"
    [ -n "$VERIFY_DEV" ] || die "hdiutil attach printed no device: $_HDIUTIL_ATTACH_OUTPUT"
    [ -d "$VERIFY_PT/$BUNDLE_NAME" ] || die "attached image has no $BUNDLE_NAME at $VERIFY_PT"
    # Each failure below exits through the EXIT trap, which detaches the volume.
    if ! codesign --verify --deep --strict "$VERIFY_PT/$BUNDLE_NAME"; then
        die "codesign verification failed on enclosed bundle"
    fi
    if ! xcrun stapler validate "$VERIFY_PT/$BUNDLE_NAME"; then
        die "stapled notarization ticket validation failed on enclosed bundle"
    fi
    if ! spctl --assess --type exec --verbose "$VERIFY_PT/$BUNDLE_NAME"; then
        die "Gatekeeper assessment failed on enclosed bundle"
    fi
    hdiutil detach "$VERIFY_DEV" -quiet 2>/dev/null \
        || hdiutil detach "$VERIFY_DEV" -force 2>/dev/null \
        || true
    # Cleared so the EXIT trap does not chase a device number that may be
    # reused by another image between here and process exit.
    _HDIUTIL_ATTACH_OUTPUT=""

    echo "Created: $FINAL_DMG ($(du -h "$FINAL_DMG" | awk '{print $1}'))"
}

if [ "${CREATE_DMG_LIB:-}" = "1" ] || [ "${BASH_SOURCE[0]}" != "$0" ]; then
    :
else
    create_dmg_main "$@"
fi
