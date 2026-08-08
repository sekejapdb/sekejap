#!/usr/bin/env bash
#
# Create the release tags for the version in the root Cargo.toml (the single
# source of truth):
#
#   vX.Y.Z              — the ecosystem release tag (fires the Release workflow:
#                         crates.io, PyPI, npm, pub.dev, Maven)
#   wrappers/go/vX.Y.Z  — the Go module tag (Go reads subdirectory-module
#                         versions from a path-prefixed tag, not the bare vX.Y.Z)
#
# Usage:
#   scripts/tag-release.sh            # create the tags locally
#   scripts/tag-release.sh --push     # create and push them to origin
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION=$(grep -m1 -E '^version = ' "$ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')

if [ -z "$VERSION" ]; then
  echo "error: could not read [workspace.package] version from Cargo.toml" >&2
  exit 1
fi

MAIN_TAG="v$VERSION"
GO_TAG="wrappers/go/v$VERSION"

echo "Version: $VERSION"
echo "  main tag : $MAIN_TAG"
echo "  go  tag  : $GO_TAG"

git -C "$ROOT" tag "$MAIN_TAG"
git -C "$ROOT" tag "$GO_TAG"
echo "created tags."

if [ "${1:-}" = "--push" ]; then
  git -C "$ROOT" push origin "$MAIN_TAG" "$GO_TAG"
  echo "pushed to origin."
else
  echo "run with --push to push, or: git push origin $MAIN_TAG $GO_TAG"
fi
