#!/usr/bin/env python3
"""Apply the reviewed proposal only after user approval; root required."""
import hashlib, json, os, pathlib, subprocess, sys
root=pathlib.Path('<scratch>')
manifest=json.loads((root/'manifest.json').read_text())
source=pathlib.Path(manifest['source'])
assert source==pathlib.Path('/boot/firmware/cmdline.txt')
assert os.geteuid()==0
rollback=sys.argv[1:] == ['--rollback']
assert sys.argv[1:] in ([], ['--rollback'])
before=(root/('cmdline.proposed' if rollback else 'cmdline.original')).read_bytes()
after=(root/('cmdline.original' if rollback else 'cmdline.proposed')).read_bytes()
assert source.read_bytes()==before, 'boot configuration changed since review; stop'
backup=source.with_name('cmdline.e4-before-20260911.txt')
if not rollback:
    with backup.open('xb') as f:
        f.write(before); f.flush(); os.fsync(f.fileno())
temporary=source.with_name('cmdline.e4-next.txt')
with temporary.open('xb') as f:
    f.write(after); f.flush(); os.fsync(f.fileno())
os.replace(temporary,source)
subprocess.run(['sync'],check=True)
assert source.read_bytes()==after
print(json.dumps({'applied':not rollback,'rolled_back':rollback,
    'source':str(source),'sha256':hashlib.sha256(after).hexdigest(),'reboot_required':True}))
