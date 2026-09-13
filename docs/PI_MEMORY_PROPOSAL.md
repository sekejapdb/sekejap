# Pi memory-controller proposal — awaiting approval

The running Pi boots with `cgroup_disable=memory` and exposes only
`cpuset cpu io pids` in `/sys/fs/cgroup/cgroup.controllers`. Systemd accepts
`MemoryMax=64M` but cannot enforce it. The benchmark's explicit check caught
this, so preliminary Pi crash/smoke/load results are not memory-capped evidence.

Proposed change: append **`cgroup_enable=memory`** to the existing single line
in `/boot/firmware/cmdline.txt`, then reboot. All existing arguments remain.
The Raspberry Pi maintainer documents this override and says the older
`cgroup_memory=1` argument is unnecessary:
[maintainer explanation](https://github.com/raspberrypi/linux/issues/6980#issuecomment-3149752155).

The original, proposed file, checksums and exact diff are staged under
`<scratch>/`.
[Application/rollback script](../tools/apply_pi_memory.py) checks the current
file against the reviewed original, preserves a boot-partition backup,
writes and syncs a candidate, then renames it over the active file.
It does not reboot automatically. After reboot, verify the `memory`
controller and actual `memory.max`/`memory.swap.max` values before retrying.

Approval is needed because changing boot settings and restarting the whole
Pi interrupts processes outside the isolated E4 workspace. No boot change
or reboot has been performed. Without approval, process address-space limits
can be used as a narrower test, with filesystem cache explicitly outside them.
