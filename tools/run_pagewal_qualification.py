"""Run frozen page-WAL controls/candidate; fixtures remain on authorized devices."""
import json,os,subprocess,sys
from pathlib import Path
base=Path(sys.argv[1]); mode=sys.argv[2]
env=dict(os.environ,TMPDIR=str(base/'tmp'),SQLITE_TMPDIR=str(base/'tmp'))
pi=str(base).startswith('<home>/')
def run(binary,args,log):
    cmd=[str(base/binary)]+[str(x) for x in args]
    if pi:cmd=['prlimit','--as=134217728','--']+cmd
    with log.open('w') as f:subprocess.run(cmd,env=env,stdout=f,stderr=subprocess.STDOUT,check=True)
if mode in ('bench','confirm','resume'):
    folder=base/'matrix'
    if mode=='bench':folder.mkdir()
    reports=[];failures=[]
    for label,n,cycles,reps in ([('sustain-400k',400000,12,3)] if mode=='confirm' else [('mixed-100k',100000,4,3),('sustain-400k',400000,12,3 if mode=='resume' else 1)]):
        for rep in range(reps):
            order=['e4','pagewal-v1','pagewal-v2','sqlite'];order=order[rep:]+order[:rep]
            pair=[]
            for arm in order:
                out=folder/(label+'-r'+str(rep+1))/arm;out.parent.mkdir(parents=True,exist_ok=True)
                engine='pagewal' if arm.startswith('pagewal') else arm
                if mode=='resume' and out.exists() and not (out/'report.json').exists():
                    failures.append(dict(label=label,rep=rep+1,arm=arm,path=str(out),reason='Existing failed attempt preserved; no successful report'))
                    print('PRESERVED FAILURE',label,rep+1,arm,flush=True);continue
                if not ((mode=='confirm' and rep==0) or (mode=='resume' and (out/'report.json').exists())):
                    assert not out.exists()
                    try:run('pagewal_bench' if arm=='pagewal-v2' else 'control',[out,engine,n,256,cycles,'mixed',1000],out.parent/(arm+'.log'))
                    except subprocess.CalledProcessError as e:
                        failures.append(dict(label=label,rep=rep+1,arm=arm,path=str(out),returncode=e.returncode))
                        print('FAILED',label,rep+1,arm,flush=True);continue
                p=out/'report.json';d=json.loads(p.read_text());assert d['rows']==n and len(d['phases'])==cycles+1
                assert d['reopen_verification']==d['phases'][-1]['verification']
                assert all(x['peak']['errors']==0 for x in d['phases'])
                pair.append(d);reports.append(dict(label=label,rep=rep+1,arm=arm,path=str(p)))
                print('completed',label,rep+1,arm,flush=True)
            for phase in range(cycles+1):assert len({x['phases'][phase]['verification']['crc32c'] for x in pair})==1
    (base/('confirm-results.json' if mode=='confirm' else 'matrix-results.json')).write_text(json.dumps(reports,indent=2)+'\n')
    if failures:(base/'matrix-failures.json').write_text(json.dumps(failures,indent=2)+'\n')
elif mode in ('cap','cap-short'):
    folder=base/mode;folder.mkdir();reports=[]
    for n in [10000,40000,100000]:
        for reader in (['short'] if mode=='cap-short' else ['none','held','rolling']):
            for arm in ['pagewal','sqlite']:
                out=folder/(str(n)+'-'+reader)/arm;out.parent.mkdir(parents=True,exist_ok=True)
                run('pagewal_cap_short' if mode=='cap-short' else 'pagewal_cap',[out,arm,n,reader],out.parent/(arm+'.log'))
                d=json.loads((out/'report.json').read_text());assert d['verified']
                if arm=='pagewal':assert d['observed_peak_logical']<=d['cap_logical']
                reports.append(dict(rows=n,reader=reader,arm=arm,path=str(out/'report.json')))
                print('completed cap',n,reader,arm,flush=True)
    (base/(mode+'-results.json')).write_text(json.dumps(reports,indent=2)+'\n')
else:raise SystemExit('bench|cap')
