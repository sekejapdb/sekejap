#!/usr/bin/env bash
#
# Pre-commit hook: keep the repo outline in sync with the code.
#
# When a commit touches any Rust source, this regenerates
# docs/developer/repo-outline.md (types + functions + line numbers) and stages it,
# so the structure map is never stale. Commits that don't touch .rs files are
# untouched — no noise.
#
#   scripts/pre-commit.sh            # run the check (what the hook calls)
#   scripts/pre-commit.sh --install  # install it as .git/hooks/pre-commit
#   scripts/pre-commit.sh --uninstall
#
# To skip it for one commit: git commit --no-verify
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
HOOK=".git/hooks/pre-commit"

case "${1:-}" in
  --install)
    mkdir -p "$(dirname "$HOOK")"
    cat > "$HOOK" <<'EOF'
#!/usr/bin/env bash
# Installed by scripts/pre-commit.sh --install
exec "$(git rev-parse --show-toplevel)/scripts/pre-commit.sh"
EOF
    chmod +x "$HOOK"
    echo "installed $HOOK (skip once with: git commit --no-verify)"
    exit 0
    ;;
  --uninstall)
    rm -f "$HOOK"
    echo "removed $HOOK"
    exit 0
    ;;
  "") ;;
  *) echo "unknown option: $1" >&2; exit 2 ;;
esac

# Only regenerate when Rust sources are part of this commit.
if ! git diff --cached --name-only --diff-filter=ACMR | grep -qE '^(src|skcli/src)/.*\.rs$'; then
  exit 0
fi

scripts/repo-outline.sh >/dev/null
if ! git diff --quiet -- docs/developer/repo-outline.md 2>/dev/null; then
  git add docs/developer/repo-outline.md
  echo "pre-commit: refreshed docs/developer/repo-outline.md"
fi
