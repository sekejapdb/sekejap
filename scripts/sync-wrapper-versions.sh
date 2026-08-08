#!/usr/bin/env bash
#
# Single source of truth for the whole workspace version:
#   [workspace.package] version = "X.Y.Z"   in the root Cargo.toml
#
# Rust crates + the Python wheel already inherit that (version.workspace = true).
# The Kotlin build reads Cargo.toml directly (see wrappers/kotlin/build.gradle.kts).
# npm's package.json and dart's pubspec.yaml are static formats that can't compute
# a value, so this script stamps the Cargo.toml version into them. Run it locally
# before a manual publish, or let the Release workflow run it in CI.
#
# Usage:  scripts/sync-wrapper-versions.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# The first `version = "..."` line in Cargo.toml is [workspace.package] (line ~9);
# `version.workspace = true` further down does NOT match `^version = `.
VERSION=$(grep -m1 -E '^version = ' "$ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')

if [ -z "$VERSION" ]; then
  echo "error: could not read [workspace.package] version from Cargo.toml" >&2
  exit 1
fi
echo "Syncing wrapper manifests to sekejap $VERSION"

# npm — replace only the top-level "version": "..." (first match; perl is portable
# across the macOS/Linux runners, unlike `sed -i`).
perl -0pi -e "s/(\"version\"\s*:\s*)\"[^\"]*\"/\${1}\"$VERSION\"/" \
  "$ROOT/wrappers/node/package.json"

# node crate — version() in the addon is env!("CARGO_PKG_VERSION") of this
# crate, so stamp it too or the addon reports a stale number.
perl -pi -e "s/^version = .*/version = \"$VERSION\"/ if \$. <= 10" \
  "$ROOT/wrappers/node/Cargo.toml"

# dart — the top-level `version:` key (runtime package + the codegen generator).
perl -pi -e "s/^version:.*/version: $VERSION/" \
  "$ROOT/wrappers/dart/pubspec.yaml"
perl -pi -e "s/^version:.*/version: $VERSION/" \
  "$ROOT/wrappers/dart/sekejap_generator/pubspec.yaml"

echo "  node : $(grep -m1 '"version"' "$ROOT/wrappers/node/package.json" | tr -d ' ,') / crate $(grep -m1 '^version = ' "$ROOT/wrappers/node/Cargo.toml")"
echo "  dart : $(grep -m1 '^version:' "$ROOT/wrappers/dart/pubspec.yaml") / generator $(grep -m1 '^version:' "$ROOT/wrappers/dart/sekejap_generator/pubspec.yaml")"
echo "  kotlin reads Cargo.toml directly (no stamp needed)"
