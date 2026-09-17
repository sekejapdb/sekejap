#!/usr/bin/env bash
set -euo pipefail
BASE=<scratch>
RUN=$BASE/phase2-native-20260917-r1
export PATH=$BASE/toolchain/bin:$PATH CARGO_HOME=$RUN/cargo-home CARGO_BUILD_JOBS=1
export TMPDIR=$RUN/tmp SQLITE_TMPDIR=$RUN/tmp
trap 'code=$?; echo "$code" > "$RUN/run.exit"' EXIT
test ! -e "$RUN/run.started"
date -Is > "$RUN/run.started"
mkdir "$RUN/tmp" "$RUN/logs" "$RUN/bin" "$RUN/cargo-home" "$RUN/target"
uname -a > "$RUN/platform.txt"
rustc --version --verbose > "$RUN/rustc.txt"
python3 - "$RUN" <<'PY'
import hashlib,json,pathlib,sys
r=pathlib.Path(sys.argv[1])
for label in ('current','old-five'):
 manifest=r/f'{label}-source.json';root=r/('src' if label=='current' else 'old-five-src')
 for p,h in json.loads(manifest.read_text()).items():
  path=root/p
  assert hashlib.sha256(path.read_bytes()).hexdigest()==h,(label,p)
  if path.suffix=='.rs' or path.name.startswith('Cargo.'):path.touch()
 config=root/'.cargo';config.mkdir(exist_ok=False)
 (config/'config.toml').write_text('[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "'+str(r/'vendor')+'"\n')
PY
cd "$RUN/old-five-src"
export CARGO_TARGET_DIR=$RUN/target/old-five
export E4_COMPAT_ENGINE_REVISION=pi-preserved-five-family-$(sha256sum "$RUN/old-five-source.json" | cut -d' ' -f1)
cargo build --release --locked --offline --bin typed_admission_probe > "$RUN/logs/build-old-five.log" 2>&1
cp "$CARGO_TARGET_DIR/release/typed_admission_probe" "$RUN/bin/old-five"
printf 'old-five-build\tPASS\n' >> "$RUN/stages.tsv"
cd "$RUN/src"
export CARGO_TARGET_DIR=$RUN/target/current
export E4_COMPAT_ENGINE_REVISION=pi-phase2-query-candidate-rollback-$(sha256sum "$RUN/current-source.json" | cut -d' ' -f1)
for mode in default retained; do
 flags=()
 if [ "$mode" = retained ]; then flags=(--features compact-cells,sqlite-balance,keyspace-append,slotref-split); fi
 cargo build --release --locked --offline "${flags[@]}" --bin phase2_format_fixture --bin multimodel_format_fixture --bin phase2_lifecycle_fixture > "$RUN/logs/build-$mode.log" 2>&1
 for binary in phase2_format_fixture multimodel_format_fixture phase2_lifecycle_fixture; do
  cp "$CARGO_TARGET_DIR/release/$binary" "$RUN/bin/$binary-$mode"
 done
 printf 'helper-build-%s\tPASS\n' "$mode" >> "$RUN/stages.tsv"
done
sha256sum "$RUN/bin/"* > "$RUN/binaries.sha256"
python3 - "$RUN" <<'PY'
import hashlib,json,pathlib,subprocess,sys
r=pathlib.Path(sys.argv[1]);summary=[]
configs=[('graph','phase2_format_fixture','phase2_format_compat.py',4),('multimodel','multimodel_format_fixture','multimodel_format_compat.py',20),('lifecycle','phase2_lifecycle_fixture','phase2_lifecycle_compat.py',80)]
for label,binary,driver,count in configs:
 bins={mode:r/'bin'/f'{binary}-{mode}' for mode in ('default','retained')}
 work=r/f'{label}-fixtures'
 command=['python3',f'tools/{driver}','--default-bin',str(bins['default']),'--retained-bin',str(bins['retained']),'--work',str(work)]
 if label!='graph':command.extend(['--older-probe','old-five',str(r/'bin/old-five'),'31'])
 with (r/f'logs/{label}-fixtures.log').open('w') as log:subprocess.run(command,check=True,stdout=log,stderr=subprocess.STDOUT)
 prior=work/'REPORT.json';d=json.loads(prior.read_text());assert d['result']=='PASS' and len(d['fixtures'])==count
 with (r/'stages.tsv').open('a') as stages:stages.write(label+'-fixtures\tPASS\n')
 build={'format':'phase2-rollback-build-v1','source_files_sha256':json.loads((r/'current-source.json').read_text()),'binaries':d['binaries']}
 (r/f'{label}-build.json').write_text(json.dumps(build,sort_keys=True,indent=2)+'\n')
 cycles=r/f'{label}-cycles'
 command=['python3','tools/phase2_rollback_compat.py','--qualification-report',str(prior),'--report-sha256',hashlib.sha256(prior.read_bytes()).hexdigest(),'--corpus',str(work/'corpus'),'--qualification-kind','cross-build','--work',str(cycles)]
 for mode,path in bins.items():command.extend(['--baseline-bin',mode,str(path),'--comparison-bin',mode,str(path)])
 with (r/f'logs/{label}-cycles.log').open('w') as log:subprocess.run(command,check=True,stdout=log,stderr=subprocess.STDOUT)
 p=cycles/'REPORT.json';d=json.loads(p.read_text());assert d['result']=='PASS' and d['protected_unchanged'] and len(d['arms'])==count*4
 summary.append({'family':label,'sources':count,'cycles':count*4,'report':str(p),'sha256':hashlib.sha256(p.read_bytes()).hexdigest()})
 with (r/'stages.tsv').open('a') as stages:stages.write(label+'-cycles\tPASS\n')
(r/'cycle-summary.json').write_text(json.dumps({'result':'PASS','scope':'ARM same-revision cross-build candidate preparation, not performance or release acceptance','families':summary},indent=2)+'\n')
PY
for mode in default retained; do
 flags=()
 if [ "$mode" = retained ]; then flags=(--features compact-cells,sqlite-balance,keyspace-append,slotref-split); fi
 cargo test --release --locked --offline "${flags[@]}" --test query_scalar --test query_multimodel -- --test-threads=1 > "$RUN/logs/query-$mode.log" 2>&1
 printf 'query-%s\tPASS\n' "$mode" >> "$RUN/stages.tsv"
done
python3 - "$RUN" <<'PY'
import hashlib,json,pathlib,sys
r=pathlib.Path(sys.argv[1])
for label in ('current','old-five'):
 root=r/('src' if label=='current' else 'old-five-src')
 for p,h in json.loads((r/f'{label}-source.json').read_text()).items():assert hashlib.sha256((root/p).read_bytes()).hexdigest()==h,(label,p)
print('PASS: sources unchanged, ARM compatibility cycles and query suites complete')
PY
