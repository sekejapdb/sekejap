#!/usr/bin/env bash
# Reusable Linux test entrypoint for the Phase-2 candidate.
# Database tests only use the caller-authorized TMPDIR supplied with --tmpdir.
set -uo pipefail

usage() {
  cat <<'EOF'
Usage:
  tools/phase2_qualify.sh --profile lean|full --config default|retained|both \
    --tmpdir ABSOLUTE_EXISTING_DIR --output ABSOLUTE_NEW_DIR
  tools/phase2_qualify.sh --validate-only [the same arguments]

Profiles:
  lean  Small Phase-2 semantics, lifecycle, admission, injected faults,
        query, current-reader, verifier, and rebuild tests. The named
        >65,536-result pagination tests and scaling tests are skipped.
  full  The complete Cargo workspace test suite. This is still not the
        external compatibility, crash, or benchmark qualification.

Configurations:
  default   No Cargo features.
  retained  compact-cells,sqlite-balance,keyspace-append,slotref-split.
  both      Run default and retained sequentially.

Passing --tmpdir asserts that the caller has authorized database artifacts in
that directory. No default temporary directory is used. Logs, commands, and
status are retained under --output; test databases are retained under a new
subdirectory of --tmpdir.
EOF
}

die() {
  printf 'phase2_qualify: %s\n' "$*" >&2
  exit 2
}

profile=
configuration=
tmp_root=
output=
validate_only=0

