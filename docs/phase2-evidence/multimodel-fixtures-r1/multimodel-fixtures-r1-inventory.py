import hashlib,json,sys
from pathlib import Path
files=sorted([*Path('src').rglob('*.rs'),*Path('kernel/src').rglob('*.rs'),*Path('tests').rglob('*.rs'),*Path('kernel/tests').rglob('*.rs'),Path('Cargo.toml'),Path('Cargo.lock'),Path('kernel/Cargo.toml')])
Path(sys.argv[1]).write_text(json.dumps({str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in files},sort_keys=True,indent=2)+'\n')
for p in files:p.touch()
