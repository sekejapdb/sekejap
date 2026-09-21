"""Build frozen one-sibling ablations using the archived ordinary-cell delta."""
import difflib, io, subprocess, tarfile
from pathlib import Path

def read(path):
    with tarfile.open(path) as t:
        return {m.name: t.extractfile(m).read() for m in t.getmembers() if m.isfile()}

full = read('/tmp/e4-pair-working.tar.gz')
base = read('/tmp/e4-scatter-baseline.tar.gz')
compact = read('/tmp/e4-scatter-compact.tar.gz')
old = base['core/kernel/src/btree.rs'].decode().splitlines(True)
new = compact['core/kernel/src/btree.rs'].decode().splitlines(True)
pair = dict(full)
source = pair['core/kernel/src/btree.rs'].decode()
for kind, _, _, lo, hi in difflib.SequenceMatcher(a=old, b=new, autojunk=False).get_opcodes():
    if kind == 'equal': continue
    assert kind == 'insert'
    addition = ''.join(new[lo:hi]); assert source.count(addition) == 1
    source = source.replace(addition, '', 1)
pair['core/kernel/src/btree.rs'] = source.encode()
pair['core/kernel/src/verify.rs'] = subprocess.check_output(['git', 'show', 'HEAD:core/kernel/src/verify.rs'])
pair.pop('core/kernel/tests/compact_cell_bounds.rs')
for name, files in [('pair', pair), ('compact-pair', full)]:
    files['VARIANT'] = (name + '\n').encode()
    with tarfile.open('/tmp/e4-pair-' + name + '.tar.gz', 'w:gz') as t:
        for path, data in sorted(files.items()):
            m = tarfile.TarInfo(path); m.size = len(data); m.mode = 0o644
            t.addfile(m, io.BytesIO(data))
