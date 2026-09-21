"""Construct ablation archives from preserved baseline, compact and combined snapshots."""
import difflib,io,tarfile
from pathlib import Path

def read(name):
    with tarfile.open('/tmp/e4-scatter-'+name+'.tar.gz') as t:
        return {m.name:t.extractfile(m).read() for m in t.getmembers() if m.isfile()}
b,a,c=map(read,['baseline','compact','combined'])
old=b['core/kernel/src/btree.rs'].decode();compact=a['core/kernel/src/btree.rs'].decode();combined=c['core/kernel/src/btree.rs'].decode()
ol=old.splitlines(True);al=compact.splitlines(True);packing=combined;check=compact
for kind,_,_,lo,hi in difflib.SequenceMatcher(a=ol,b=al,autojunk=False).get_opcodes():
    if kind=='equal':continue
    assert kind=='insert',kind
    addition=''.join(al[lo:hi]);assert packing.count(addition)==1
    packing=packing.replace(addition,'',1);check=check.replace(addition,'',1)
assert check==old
p=dict(b);p['core/kernel/src/btree.rs']=packing.encode()
shared=['tools/run_scatter_loop.py','tools/scatter_native_build.sh','tools/scatter_variants.py']
for name,files in [('baseline',b),('compact',a),('packing',p),('combined',c)]:
    for path in shared:files[path]=Path(path).read_bytes()
    if name=='compact':files['core/kernel/tests/compact_cell_bounds.rs']=c['core/kernel/tests/compact_cell_bounds.rs']
    # Identical test-only path permission on all variants. No engine change.
    source=files['core/engine/tests/schema_recovery.rs'].decode()
    source=source.replace('|| p.starts_with("<scratch>")',
        '|| p.starts_with("<scratch>") || p.starts_with("<scratch>")')
    source=source.replace('|| tmp.starts_with("<scratch>")',
        '|| tmp.starts_with("<scratch>") || tmp.starts_with("<scratch>")')
    files['core/engine/tests/schema_recovery.rs']=source.encode();files['VARIANT']=(name+'\n').encode()
    with tarfile.open('/tmp/e4-scatter-ready-'+name+'.tar.gz','w:gz') as t:
        for path,data in sorted(files.items()):
            m=tarfile.TarInfo(path);m.size=len(data);m.mode=0o644;t.addfile(m,io.BytesIO(data))
