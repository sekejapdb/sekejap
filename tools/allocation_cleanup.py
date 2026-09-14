#!/usr/bin/env python3
"""Archive verified diagnostic metadata before removing reproducible test data.

prepare ART; independently verify/copy its archive; then remove ART SHA256.
Only the two named native artifact roots and their exact benchmark files.
"""
import hashlib
import json
from pathlib import Path
import sys
import tarfile

action, root = sys.argv[1], Path(sys.argv[2]).resolve()
assert str(root) in (
    "<scratch>",
    "<scratch>")
archive = root.parent / "allocation-complete-evidence.tgz"
plan_path = root / "cleanup-prepared.json"

def info(p):
    assert not p.is_symlink() and p.is_file()
    s = p.stat()
    return {"path": str(p.relative_to(root)), "logical": s.st_size,
            "allocated": s.st_blocks * 512, "inode": s.st_ino, "device": s.st_dev}

if action == "prepare":
    assert not archive.exists() and not plan_path.exists()
    paths, dbs = [], []
    for base in (root, root / "chunk"):
        assert (base / "bench-complete").is_file()
        reports = sorted((base / "bench").glob("*/report.json"))
        assert len(reports) == 10
        for report in reports:
            r = json.loads(report.read_text())
            assert r["reopen_verification"] == r["phases"][-1]["verification"]
            observations = json.loads((report.parent / "allocation-observations.json").read_text())
            assert observations["returncode"] == 0
            db = report.parent / "db"
            files = [info(p) for p in sorted(db.rglob("*")) if not p.is_dir()]
            difference = r["phases"][-1]["final_logical"] - sum(p["logical"] for p in files)
            # The benchmark measures while open; SQLite removes its 32 KiB
            # shared-memory file when the final connection closes.
            assert difference == 0 or (r["engine"] == "sqlite" and difference == 32768
                                       and not (db / "data.sqlite-shm").exists())
            paths.extend(files)
            dbs.append(str(db.relative_to(root)))
    for p in sorted(root.glob("*.data")) + sorted((root / "chunk").glob("*.data")):
        check = p.with_suffix(".jsonl")
        verified = json.loads(check.read_text().splitlines()[-1])
        assert verified["verified"]
        expected = verified.get("bytes", verified.get("pages", 0) * 4096)
        assert p.stat().st_size == expected
        paths.append(info(p))
    assert len([p for p in paths if p["path"].endswith(".data")]) == 7
    plan = {"root": str(root), "files": paths, "database_directories": dbs}
    plan_path.write_text(json.dumps(plan, indent=2) + "\n")
    excluded = {p["path"] for p in paths}
    with tarfile.open(archive, "w:gz") as tar:
        for p in sorted(root.rglob("*")):
            assert not p.is_symlink()
            if p.is_file() and str(p.relative_to(root)) not in excluded:
                tar.add(p, arcname=str(p.relative_to(root)), recursive=False)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    print(json.dumps({"archive": str(archive), "sha256": digest,
                      "planned_allocated_bytes": sum(p["allocated"] for p in paths),
                      "database_directories": len(dbs), "data_files": len(paths)}))
elif action == "remove":
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    assert len(sys.argv) == 4 and digest == sys.argv[3]
    plan = json.loads(plan_path.read_text())
    assert plan["root"] == str(root)
    # Every archived source/log/report must still match before any deletion.
    with tarfile.open(archive) as tar:
        for member in tar.getmembers():
            assert member.isfile()
            p = root / member.name
            assert p.resolve().is_relative_to(root) and not p.is_symlink()
            assert p.read_bytes() == tar.extractfile(member).read()
    current = []
    expected_paths = {f["path"] for f in plan["files"]}
    for name in plan["database_directories"]:
        found = {str(p.relative_to(root)) for p in (root/name).rglob("*") if p.is_file()}
        assert found == {p for p in expected_paths if p.startswith(name + "/")}
    for expected in plan["files"]:
        p = root / expected["path"]
        assert p.resolve().is_relative_to(root)
        actual = info(p)
        for field in ("logical", "inode", "device"):
            assert actual[field] == expected[field], (p, field)
        current.append(actual)
    with (root / "cleanup-progress.jsonl").open("x") as journal:
        for actual in current:
            (root / actual["path"]).unlink()
            journal.write(json.dumps(actual) + "\n")
            journal.flush()
    for name in plan["database_directories"]:
        db = root / name
        for p in sorted(db.rglob("*"), key=lambda p: len(p.parts), reverse=True):
            p.rmdir()
        db.rmdir()
    result = {"completed": True, "archive_sha256": digest,
              "removed_allocated_bytes": sum(p["allocated"] for p in current),
              "removed_logical_bytes": sum(p["logical"] for p in current),
              "database_directories": len(plan["database_directories"]),
              "files": current}
    (root / "cleanup.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({k:v for k,v in result.items() if k != "files"}))
else:
    raise ValueError(action)
