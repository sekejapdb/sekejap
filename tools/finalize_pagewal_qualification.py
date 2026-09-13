"""Preserve final qualification decision, documentation and cleanup evidence."""
import hashlib,json,shutil
from pathlib import Path
r=Path(__file__).resolve().parents[1];b=Path('<scratch>')
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
cleanup=json.loads((b/'cleanup.json').read_text())
assert all(not (b/x['path']).exists() for x in cleanup['removed'])
assert all((b/x['path']).is_dir() for x in cleanup['retained'])
summary=json.loads((r/'docs/PAGEWAL_QUALIFICATION_RESULTS.json').read_text())
for x in summary['raw_reports']:assert sha(Path(x['path']))==x['sha256'],x['path']
audit=json.loads((b/'control-audit.json').read_text());assert (audit['rows'],audit['mismatches'],audit['ordering_errors'],audit['point_395080_version'])==(399999,3,1,6)
assert 'left: (399999, 3, 1)' in (b/'accepted-control-regression-red.log').read_text()
combined={'Mac':cleanup,'Pi':{'status':'pending: SSH unreachable after completed tests/benchmarks and evidence copy',
    'data_removed':False,'results_preserved_locally':True,'next_command':'tools/cleanup_pagewal_qualification.py PI_ARTIFACT_ROOT plan, then delete after reviewing the scoped manifest'}}
(r/'docs/PAGEWAL_QUALIFICATION_CLEANUP.json').write_text(json.dumps(combined,indent=2)+'\n')
decision={'decision':'Retain page-WAL v2 isolated; not promoted or shipped','source_archive_sha256':summary['frozen_provenance']['artifacts']['source.tar.gz'],
    'production_implementation_unchanged':True,'existing_production_files_verified':94,'laws_unchanged':True,
    'new_known_failure_regression':{'path':'tests/accepted_control_scan_regression.rs','sha256':sha(r/'tests/accepted_control_scan_regression.rs'),'default':'ignored with explicit reason; explicit run reproduces failure'},
    'completed_reports':111,'completed_uncontended_timing_arms':47,'failed_control_attempts':1,'contended_timing_excluded':16,'cap_diagnostics':48,
    'control_audit':audit,'cleanup':{'mac_removed_allocated_bytes':cleanup['removed_allocated_bytes'],'pi':'pending SSH access'},
    'f1_gate':json.loads((b/'promotion-gate.json').read_text()),'report':'docs/PAGEWAL_QUALIFICATION.md'}
(b/'decision.json').write_text(json.dumps(decision,indent=2)+'\n')
doc=r/'docs/PAGEWAL_QUALIFICATION.md';text=doc.read_text().split('\n## Loop decision and cleanup\n')[0]
text+='''
## Loop decision and cleanup

Retain page-WAL v2 as the isolated architectural candidate. Its ordinary-row
density is unchanged, and the safety additions preserve the previous pilot's
performance at the measured scale. The owner accepts elapsed time below 1.50×
SQLite; the measured ordinary workloads pass on Mac and Pi. Resize/overflow and full hybrid
collection qualification remain separate, unfinished gates.

The control's persistent scan/get inconsistency is an additional release blocker,
with its failing fixture and explicit ignored regression preserved. Neither
successful repetitions nor the full candidate-suite pass clear that failure.
No production implementation, SQL/service interface or new multimodel index
was shipped. The seven laws remain unchanged.

Removed 56 verified disposable Mac databases, totalling **2,533,228,544 allocated
bytes** (2,483,456,848 logical bytes). Retained the failed control, four final
400K engines, the 100K repair source/destination, long-reader cap evidence,
all 111 reports, logs, source archives and binaries. Allocated-file accounting
is not a promise of an identical filesystem free-space change.

Pi tests, all comparisons and repair completed, and their results were copied
before SSH became unreachable. **Pi data was not deleted**; cleanup remains
pending connectivity. See `PAGEWAL_QUALIFICATION_CLEANUP.json` for the exact
Mac deletion manifest and Pi status.
'''
doc.write_text(text)
for name in ['README.md','CONTRACT.md','docs/FOUNDATION_TEST_STANDARD.md','docs/FOUNDATION_GATES.json',
             'docs/PAGEWAL_QUALIFICATION.md','docs/PAGEWAL_QUALIFICATION_RESULTS.json','docs/PAGEWAL_QUALIFICATION_CLEANUP.json']:
    shutil.copy2(r/name,b/Path(name).name)
tools=b/'tools';tools.mkdir(exist_ok=True)
for name in ['run_pagewal_qualification.py','record_pagewal_qualification.py','update_pagewal_qualification_gates.py',
             'cleanup_pagewal_qualification.py','finalize_pagewal_qualification.py','pagewal_qualification_pi.sh',
             'pagewal_qualification_extra_pi.sh','pagewal_control_audit.rs','check_foundation_gate.py']:
    shutil.copy2(r/'tools'/name,tools/name)
shutil.copy2('/tmp/e4-pagewal-qualify/src/bin/pagewal_cap.rs',b/'short-cap.rs')
shutil.copy2(r/'tests/accepted_control_scan_regression.rs',b/'accepted_control_scan_regression.rs')
shutil.copy2('/tmp/e4-commit-candidate/target/release/pagewal-control-audit',b/'control-audit')
audit_source=b/'audit-source';audit_source.mkdir(exist_ok=True)
for name in ['Cargo.toml','Cargo.lock']:
    shutil.copy2(Path('/tmp/e4-pagewal-audit')/name,audit_source/name)
shutil.copy2('/tmp/e4-pagewal-audit/src/main.rs',audit_source/'main.rs')
extra={str(p.relative_to(b)):sha(p) for p in [b/'short-cap.rs',b/'pagewal_cap_short',b/'control-audit',b/'accepted_control_scan_regression.rs']}
(b/'additional-provenance.json').write_text(json.dumps(extra,indent=2)+'\n')
print(json.dumps({'verified_raw_report_hashes':len(summary['raw_reports']),'mac_removed_databases':56,'mac_removed_allocated':cleanup['removed_allocated_bytes'],'pi_cleanup':'pending SSH','promoted':False},indent=2))
