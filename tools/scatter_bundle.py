"""Archive exact source variants for native Pi/server builds; no databases."""
import argparse
import io
from pathlib import Path
import subprocess
import tarfile

p=argparse.ArgumentParser();p.add_argument('output',type=Path);p.add_argument('variant');a=p.parse_args()
root=Path(__file__).resolve().parents[1]
names=subprocess.check_output(['git','ls-files','--cached','--others','--exclude-standard','-z'],cwd=root).decode().split('\0')
with tarfile.open(a.output,'w:gz') as archive:
    for name in sorted(set(names)):
        if not name or not (root/name).is_file():continue
        archive.add(root/name,arcname=name,recursive=False)
    b=(a.variant+'\n').encode();entry=tarfile.TarInfo('VARIANT');entry.size=len(b)
    archive.addfile(entry,io.BytesIO(b))
