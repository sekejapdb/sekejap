"""Set up a diagnostic-only commit-stage build, separate from timing arms."""
import json, shutil
from pathlib import Path
base=Path('<scratch>')
src=Path('/tmp/e4-stage-candidate')
base.mkdir(); (base/'tmp').mkdir()
shutil.copy2('<scratch>',base/'baseline')
files={n:(src/'kernel/src'/n).read_text() for n in ('store.rs','pool.rs','verify.rs')}
(base/'profile-originals.json').write_text(json.dumps(files))
def replace(s,a,b):
    assert s.count(a)==1,(a,s.count(a)); return s.replace(a,b)
s=files['store.rs']
a=s.index('    fn checkpoint_inner('); b=s.index('    #[cfg(test)]\n    fn checkpoint_trace',a)
t=s[a:b]
t=replace(t,'        if let Err(e) = self.pool.flush_all(self.barrier()) {','        let stage_start = std::time::Instant::now();\n        if let Err(e) = self.pool.flush_all(self.barrier()) {') if t.count('        if let Err(e) = self.pool.flush_all(self.barrier()) {')==1 else t.replace('        if let Err(e) = self.pool.flush_all(self.barrier()) {','        let stage_start = std::time::Instant::now();\n        if let Err(e) = self.pool.flush_all(self.barrier()) {',1)
t=replace(t,'        #[cfg(test)] { self.trace.push("flush_pages");','        eprintln!("STAGE data {}", stage_start.elapsed().as_nanos());\n        let stage_start = std::time::Instant::now();\n        #[cfg(test)] { self.trace.push("flush_pages");')
t=replace(t,'        self.generation = gen;','        eprintln!("STAGE meta {}", stage_start.elapsed().as_nanos());\n        let stage_start = std::time::Instant::now();\n        self.generation = gen;')
t=replace(t,'        let _ = crate::verify::persist_checkpoint_freelist(','        eprintln!("STAGE reuse {}", stage_start.elapsed().as_nanos());\n        let stage_start = std::time::Instant::now();\n        let _ = crate::verify::persist_checkpoint_freelist(')
t=replace(t,'        // 2n recycling horizon:', '        eprintln!("STAGE hint {}", stage_start.elapsed().as_nanos());\n        // 2n recycling horizon:')
t=replace(t,'        self.wal_mut()?.rotate_published()?;','        let stage_start = std::time::Instant::now();\n        self.wal_mut()?.rotate_published()?;\n        eprintln!("STAGE rotate {}", stage_start.elapsed().as_nanos());')
(src/'core/kernel/src/store.rs').write_text(s[:a]+t+s[b:])
s=files['pool.rs'];a=s.index('    pub fn flush_all(');b=s.index('\n    }',a)+6;t=s[a:b]
t=replace(t,'        let mut inner =','        let stage_start = std::time::Instant::now();\n        let mut inner =')
t=replace(t,'        match barrier {','        eprintln!("STAGE page_write {}", stage_start.elapsed().as_nanos());\n        let stage_start = std::time::Instant::now();\n        let result = match barrier {')
t=t.rsplit('        }',1)[0]+'        };\n        eprintln!("STAGE page_sync {}", stage_start.elapsed().as_nanos());\n        result\n    }'
(src/'core/kernel/src/pool.rs').write_text(s[:a]+t+s[b:])
s=files['verify.rs'];a=s.index('pub(crate) fn persist_checkpoint_freelist(');b=s.index('\n}\n',a)+2;t=s[a:b]
t=replace(t,'    use std::io::Write;','    let stage_start = std::time::Instant::now();\n    use std::io::Write;')
t=replace(t,'    file.sync_all()?;','    eprintln!("STAGE hint_write {}", stage_start.elapsed().as_nanos());\n    let stage_start = std::time::Instant::now();\n    file.sync_all()?;\n    eprintln!("STAGE hint_sync {}", stage_start.elapsed().as_nanos());')
(src/'core/kernel/src/verify.rs').write_text(s[:a]+t+s[b:])
print('Diagnostic sources instrumented; production unchanged')
