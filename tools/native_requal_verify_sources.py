"""Verify native source trees against the same frozen, reviewed manifests."""
import hashlib
import json
import sys
from pathlib import Path

root = Path(sys.argv[1]).resolve()
assert str(root) in (
    "<scratch>",
    "<scratch>",
)
results = {}
for variant in ("baseline", "candidate"):
    manifest = root / ("qualified-" + variant + "-manifest.json")
    expected = json.loads(manifest.read_text())
    source = root / "src" / variant
    for name, digest in expected.items():
        assert hashlib.sha256((source / name).read_bytes()).hexdigest() == digest, (variant, name)
    actual = {str(p.relative_to(source)) for p in source.rglob("*")
              if p.is_file() and ".cargo" not in p.relative_to(source).parts}
    assert actual == set(expected), (variant, actual ^ set(expected))
    results[variant] = {"files": len(expected),
                        "manifest_sha256": hashlib.sha256(manifest.read_bytes()).hexdigest()}
(root / "verified-sources.json").write_text(json.dumps(results, indent=2) + "\n")
print(json.dumps(results))
