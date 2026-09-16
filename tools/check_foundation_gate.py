"""Fail closed: an isolated pager cannot be promoted with missing law gates."""
import json,sys
from pathlib import Path
root=Path(__file__).resolve().parents[1]
d=json.loads((root/'docs/FOUNDATION_GATES.json').read_text())
assert d['version']=='F1' and [v['law'] for v in d['laws']]==list(range(1,9))
blocked=[v['id']+': '+v['status'] for v in d['laws'] if v['status']!='PASS']
blocked += [v['id']+': '+v['status'] for v in d['workloads'] if v['status']!='PASS']
blocked += [v['id']+': '+v['status'] for v in d.get('release_blockers',[]) if v['status']!='PASS']
blocked += [k+': '+v for k,v in d['fixtures'].items() if 'pending' in v.lower()]
for platform,rows in d['parity'].items():
    for row in rows:
        for field in ('time_target','size_target','acceptance_repetitions'):
            if row[field]!='PASS':blocked.append(platform+'/'+row['case']+'/'+field+': '+row[field])
promotable=not blocked
assert d['production_promotable']==promotable
print(json.dumps(dict(promotable=promotable,blockers=blocked),indent=2))
sys.exit(0 if promotable else 1)
