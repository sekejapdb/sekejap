"""Native baseline / encoding / packing ablation and selected-candidate qualification."""
import hashlib,json,os,subprocess,sys
from pathlib import Path
base=Path(sys.argv[1]).resolve();mode=sys.argv[2]
assert str(base) in ['<scratch>',
    '<scratch>']
assert mode in ['probe','qualify','tradeoff'];candidate=sys.argv[3] if len(sys.argv)>3 else 'combined'
assert candidate in ['compact','packing','combined']
out=base/mode;out.mkdir();(base/'tmp').mkdir(exist_ok=True)
env=dict(os.environ,TMPDIR=str(base/'tmp'),SQLITE_TMPDIR=str(base/'tmp'))
records=[];failures=[]
def save():
    (out/'results.json').write_text(json.dumps(dict(records=records,failures=failures,mode=mode,candidate=candidate),indent=2)+'\n')
def run(case,rep,arm,kind,args):
    path=out/f'{case}-r{rep+1}-{arm}'
    binary=base/'bin'/('baseline' if arm=='sqlite' else arm)/kind
    engine='sqlite' if arm=='sqlite' else 'pagewal'
    command=['prlimit','--as=134217728','--',str(binary),str(path),engine,*map(str,args)]
    with (out/(path.name+'.log')).open('w') as log:
        code=subprocess.run(command,env=env,stdout=log,stderr=subprocess.STDOUT).returncode
    if code:
        failures.append(dict(case=case,repetition=rep+1,arm=arm,code=code,path=str(path)));save()
        print('FAILED',case,arm,flush=True);return None
    raw=(path/'report.json').read_bytes();r=json.loads(raw)
    records.append(dict(case=case,repetition=rep+1,arm=arm,kind=kind,path=str(path),command=command,
        sha256=hashlib.sha256(raw).hexdigest(),binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),report=r))
    save();print('completed',case,rep+1,arm,flush=True)
    return r
cases=([(f'fixed-{n}-scattered','foundation_scale',[n,'scattered',1000]) for n in [100000,1000000]] if mode=='probe' else
    [(f'fixed-{n}-{loc}','foundation_scale',[n,loc,1000]) for n in [10000,100000,1000000] for loc in ['local','scattered']]
    + [('mixed-400k','pagewal_bench',[400000,256,12,'mixed',1000]),('resize-1k','pagewal_bench',[1000,256,6,'resize',1000])])
if mode=='tradeoff': cases=[c for c in cases if c[1]=='pagewal_bench']
for rep in range(3 if mode=='qualify' else 1):
    for case,kind,args in cases:
        arms=['baseline','compact','packing','combined','sqlite'] if mode!='qualify' else ['baseline',candidate,'sqlite']
        arms=arms[rep:]+arms[:rep];pair=[]
        for arm in arms:
            r=run(case,rep,arm,kind,args)
            if r is not None:
                oracle=r['verification'] if kind=='foundation_scale' else r['reopen_verification']
                pair.append(oracle)
                if kind=='pagewal_bench':
                    assert r['reopen_verification']==r['phases'][-1]['verification']
                    assert all(p['peak']['errors']==0 for p in r['phases'])
        assert all(v==pair[0] for v in pair),'oracle mismatch'
if mode in ['qualify','tradeoff']:
    for reader in ['none','held','rolling']:
        for arm in (['baseline',candidate,'sqlite'] if mode=='qualify' else ['baseline','compact','packing','combined','sqlite']):
            r=run('cap-10k-'+reader,0,arm,'pagewal_cap',[10000,reader])
            if r is not None:
                assert r['verified']
                if arm!='sqlite':assert r['observed_peak_logical']<=r['cap_logical']
assert not failures,failures
print('COMPLETE',mode,flush=True)
