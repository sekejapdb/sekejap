# Pending checkpoint experiment

The measured 100K fixed-update candidate plateaus at 35.18 MB, versus SQLite
17.75 MB. E4 protects two different published roots plus the current mutable
epoch. A low checkpoint threshold limits each epoch but leaves the fallback
root retaining another epoch of pages.

A focused experiment can publish the same durable root in both metadata slots.
Flush data once, publish and sync the first slot, then publish and sync the
second. Only then refresh reuse and rotate WAL. This costs three data-file
barriers rather than two; it may recover space at a latency cost. Existing
snapshots continue to pin their actual versions. Ordinary checkpoint behavior
should remain unchanged until measured evidence supports a policy decision.

Required tests before benchmarking: failure at every barrier preserves committed
rows and existing snapshots; either single lost metadata slot reopens the latest
published root; real process-kill tests cover checkpointed and WAL-only branches;
repeated writer reopen retains valid reclamation hints. Generation overflow must
be refused before changing any page. Never reuse a page reachable from either
metadata slot before both publications finish.

This is not a disk cap. The separate admission gate must cover allocator growth,
WAL buffers, freelist candidates, temporary reader registrations, replay and
recovery headroom, including all write APIs. A transaction-boundary size check
can discover an overrun after it happened and must not be called enforcement.
