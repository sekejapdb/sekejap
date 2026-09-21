#!/usr/bin/env python3
"""Prove resource guards are reached; always restore the source afterward."""
import json, os, pathlib, subprocess

root = pathlib.Path('<scratch>')
source = pathlib.Path('core/kernel/src/pool.rs')
original = source.read_text()
mutations = [
    ('data-admission', 'if !reuse && i.next_page as u64 >= l.data_bytes / PAGE_SIZE as u64',
     'if false && !reuse && i.next_page as u64 >= l.data_bytes / PAGE_SIZE as u64',
     'refusal_preserves_acknowledged_rows_and_old_reader_with_no_replay_growth'),
    ('duplicate-free', '|| !seen.insert(p)', '|| { let _ = seen.insert(p); false }',
     'duplicate_freelist_pages_are_rejected_before_installing_reuse_state'),
]
results=[]
try:
    for name, old, new, test in mutations:
        assert original.count(old)==1, name
        source.write_text(original.replace(old,new))
        log=root/f'mutation-{name}.log'
        with log.open('w') as out:
            status=subprocess.run(['cargo','test','-p','kernel','--features','sqlite-balance,compact-cells',
                '--test','resource_limits',test,'--','--exact','--nocapture'],stdout=out,stderr=subprocess.STDOUT,
                env={**os.environ,'TMPDIR':str(root/'tmp')}).returncode
        text=log.read_text()
        caught=status!=0 and 'test result: FAILED' in text and '1 failed' in text
        results.append({'mutation':name,'test':test,'exit_code':status,'runtime_failure':caught})
        assert caught, (name,status)
finally:
    source.write_text(original)
    (root/'mutations.json').write_text(json.dumps(results,indent=2)+'\n')
