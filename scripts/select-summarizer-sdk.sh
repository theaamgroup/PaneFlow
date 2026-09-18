#!/usr/bin/env bash
# select-summarizer-sdk.sh - print the Xcode developer directory the summariser
# sidecar should compile with (issue #586).
#
# CI pins Xcode 16.4 for the Metal / GPUI build, and that SDK predates
# FoundationModels. src-app/build.rs reads PANEFLOW_SUMMARIZER_DEVELOPER_DIR
# and points the ONE `swiftc` call for the sidecar at it, so the pin stays
# where it is. The hosted image rotates its Xcode 26 point releases, so this
# takes the newest one present rather than naming a version a refresh removes.
#
# Prints the directory on stdout; diagnostics go to stderr.
# Usage: PANEFLOW_SUMMARIZER_DEVELOPER_DIR="$(scripts/select-summarizer-sdk.sh)"
#   --applications <dir>  search <dir> instead of /Applications (tests)
set -euo pipefail

APPLICATIONS="/Applications"
if [ "${1:-}" = "--applications" ]; then
  APPLICATIONS="${2:?--applications needs a directory}"
fi

NEWEST=""
NEWEST_VERSION=""
for candidate in "$APPLICATIONS"/Xcode_26*.app "$APPLICATIONS"/Xcode.app; do
  [ -d "$candidate/Contents/Developer" ] || continue
  sdk="$(env -u SDKROOT DEVELOPER_DIR="$candidate/Contents/Developer" \
    xcrun --sdk macosx --show-sdk-path 2>/dev/null || true)"
  [ -d "$sdk/System/Library/Frameworks/FoundationModels.framework" ] || continue
  version="$(env -u SDKROOT DEVELOPER_DIR="$candidate/Contents/Developer" \
    xcrun --sdk macosx --show-sdk-version 2>/dev/null || true)"
  # Highest SDK version wins; `sort -V` on two lines, no early-exit reader.
  if [ -z "$NEWEST" ] || [ "$(printf '%s\n%s\n' "$NEWEST_VERSION" "$version" | sort -V | tail -n 1)" != "$NEWEST_VERSION" ]; then
    NEWEST="$candidate"
    NEWEST_VERSION="$version"
  fi
done

if [ -z "$NEWEST" ]; then
  printf 'error: no Xcode under %s has a macOS SDK with FoundationModels.framework\n' "$APPLICATIONS" >&2
  ls -d "$APPLICATIONS"/Xcode*.app >&2 2>/dev/null || true
  exit 1
fi

printf 'summariser sidecar SDK: macOS %s from %s\n' "$NEWEST_VERSION" "$NEWEST" >&2
printf '%s\n' "$NEWEST/Contents/Developer"
