#!/usr/bin/env python3
"""Independent tiny-file counterexample to unaccounted chunk reservations."""
import json
import os
from pathlib import Path
import sys

path = Path(sys.argv[1])
assert str(path).startswith((
    "<scratch>/",
    "<scratch>/"))
assert ".." not in path.parts
fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
payload = bytes(range(256)) * 16
done = 0
while done < len(payload):
    n = os.pwrite(fd, payload[done:], done)
    assert n > 0
    done += n
os.fsync(fd)
s = os.fstat(fd)
print(json.dumps({"stage": "before_close", "logical": s.st_size,
                  "allocated": s.st_blocks * 512}), flush=True)
os.close(fd)
with path.open("rb") as source:
    assert source.read() == payload
print(json.dumps({"verified": True, "bytes": len(payload)}), flush=True)
