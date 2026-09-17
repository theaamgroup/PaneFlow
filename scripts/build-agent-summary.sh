#!/usr/bin/env bash
# Compile the paneflow-agent-summary sidecar (issue #576).
#
# The sidecar is a single Swift file (native/agent-summary/main.swift) that
# talks to Apple's FoundationModels framework, which has no Objective-C
# surface and so cannot be reached from Rust directly. It is NOT staged by
# src-app/build.rs and does not count against EMBED_SIZE_LIMIT_BYTES:
# bundle-macos.sh installs it as Contents/Helpers/paneflow-agent-summary,
# and sign-macos.sh signs everything under Contents/Helpers.
#
# For a `cargo run` build, drop it beside the debug binary and the app finds
# it there (`app/agent_summary/sidecar.rs::locate`):
#
#   scripts/build-agent-summary.sh --out target/debug/paneflow-agent-summary
#
# Usage:
#   scripts/build-agent-summary.sh --out <path> [--arch aarch64|x86_64]
#
# The deployment target is the app's own floor (macOS 13, assets/Info.plist)
# so the helper always launches; the framework is availability-gated at run
# time, and an SDK without it (Xcode 16.4, the release lane's pin) compiles
# a stub that reports `available:false`.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
SOURCE="$REPO_ROOT/native/agent-summary/main.swift"

OUT=""
ARCH="aarch64"

die() {
    echo "error: $*" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --out)
            [ "$#" -ge 2 ] || die "--out requires a path"
            OUT="$2"
            shift 2
            ;;
        --arch)
            [ "$#" -ge 2 ] || die "--arch requires an argument"
            ARCH="$2"
            shift 2
            ;;
        -h|--help)
            sed -n '2,24p' "$0" >&2
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

[ -n "$OUT" ] || die "--out is required"
[ -f "$SOURCE" ] || die "sidecar source not found at $SOURCE"

case "$ARCH" in
    aarch64) TARGET="arm64-apple-macos13.0" ;;
    x86_64)  TARGET="x86_64-apple-macos13.0" ;;
    *)       die "--arch must be 'aarch64' or 'x86_64' (got '$ARCH')" ;;
esac

command -v xcrun >/dev/null 2>&1 || die "xcrun not found; full Xcode is a build prerequisite (see CLAUDE.md)"

mkdir -p "$(dirname "$OUT")"

# `-parse-as-library` is deliberately absent: main.swift is a top-level
# script and must stay one. Warnings are not errors here because the SDK
# decides which branch of the `#if canImport` compiles.
xcrun swiftc \
    -O \
    -target "$TARGET" \
    -module-name paneflow_agent_summary \
    -o "$OUT" \
    "$SOURCE"
chmod 0755 "$OUT"

if [ -d "$(xcrun --show-sdk-path)/System/Library/Frameworks/FoundationModels.framework" ]; then
    echo "Built sidecar: $OUT ($ARCH, FoundationModels SDK present)"
else
    echo "Built sidecar: $OUT ($ARCH, stub: this SDK has no FoundationModels; summaries report unavailable)" >&2
fi
