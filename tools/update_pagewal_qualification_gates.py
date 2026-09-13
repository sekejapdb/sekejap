"""Record earned v2 evidence without turning partial law coverage into PASS."""
import json
from pathlib import Path
r=Path(__file__).resolve().parents[1];p=r/'docs/FOUNDATION_GATES.json';d=json.loads(p.read_text())
results=json.loads((r/'docs/PAGEWAL_QUALIFICATION_RESULTS.json').read_text())
time_limit=d.get('project_targets',{}).get('sqlite_time_ratio_exclusive',1.5)
historical={platform:{x['case']:x for x in json.loads((r/('docs/PAGEWAL_PI_RESULTS.json' if platform=='Pi' else 'docs/PAGEWAL_RESULTS.json')).read_text())['summary']} for platform in ('Mac','Pi')}
by={x['id']:x for x in d['laws']}
by['L1-MEM'].update(passed='8MiB ordinary cache, 16MiB WAL bound, persisted disk cap and eight-snapshot admission; Pi benchmark arms under 128MiB address-space limit',remaining='Aggregate cache/index/allocator accounting; complete record/reader policy and repair-resource admission')
by['L3-ATOMIC'].update(passed='356 injected boundary/mode cases, 712 ordinary/simulated-durability reopen checks; four actual checkpoint process exits; torn checkpoint extension defect fixed; source-preserving salvage checks',remaining='Full open/create and repair-destination I/O/interruption matrix, wider crash schedules/sector failure model and verified recovery publication')
by['L5-DAMAGE'].update(passed='Independent read-only scan and committed WAL overlay; current/candidate separation; leaf/overflow/root/meta/free fixtures; exact source preservation and verified 100K repair on Mac/Pi',remaining='Corrupt-WAL-region salvage currently refuses; rootless current-membership proof, typed schema dependencies, truncation/unreadable-extent and full mutation/fault corpus remain open')
by['L6-READ'].update(passed='Old/new functional snapshots, bounded admission, persisted-cap refusal, short-reader sustained progress under 2x on Mac/Pi',remaining='Cross-process readers, zero-coordination/latency/I/O regression gate; surviving reader still prevents replacement writer opening')
for x in d['workloads']:
    if x['id']=='W-CAP':x.update(remaining='Mac/Pi 10K/40K/100K update-only none/held/whole-round/5K-rolling cases measured, 48 arms; full mixed/resize/batch/allocated-space matrix remains pending')
    if x['id']=='W-REPAIR':x.update(remaining='Eight raw-KV fault cases and 100K verified repair on both platforms; corrupt WAL/rootless proof/typed/destination failure coverage remains pending')
    if x['id']=='W-READERS':x.update(remaining='Functional and cap-work progress measured; standalone reader latency/I/O and cross-process matrix pending')
for platform,rows in d['parity'].items():
    measured=results['platforms'][platform]['timing']
    for row in rows:
        if row['case'] in measured:
            x=measured[row['case']]
            row.update(time_target='PASS' if x['v2_over_sqlite_time']<time_limit else 'FAIL',size_target='PASS' if x['v2_over_sqlite_size']<=1.1 else 'FAIL',acceptance_repetitions='PASS',evidence='Page-WAL qualification v2; three rotations; owner accepts time <1.5x SQLite')
        else:
            old=historical[platform][row['case']];field='load_seconds' if row['case'].startswith('load-') else 'mutation_seconds'
            ratio=old['pagewal'][field]/old['sqlite'][field]
            row.update(time_target='PASS' if ratio<time_limit else 'FAIL',evidence='Historical v1 pilot reclassified under owner time threshold; not newly measured as standalone v2 workload')
d['release_blockers']=[{'id':'CONTROL-SCAN','status':'FAIL','remaining':'Frozen accepted Store control has a persistent round-6 scan/get disagreement. Source-preserving audit and ignored regression retained; cause and its shared-kernel implications unresolved.'}]
d['qualification_v2']={'source_archive_sha256':results['frozen_provenance']['artifacts']['source.tar.gz'],
    'full_mac_test_entries':360,'pi_selected_test_entries':27,'raw_reports':111,'failed_control_attempts':1,
    'ordinary_timing_completed':47,'contended_timing_excluded':16,'cap_diagnostics':48,
    'report':'docs/PAGEWAL_QUALIFICATION.md','result':'docs/PAGEWAL_QUALIFICATION_RESULTS.json'}
d['production_promotable']=False;d['verdict']='Retain v2 architecture candidate; no promotion. Safety coverage expanded; remaining law gates and control scan failure explicit.'
p.write_text(json.dumps(d,indent=2)+'\n')