while (($#)); do
  case "$1" in
    --profile)
      (($# >= 2)) || die '--profile requires a value'
      profile=$2
      shift 2
      ;;
    --config)
      (($# >= 2)) || die '--config requires a value'
      configuration=$2
      shift 2
      ;;
    --tmpdir)
      (($# >= 2)) || die '--tmpdir requires a value'
      tmp_root=$2
      shift 2
      ;;
    --output)
      (($# >= 2)) || die '--output requires a value'
      output=$2
      shift 2
      ;;
    --validate-only)
      validate_only=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1" ;;
  esac
done

case "$profile" in lean|full) ;; *) die '--profile must be lean or full' ;; esac
case "$configuration" in default|retained|both) ;; *) die '--config must be default, retained, or both' ;; esac
[[ "$tmp_root" == /* ]] || die '--tmpdir must be absolute'
[[ "$output" == /* ]] || die '--output must be absolute'
[[ "$tmp_root" != / ]] || die '--tmpdir cannot be the filesystem root'
[[ "$output" != / ]] || die '--output cannot be the filesystem root'
[[ "$tmp_root" != "$output" ]] || die '--tmpdir and --output must differ'

retained_features='compact-cells,sqlite-balance,keyspace-append,slotref-split'
if ((validate_only)); then
  printf 'VALID profile=%s config=%s tmpdir=%s output=%s\n' \
    "$profile" "$configuration" "$tmp_root" "$output"
  printf 'No database or Cargo command was run.\n'
  exit 0
fi

[[ "$(uname -s)" == Linux ]] || die 'database qualification is Linux-only'
[[ -d "$tmp_root" ]] || die '--tmpdir must already exist'
[[ -w "$tmp_root" ]] || die '--tmpdir must be writable'
[[ ! -e "$output" ]] || die '--output must be a new path'
[[ -d "$(dirname "$output")" ]] || die '--output parent must already exist'

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
repo=$(cd "$script_dir/.." && pwd -P)
cd "$repo" || die 'cannot enter repository root'

umask 077
mkdir "$output" || die 'cannot create output directory'
run_tmp=$(mktemp -d "$tmp_root/phase2-qualify.XXXXXXXX") || die 'cannot create test TMPDIR'
export TMPDIR=$run_tmp
export SQLITE_TMPDIR=$TMPDIR
export RUST_BACKTRACE=1

status_file=$output/status.tsv
printf 'group\tconfiguration\tcargo_exit_code\tlog_exit_code\tharness_exit_code\telapsed_seconds\tlog\treason\n' > "$status_file"
overall=0
completed=0

write_summary() {
  local shell_status=$1
  local result=PASS
  if ((overall != 0 || shell_status != 0)); then
    result=FAIL
  fi
  {
    printf 'result=%s\n' "$result"
    printf 'profile=%s\n' "$profile"
    printf 'configuration=%s\n' "$configuration"
    printf 'completed_groups=%s\n' "$completed"
    printf 'test_tmpdir=%s\n' "$run_tmp"
    printf 'status_tsv=%s\n' "$status_file"
    printf 'exit_code=%s\n' "$overall"
  } > "$output/summary.txt"
}

on_exit() {
  local shell_status=$?
  if ((shell_status != 0 && overall == 0)); then
    overall=$shell_status
  fi
  write_summary "$shell_status"
}
trap on_exit EXIT
trap 'overall=130; exit 130' INT TERM HUP

{
  printf 'repository=%s\n' "$repo"
  printf 'profile=%s\nconfiguration=%s\n' "$profile" "$configuration"
  printf 'tmpdir=%s\noutput=%s\n' "$run_tmp" "$output"
  printf 'sqlite_tmpdir=%s\n' "$SQLITE_TMPDIR"
  printf 'uname='; uname -a
  printf 'rustc='; rustc --version
  printf 'cargo='; cargo --version
  printf 'git_head='; git rev-parse HEAD 2>/dev/null || printf 'unavailable\n'
  printf 'retained_features=%s\n' "$retained_features"
  printf 'started_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'source_status_begin\n'
  git status --short 2>/dev/null || true
  printf 'source_status_end\n'
} > "$output/metadata.txt"

run_group() {
  local label=$1
  local config=$2
  local require_tests=$3
  shift 3
  local log=$output/$label.log
  local command_file=$output/$label.command
  local started ended cargo_rc log_rc harness_rc reason
  printf '%q ' "$@" > "$command_file"
  printf '\n' >> "$command_file"
  started=$(date +%s)
  "$@" 2>&1 | tee "$log"
  local pipeline_status=("${PIPESTATUS[@]}")
  cargo_rc=${pipeline_status[0]}
  log_rc=${pipeline_status[1]:-1}
  harness_rc=0
  reason=ok
  if ((log_rc != 0)); then
    harness_rc=1
    reason=log-write-failed
  fi
  if [[ "$require_tests" == yes ]] && ((cargo_rc == 0)); then
    if ! grep -Eq '^running [1-9][0-9]* tests?$' "$log"; then
      harness_rc=1
      if [[ "$reason" == ok ]]; then
        reason=zero-tests-executed
      else
        reason=$reason,zero-tests-executed
      fi
    fi
  fi
  ended=$(date +%s)
  if ((cargo_rc != 0)); then
    if [[ "$reason" == ok ]]; then
      reason=cargo-failed
    else
      reason=$reason,cargo-failed
    fi
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$label" "$config" "$cargo_rc" "$log_rc" "$harness_rc" \
    "$((ended - started))" "$log" "$reason" >> "$status_file"
  completed=$((completed + 1))
  if ((cargo_rc != 0 || log_rc != 0 || harness_rc != 0)); then
    overall=1
  fi
}

run_configuration() {
  local config=$1
  local feature_args=()
  if [[ "$config" == retained ]]; then
    feature_args=(--features "$retained_features")
  fi
  local cargo_base=(cargo test --release --locked --offline --no-fail-fast "${feature_args[@]}")

  if [[ "$profile" == full ]]; then
    run_group "$config-full-workspace" "$config" no \
      "${cargo_base[@]}" --workspace -- --test-threads=1
    return
  fi

  run_group "$config-small-semantics" "$config" no "${cargo_base[@]}" \
    --test collections \
    --test index_scalar_oracle \
    --test graph_collections \
    --test index_vector \
    --test index_vector_quantized \
    --test index_spatial \
    --test index_text \
    -- --test-threads=1

  run_group "$config-lifecycle" "$config" no "${cargo_base[@]}" \
    --test index_lifecycle -- --test-threads=1

  run_group "$config-admission" "$config" no "${cargo_base[@]}" \
    --test index_admission \
    --test graph_admission \
    --test vector_admission \
    --test spatial_admission \
    --test text_admission \
    -- --test-threads=1

  run_group "$config-family-write-faults" "$config" yes "${cargo_base[@]}" \
    --lib write_boundaries_preserve -- --test-threads=1
  run_group "$config-quantized-write-faults" "$config" yes "${cargo_base[@]}" \
    --lib quantized_catalog_crud_build_and_drop_are_atomic_at_every_key_write \
    -- --test-threads=1

  run_group "$config-small-scalar-query" "$config" no "${cargo_base[@]}" \
    --test query_scalar -- --test-threads=1 \
    --skip scalar_driver_streams_and_pages_more_than_65536_matches_completely
  run_group "$config-small-multimodel-query" "$config" no "${cargo_base[@]}" \
    --test query_multimodel -- --test-threads=1 \
    --skip text_and_spatial_drivers_page_more_than_65536_matches_without_a_result_cap

  run_group "$config-current-source-reader" "$config" yes "${cargo_base[@]}" \
    --lib current_reader::tests -- --test-threads=1
  run_group "$config-index-verifier-rebuild" "$config" no "${cargo_base[@]}" \
    --test index_verifier --test index_rebuild -- --test-threads=1
}

case "$configuration" in
  default) run_configuration default ;;
  retained) run_configuration retained ;;
  both)
    run_configuration default
    run_configuration retained
    ;;
esac

printf 'finished_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$output/metadata.txt"
exit "$overall"
