#!/usr/bin/env python3
"""Stage a boot change for review. Does not change /boot or reboot the Pi."""
import difflib, hashlib, json, pathlib
root=pathlib.Path('<scratch>')
source=pathlib.Path('/boot/firmware/cmdline.txt')
original=source.read_bytes()
assert len(original.splitlines())==1
assert b'cgroup_enable=memory' not in original.split()
proposed=original.rstrip(b'\r\n')+b' cgroup_enable=memory\n'
out=root/'boot-proposal'
out.mkdir()
(out/'cmdline.original').write_bytes(original)
(out/'cmdline.proposed').write_bytes(proposed)
(out/'change.diff').write_text(''.join(difflib.unified_diff(
    (original.decode().rstrip("\r\n")+"\n").splitlines(True),proposed.decode().splitlines(True),
    fromfile=str(source),tofile=str(source))))
(out/'manifest.json').write_text(json.dumps({'source':str(source),
    'original_sha256':hashlib.sha256(original).hexdigest(),
    'proposed_sha256':hashlib.sha256(proposed).hexdigest(),
    'applied':False,'requires_reboot':True},indent=2)+'\n')
print((out/'change.diff').read_text())
