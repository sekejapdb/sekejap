#!/bin/bash
set -euo pipefail
root=<scratch>
matrix="$root/artifacts/matrix-address-space"
test ! -e "$root/artifacts/postboot-verification.json"
printf '%s  %s\n' 03fe9026ddfb32fd5fab03f3313360f9c2910dc047cb5494c1fc197f8f7c3625 "$root/artifacts/lifecycle-pi" | sha256sum --check
export PATH="$root/toolchain/bin:$PATH" CARGO_HOME="$root/cargo-home"
export TMPDIR="$root/tmp" SQLITE_TMPDIR="$root/tmp" CARGO_BUILD_JOBS=2
cd "$root/src"
cargo build -p sekejap-dist --offline --release --features sqlite-balance,compact-cells --bin lifecycle > "$root/artifacts/postboot-verifier-build.log" 2>&1
cp target/release/lifecycle "$root/artifacts/lifecycle-postboot-verifier"
sha256sum "$root/artifacts/lifecycle-postboot-verifier" > "$root/artifacts/lifecycle-postboot-verifier.sha256"
prlimit --as=134217728 -- "$root/artifacts/lifecycle-postboot-verifier" --verify-saved "$matrix" > "$root/artifacts/postboot-verification.json" 2> "$root/artifacts/postboot-verification.log"
python3 - "$root/artifacts/postboot-verification.json" <<'PY'
import json,sys
r=json.load(open(sys.argv[1]))
assert r['complete'] and len(r['pairs'])==10
assert r['process_memory_limits']['address_space_soft_hard']==['134217728']*2
PY
# Keep the interrupted pair, log and original start time as evidence.
held="$root/artifacts/interrupted-user-poweroff"
mkdir "$held"
name=ordinary-updates-400000
mv "$matrix/$name" "$matrix/$name.log" "$matrix/$name.started" "$held/"
python3 "$root/src/tools/check_sustained.py" "$matrix" > "$root/artifacts/postboot-saved-results-audit.json"
{
  date -Is
  uptime
  uname -a
  cat /proc/cmdline
  cat /sys/fs/cgroup/cgroup.controllers
  df -h "$root"
  if command -v vcgencmd >/dev/null; then vcgencmd get_throttled; vcgencmd measure_temp; fi
} > "$root/artifacts/hardware-after-reboot.txt"
bash "$root/resource_pi_matrix.sh" address-space --resume
