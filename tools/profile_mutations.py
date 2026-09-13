"""Sample a baseline writer after load, retaining the diagnostic separately."""
import json, os, subprocess
from pathlib import Path

base = Path('<scratch>')
env = dict(os.environ, TMPDIR=str(base/'tmp'), SQLITE_TMPDIR=str(base/'tmp'), COLLECTION_BATCH='1000')
log = base/'profile-baseline.log'
assert not log.exists()
with log.open('w') as out:
    proc = subprocess.Popen([str(base/'baseline'), str(base/'profile-baseline'),
                             'e4','400000','off','2','mixed','none'],
                            stdout=subprocess.PIPE, stderr=out, text=True, env=env)
    sample = None
    for line in proc.stdout:
        out.write(line); out.flush()
        if sample is None and line.startswith('{') and json.loads(line).get('cycle') == 0:
            sample = subprocess.Popen(['/usr/bin/sample', str(proc.pid), '3', '1',
                                       '-file',str(base/'mutation-sample.txt')], stdout=out, stderr=out)
    code = proc.wait()
    assert code == 0, code
    assert sample is not None and sample.wait() == 0
print('Mutation profile and complete diagnostic row oracle retained')
