#!/usr/bin/env bash
# verify-summarizer-sidecar.sh - prove a built `paneflow-summarize` can reach
# the on-device model (issue #586).
#
# A sidecar compiled against an SDK older than macOS 26 still builds, runs and
# answers "This build has no Foundation Models support", so nothing downstream
# notices that every summary is dead. This reads the Mach-O load commands and
# requires:
#   - a WEAK load of FoundationModels.framework (weak, so the binary still
#     launches on a system that does not have the framework), and
#   - the macOS 13.0 deployment target the runtime availability guard relies on.
#
# Usage: scripts/verify-summarizer-sidecar.sh <path-to-paneflow-summarize>
set -euo pipefail

if [ "$#" -ne 1 ]; then
  printf 'usage: %s <path-to-paneflow-summarize>\n' "$0" >&2
  exit 2
fi

SIDECAR="$1"
if [ ! -f "$SIDECAR" ]; then
  printf 'error: %s was not built; look for the #576 cargo warning in the build log\n' "$SIDECAR" >&2
  exit 1
fi

# One awk pass over the whole listing: no early-exit reader, so `pipefail`
# cannot turn a match into a SIGPIPE failure.
LOAD_COMMANDS="$(otool -l "$SIDECAR")"
VERDICT="$(awk '
  $1 == "cmd" { cmd = $2 }
  cmd == "LC_LOAD_WEAK_DYLIB" && $1 == "name" && $2 ~ /\/FoundationModels\.framework\// { weak = 1 }
  cmd == "LC_LOAD_DYLIB" && $1 == "name" && $2 ~ /\/FoundationModels\.framework\// { strong = 1 }
  cmd == "LC_BUILD_VERSION" && $1 == "minos" { minos = $2 }
  END { printf "%d %d %s\n", weak, strong, (minos == "" ? "unknown" : minos) }
' <<<"$LOAD_COMMANDS")"
read -r WEAK STRONG MINOS <<<"$VERDICT"

if [ "$STRONG" = "1" ]; then
  printf 'error: %s links FoundationModels strongly; it would fail to launch before macOS 26\n' "$SIDECAR" >&2
  exit 1
fi
if [ "$WEAK" != "1" ]; then
  printf 'error: %s does not link FoundationModels; it was compiled without a macOS 26 SDK\n' "$SIDECAR" >&2
  printf '       set PANEFLOW_SUMMARIZER_DEVELOPER_DIR to an Xcode 26 developer directory\n' >&2
  exit 1
fi
if [ "$MINOS" != "13.0" ]; then
  printf 'error: %s has deployment target %s, expected 13.0\n' "$SIDECAR" "$MINOS" >&2
  exit 1
fi

printf 'ok: %s weak-links FoundationModels, deployment target %s\n' "$SIDECAR" "$MINOS"
