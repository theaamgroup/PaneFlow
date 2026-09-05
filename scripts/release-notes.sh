#!/usr/bin/env bash
# Use the same short, curated notes on GitHub and in Sparkle.
set -euo pipefail

tag="${1:?Usage: release-notes.sh vX.Y.Z}"
if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
  echo "Invalid release tag: $tag" >&2
  exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
curated="$repo_root/docs/releases/$tag.md"
if [[ -s "$curated" ]]; then
  cat "$curated"
else
  gh api --method POST \
    "repos/${GITHUB_REPOSITORY:?Set GITHUB_REPOSITORY for generated notes}/releases/generate-notes" \
    -f tag_name="$tag" \
    -f target_commitish="${GITHUB_SHA:?Set GITHUB_SHA for generated notes}" \
    --jq '.body'
fi
