"""Archive verified native evidence; optionally remove only named successful DBs."""
import hashlib
import json
import os
import shutil
import sys
import tarfile
from pathlib import Path

root = Path(sys.argv[1]).resolve()
action = sys.argv[2]
assert action in ("archive", "cleanup")
assert str(root) in (
    "<scratch>",
    "<scratch>",
)
assert (root / "aligned-run/correctness-complete").is_file()
raw = json.loads((root / "qualify/results.json").read_text())
assert len(raw["records"]) == 81 and not raw["failures"]
removable = []
for record in raw["records"]:
    path = Path(record["path"])
    assert path.parent == root / "qualify"
    report = path / "report.json"
    assert hashlib.sha256(report.read_bytes()).hexdigest() == record["sha256"]
    binary = Path(record["command"][3])
    assert binary.is_relative_to(root / "bin")
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == record["binary_sha256"]
    removable.append(path / "db")

# Only fixture groups whose exact-key/value/ordering test passed are disposable.
fixture_logs = [
    ("baseline-law1", "baseline-workspace.log"),
    ("candidate-law1", "candidate-workspace.log"),
    ("aligned-run/baseline-law1", "aligned-run/baseline-workspace.log"),
    ("aligned-run/candidate-law1", "aligned-run/candidate-workspace.log"),
    ("aligned-run/candidate-final-law1", "aligned-run/candidate-final-workspace.log"),
]
for folder, log in fixture_logs:
    fixtures = root / folder
    if fixtures.exists():
        assert "test heap_does_not_grow_with_the_store ... ok" in (root / log).read_text()
        removable.extend(fixtures.glob("rows-*"))

archive = root / "evidence.tar.gz"
proof = root / "evidence-verified.json"
if action == "archive":
    hashes = {}
    for parent, directories, files in os.walk(root):
        directories[:] = [d for d in directories if d not in ("src", "targets", "bin", "tmp", "db") and not d.startswith("rows-")]
        for name in files:
            path = Path(parent) / name
            if path.suffix in (".json", ".log", ".txt", ".patch", ".rs", ".sh", ".py", ".yaml") and path != proof:
                hashes[str(path.relative_to(root))] = hashlib.sha256(path.read_bytes()).hexdigest()
    with tarfile.open(archive, "w:gz") as tar:
        for name in sorted(hashes):
            tar.add(root / name, arcname=name)
    with tarfile.open(archive) as tar:
        for name, digest in hashes.items():
            assert hashlib.sha256(tar.extractfile(name).read()).hexdigest() == digest
    result = {"archive_sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
              "files": hashes, "source_archives": {p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                  for p in root.glob("*.tar.gz") if p != archive}}
    proof.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"metadata_files": len(hashes), "archive_sha256": result["archive_sha256"]}))
else:
    verified = json.loads(proof.read_text())
    assert hashlib.sha256(archive.read_bytes()).hexdigest() == verified["archive_sha256"]
    removed = []
    for path in removable:
        assert path.is_relative_to(root) and path.is_dir() and not path.is_symlink(), path
        files = [p for p in path.rglob("*") if p.is_file()]
        removed.append({"path": str(path), "logical": sum(p.stat().st_size for p in files),
                        "allocated": sum(p.stat().st_blocks * 512 for p in files)})
        shutil.rmtree(path)
    result = {"removed": removed, "allocated_reclaimed": sum(r["allocated"] for r in removed),
              "archive_sha256": verified["archive_sha256"],
              "preserved": "All logs, source archives, binaries, reports and earlier failure evidence; no other loop directories touched."}
    (root / "cleanup.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"directories": len(removed), "allocated_reclaimed": result["allocated_reclaimed"]}))
