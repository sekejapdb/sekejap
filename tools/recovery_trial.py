#!/usr/bin/env python3
"""Retained clean/damaged P1 trial; never mutate the benchmark source."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def hashes(folder):
    result = {}
    for name in ("data", "wal", "free"):
        path = folder / name
        if path.exists():
            h = hashlib.sha256()
            with path.open("rb") as f:
                for chunk in iter(lambda: f.read(1 << 20), b""):
                    h.update(chunk)
            result[name] = h.hexdigest()
    return result


def main():
    source, output = map(Path, sys.argv[1:3])
    rows = int(sys.argv[3])
    root = Path(os.environ.get("SEKEJAP_BENCH_ROOT", tempfile.gettempdir())).resolve()
    if not output.resolve().is_relative_to(root):
        raise ValueError(f"trial artifacts must stay under SEKEJAP_BENCH_ROOT ({root})")
    original = hashes(source)
    output.mkdir()
    binary = Path(__file__).resolve().parents[1] / "target/release/recover"

    def command(*args):
        return json.loads(subprocess.check_output([str(binary), *map(str, args)], text=True))

    results = []
    for label in ("clean", "damaged-overflow"):
        fixture = output / label
        fixture.mkdir()
        for name in original:
            shutil.copyfile(source / name, fixture / name)
        mutation = None
        if label != "clean":
            with (fixture / "data").open("r+b") as f:
                page = 0
                while chunk := f.read(4096):
                    if len(chunk) == 4096 and int.from_bytes(chunk[6:8], "little") == 4:
                        offset = page * 4096 + 50
                        f.seek(offset)
                        f.write(bytes([chunk[50] ^ 1]))
                        mutation = {"page": page, "offset": offset, "xor": 1}
                        break
                    page += 1
            assert mutation, "fixture must contain an overflow value"
        before = hashes(fixture)
        inspection = command("inspect", fixture)
        report = command("salvage", fixture, output / (label + "-result"))
        verified = command("verify", output / (label + "-result"))
        assert hashes(fixture) == before
        losses = int(label != "clean")
        assert report["known_value_losses"] == losses, report
        assert report["entries_recovered"] == rows + 3 - losses, report  # 3 catalog replicas
        assert report["unknown_extents"] == 0, report
        results.append({"case": label, "source_hashes": before, "mutation": mutation,
                        "inspection": inspection, "report": report, "verified": verified})
    assert hashes(source) == original
    (output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps([{"case": r["case"], "rows": r["report"]["entries_recovered"],
                       "seconds": r["report"]["elapsed_seconds"], "source_unchanged": True}
                      for r in results], indent=2))


if __name__ == "__main__":
    main()
