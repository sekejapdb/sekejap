#!/bin/sh
# The textual half of the three-layer restructure, as replayable rules.
#
#   sh tools/layers_rewrite.sh <file> [<file> ...]
#
# Every rule is idempotent: running the script twice over the same file leaves
# it unchanged, so a file that arrives later from another branch can be put
# through the same rules without a merge by hand. The rules only rename crates
# and paths; nothing else in a file is touched.
#
# The layers and the crates they became:
#   kernel/           -> core/kernel/       crate `kernel`        (unchanged)
#   src minus sql,bin -> core/engine/src/   crate `sekejap-core`  (lib sekejap_core)
#   the sql module    -> lang/src/          crate `sekejap-lang`  (lib sekejap_lang)
#   src/bin ops bins  -> dist/src/cli/      crate `sekejap-dist`
#   src/bin rest      -> bench/src/bin/     crate `sekejap-bench`
#
# Paths are rewritten to REPOSITORY-ROOT-RELATIVE form, so the result reads the
# same whichever crate the referring file ended up in. Do not pass this script
# to itself: the table above spells the old paths on purpose.
set -eu

[ $# -gt 0 ] || { echo "usage: $0 <file> [<file> ...]" >&2; exit 2; }

for f in "$@"; do
  [ -f "$f" ] || { echo "skip (not a file): $f" >&2; continue; }

  # ---- 1. crate names ----------------------------------------------------
  # The SQL slice left the engine crate; name it before the engine rename so
  # `e4_prototype::sql::X` does not become `sekejap_core::sql::X` first.
  # Rust only: a recorded log or an evidence manifest names the crate as it was
  # when the run happened, and that is a fact, not a path.
  case "$f" in
   *.rs)
  sed -i '' \
    -e 's|e4_prototype::sql::|sekejap_lang::|g' \
    -e 's|e4_prototype::sql\([^:A-Za-z0-9_]\)|sekejap_lang\1|g' \
    -e 's|crate::sql::|sekejap_lang::|g' \
    -e 's|crate::sql\([^:A-Za-z0-9_]\)|sekejap_lang\1|g' \
    -e 's|e4_prototype|sekejap_core|g' \
    "$f"
   ;;
  esac

  # ---- 2. the engine items the SQL slice reaches, now cross-crate --------
  # Only for files of the lang layer: inside the engine crate `crate::` still
  # means the engine, and rewriting it there would be wrong.
  case "$f" in
    lang/*|*/lang/*)
      sed -i '' \
        -e 's|crate::collections::|sekejap_core::collections::|g' \
        -e 's|crate::query::|sekejap_core::query::|g' \
        -e 's|crate::spatial_math::|sekejap_core::spatial_math::|g' \
        -e 's|crate::spatial_geometry::|sekejap_core::spatial_geometry::|g' \
        -e 's|crate::Kind|sekejap_core::Kind|g' \
        "$f"
      ;;
  esac

  # ---- 3. a `sql::{...}` arm nested in a `use` group of the engine crate --
  # It cannot stay inside the group: the names come from another crate now.
  # The arm becomes its own `use`, and the engine group is reopened after it;
  # an engine group left with no arms is then deleted.
  case "$f" in
   *.rs)
  sed -i '' \
    -e 's|^[[:space:]]*sql::{\(.*\)},$|};\nuse sekejap_lang::{\1};\nuse sekejap_core::{|' \
    "$f"
  sed -i '' -e ':a' -e 'N' -e '$!ba' \
    -e 's|use sekejap_core::{\n};\n||g' \
    "$f"
   ;;
  esac

  # ---- 4. docs moved under the layer that owns them ----------------------
  sed -i '' \
    -e 's|docs/QL_CONTRACT\.md|docs/lang/QL_CONTRACT.md|g' \
    -e 's|docs/CONTRACT_TEST_MAP\.md|docs/lang/CONTRACT_TEST_MAP.md|g' \
    -e 's|docs/E3_PARITY\.md|docs/lang/E3_PARITY.md|g' \
    -e 's|docs/OPS_CONTRACT\.md|docs/dist/OPS_CONTRACT.md|g' \
    -e 's|docs/ARCHITECTURE\.md|docs/core/ARCHITECTURE.md|g' \
    -e 's|docs/COLLECTIONS\.md|docs/core/COLLECTIONS.md|g' \
    -e 's|docs/FORMAT_BASELINE\.md|docs/core/FORMAT_BASELINE.md|g' \
    -e 's|docs/FORMAT_FREEZE\.md|docs/core/FORMAT_FREEZE.md|g' \
    -e 's|docs/FORMAT_V1\.md|docs/core/FORMAT_V1.md|g' \
    -e 's|docs/FOUNDATION_TEST_STANDARD\.md|docs/core/FOUNDATION_TEST_STANDARD.md|g' \
    -e 's|docs/GRAPH_CONTRACT\.md|docs/core/GRAPH_CONTRACT.md|g' \
    -e 's|docs/RECOVERY_BLOCKERS_LOOP\.md|docs/core/RECOVERY_BLOCKERS_LOOP.md|g' \
    -e 's|docs/RECOVERY_CONTRACT\.md|docs/core/RECOVERY_CONTRACT.md|g' \
    -e 's|docs/RECOVERY_MATRIX\.json|docs/core/RECOVERY_MATRIX.json|g' \
    -e 's|docs/RECOVERY_R1\.md|docs/core/RECOVERY_R1.md|g' \
    -e 's|docs/RECOVERY_R2\.md|docs/core/RECOVERY_R2.md|g' \
    -e 's|docs/SOURCE_LAYOUT\.md|docs/core/SOURCE_LAYOUT.md|g' \
    -e 's|docs/SPATIAL_FUNCTIONS\.md|docs/core/SPATIAL_FUNCTIONS.md|g' \
    -e 's|docs/V2_BENCHMARK_PROTOCOL\.md|docs/core/V2_BENCHMARK_PROTOCOL.md|g' \
    -e 's|docs/V2_COLLECTION_INTEGRATION\.md|docs/core/V2_COLLECTION_INTEGRATION.md|g' \
    -e 's|docs/V2_COMPAT_FIXTURES\.md|docs/core/V2_COMPAT_FIXTURES.md|g' \
    -e 's|docs/V2_FOUNDATION_LOOP\.md|docs/core/V2_FOUNDATION_LOOP.md|g' \
    "$f"

  # ---- 5. source paths ---------------------------------------------------
  # Paths are rewritten wherever they appear, whatever directory variable or
  # quote sits in front of them. Idempotency comes from PROTECTING the already
  # rewritten form with a sentinel before the rule runs and restoring it after,
  # which is also what keeps `#[path = "../src/bin/..."]` -- a real relative
  # include inside the bench crate -- from being rewritten at all.
  # ERE (`sed -E`, delimiter `#`) throughout: BSD sed's BRE has no `|`.
  sed -E -i '' \
    -e 's#(bench/)?src/bin/(recover|collection_inspect|collections|control_tree_audit|pagewal_repair|entry|lifecycle)\.rs#dist/src/cli/\2.rs#g' \
    -e 's#src/main\.rs#dist/src/cli/main.rs#g' \
    -e 's#src/sql/#lang/src/#g' \
    -e 's#\.\./src/bin/#@@LAYERS_REL_BIN@@#g' \
    -e 's#bench/src/bin/#@@LAYERS_BENCH_BIN@@#g' \
    -e 's#src/bin/#bench/src/bin/#g' \
    -e 's#@@LAYERS_BENCH_BIN@@#bench/src/bin/#g' \
    -e 's#@@LAYERS_REL_BIN@@#../src/bin/#g' \
    -e 's#lang/tests/sql_#@@LAYERS_LANG_SQL@@#g' \
    -e 's#tests/sql_#lang/tests/sql_#g' \
    -e 's#@@LAYERS_LANG_SQL@@#lang/tests/sql_#g' \
    -e 's#lang/tests/sqlslice#@@LAYERS_LANG_SLICE@@#g' \
    -e 's#tests/sqlslice#lang/tests/sqlslice#g' \
    -e 's#@@LAYERS_LANG_SLICE@@#lang/tests/sqlslice#g' \
    -e 's#core/kernel/src/#@@LAYERS_KSRC@@#g' \
    -e 's#kernel/src/#core/kernel/src/#g' \
    -e 's#@@LAYERS_KSRC@@#core/kernel/src/#g' \
    -e 's#core/kernel/tests/#@@LAYERS_KTEST@@#g' \
    -e 's#kernel/tests/#core/kernel/tests/#g' \
    -e 's#@@LAYERS_KTEST@@#core/kernel/tests/#g' \
    "$f"
done

# ---- 6. `Database::sql*` is an extension trait now ------------------------
# `Database` belongs to `sekejap-core`; the orphan rule forbids an inherent
# `impl` on it from `sekejap-lang`, so `sql`, `sql_with`, `sql_prepare` and
# `sql_explain` are the `SqlDatabase` trait. A caller keeps every call site
# and adds the import. Skipped for the file that declares the trait, and for a
# file that already has the import, which is what makes the rule idempotent.
for f in "$@"; do
  [ -f "$f" ] || continue
  case "$f" in *.rs) ;; *) continue ;; esac
  grep -q 'trait SqlDatabase' "$f" && continue
  grep -q 'use sekejap_lang::SqlDatabase;' "$f" && continue
  grep -qE '\.sql(_with|_prepare|_explain)?\(' "$f" || continue
  n=$(grep -n '^use ' "$f" | head -1 | cut -d: -f1)
  [ -n "$n" ] || { echo "no \`use\` line to anchor the import: $f" >&2; continue; }
  sed -i '' -e "${n}s|^|use sekejap_lang::SqlDatabase;\n|" "$f"
done

# ---- 7. cargo invocations in the tool scripts ----------------------------
# The workspace is virtual now: `cargo test` with no package resolves to
# `default-members`, which is `core/engine`, so a bare `--test <name>` or
# `--lib` over the engine keeps working untouched. A `--bin` does not: every
# binary moved into `dist` or `bench`, and cargo needs the package named. The
# `-p sekejap-` guard is what makes these idempotent; a line naming binaries of
# both packages has to be split by hand first, and cargo says so if it is not.
DIST_BINS='lifecycle|collections|collection_inspect|control_tree_audit|pagewal_repair|recover|entry|sekejap'
BENCH_BINS='foundation_scale|foundation_space|pagewal_bench|pagewal_cap|battle50k|popsim|two_ways|sql_probe|people|index_format_fixture|format_fixture|format_compat|format_audit|multimodel_format_fixture|typed_admission_probe|v2_compat_fixture|v2_foundation_bench|g2_budget|hop1_budget|q3_budget|q6_budget|q7_budget|phase2_[a-z_]+'
for f in "$@"; do
  [ -f "$f" ] || continue
  case "$f" in *.sh|*.py) ;; *) continue ;; esac
  sed -E -i '' \
    -e 's|-p e4-prototype|-p sekejap-core|g' \
    -e 's|--package e4-prototype|--package sekejap-core|g' \
    "$f"
  sed -E -i '' \
    -e "/cargo (build|run)/{/-p sekejap-/!{/--bin ($DIST_BINS)/{/--bin ($BENCH_BINS)/!{s#cargo (build|run)#cargo \\1 -p sekejap-dist#;};};};}" \
    "$f"
  sed -E -i '' \
    -e "/cargo (build|run)/{/-p sekejap-/!{/--bin ($BENCH_BINS)/{s#cargo (build|run)#cargo \\1 -p sekejap-bench#;};};}" \
    "$f"
done

# ---- 8. patch headers ----------------------------------------------------
# A `.patch` in tools/ applies to the worktree, so its `a/`/`b/` paths move
# with the files. Rule 5 already rewrote the body; the two header lines carry
# the prefix and are done here.
for f in "$@"; do
  [ -f "$f" ] || continue
  case "$f" in *.patch) ;; *) continue ;; esac
  sed -E -i '' \
    -e 's#^(---|\+\+\+) (a|b)/core/kernel/#\1 \2/@@LAYERS_K@@#' \
    -e 's#^(---|\+\+\+) (a|b)/kernel/#\1 \2/core/kernel/#' \
    -e 's#@@LAYERS_K@@#core/kernel/#' \
    "$f"
done
