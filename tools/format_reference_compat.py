#!/usr/bin/env python3
"""Capture a new reference corpus, or qualify copies across two real binaries.

Capture is explicit and separate from tests. Verification requires a pinned
INDEX digest and never invokes a generator. All work directories must be new.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import uuid

REFERENCE_REVISION = "59d1cbc770284f160ffda53cc1ee545167733d11"
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()
EXPECTED_NAMES = {
    "compact-off-checkpointed", "compact-off-wal-pending",
    "compact-on-checkpointed", "compact-on-wal-pending", "limited-checkpointed",
}


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def inventory(root):
    result = {}
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"symlink is not a preserved regular file: {path}")
        if path.is_file():
            result[path.relative_to(root).as_posix()] = {"bytes": path.stat().st_size, "sha256": digest(path)}
        elif not path.is_dir():
            raise ValueError(f"unexpected filesystem object: {path}")
    return result


def validate_corpus(root, index_sha256):
    assert root.is_dir(), f"required permanent reference corpus missing: {root}"
    assert digest(root / "INDEX.json") == index_sha256, "reference INDEX digest mismatch"
    index = json.loads((root / "INDEX.json").read_text())
    assert index["generator"]["git_head"] == REFERENCE_REVISION, "not the captured reference commit"
    assert index["generator"]["diff_sha256"] == EMPTY_SHA256, "reference generator had tracked source changes"
    fixtures = index["fixtures"]
    assert len(fixtures) == 5 and {f["name"] for f in fixtures} == EXPECTED_NAMES
    expected_paths = {"INDEX.json"}
    for entry in fixtures:
        name = entry["name"]
        directory = root / name
        manifest_path = directory / "MANIFEST.json"
        assert digest(manifest_path) == entry["manifest_sha256"], f"{name}: manifest digest mismatch"
        manifest = json.loads(manifest_path.read_text())
        assert manifest["generator"] == index["generator"], f"{name}: inconsistent provenance"
        assert manifest["counts"]["entities"] == 283 and manifest["counts"]["collections"] == 3
        assert manifest["compact_cells"] == name.startswith("compact-on-")
        assert manifest["checkpointed"] == name.endswith("checkpointed")
        assert manifest["limited"] == name.startswith("limited-")
        assert entry["compact_cells"] == manifest["compact_cells"]
        assert entry["checkpointed"] == manifest["checkpointed"]
        assert entry["limited"] == manifest["limited"]
        expected_paths.add(f"{name}/MANIFEST.json")
        for filename, record in manifest["files"].items():
            assert Path(filename).name == filename, "manifest path must be one filename"
            path = directory / filename
            assert path.stat().st_size == record["bytes"] and digest(path) == record["sha256"], f"changed file: {path}"
            expected_paths.add(f"{name}/{filename}")
    before = inventory(root)
    assert set(before) == expected_paths, "reference corpus file inventory mismatch"
    assert len(before) == 66, "expected five complete 12-file databases, five manifests and INDEX"
    return fixtures, before


def new_artifact_path(path):
    # Resolve existing symlink parents and '..' before any containment check.
    # This helper must remain read-only: callers check protected roots before
    # creating even an intermediate parent directory.
    path = path.resolve(strict=False)
    assert not path.exists(), f"refusing to replace existing artifact: {path}"
    root = os.environ.get("SEKEJAP_BENCH_ROOT")
    if root:
        assert path.is_relative_to(Path(root).resolve()), "database artifacts must be under SEKEJAP_BENCH_ROOT"
    return path


def capture(args):
    corpus = new_artifact_path(args.corpus)
    generator = args.capture_generator.resolve(strict=True)
    cwd = args.capture_cwd.resolve(strict=True)
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=cwd, text=True).strip()
    assert revision == REFERENCE_REVISION, "capture requires the actual reference checkout"
    diff = subprocess.check_output(["git", "diff", "HEAD"], cwd=cwd)
    assert hashlib.sha256(diff).hexdigest() == EMPTY_SHA256, "capture requires unchanged tracked reference source"
    corpus.parent.mkdir(parents=True, exist_ok=True)
    # The legacy generator deletes its output first. Give it a fresh UUID path,
    # never the permanent destination or any preserved candidate fixture path.
    staging = corpus.parent / f".{corpus.name}.capture-{uuid.uuid4().hex}"
    assert not staging.exists()
    generator_hash = digest(generator)
    subprocess.run([str(generator), str(staging)], cwd=cwd, check=True)
    index_hash = digest(staging / "INDEX.json")
    _, files = validate_corpus(staging, index_hash)
    assert digest(generator) == generator_hash, "generator binary changed during capture"
    # mkdir is exclusive: even an empty existing destination is never replaced.
    corpus.mkdir()
    for path in staging.iterdir():
        path.rename(corpus / path.name)
    staging.rmdir()
    assert inventory(corpus) == files
    record = {"reference_revision": revision, "generator_sha256": generator_hash,
              "index_sha256": index_hash, "files": files, "corpus": str(corpus)}
    with corpus.with_name(corpus.name + ".capture.json").open("x") as output:
        json.dump(record, output, indent=2, sort_keys=True)
        output.write("\n")
    print(json.dumps({"result": "CAPTURED", "corpus": str(corpus), "index_sha256": index_hash,
                      "reference_revision": revision, "generator_sha256": generator_hash}, sort_keys=True))


def qualify(args):
    assert args.index_sha256 and args.baseline_bin and args.current_bin and args.work, "qualification requires pinned index digest, both binaries and new work path"
    corpus = args.corpus.resolve(strict=True)
    baseline = args.baseline_bin.resolve(strict=True)
    current = args.current_bin.resolve(strict=True)
    fixtures, preserved = validate_corpus(corpus, args.index_sha256)
    binary_hashes = {"baseline": digest(baseline), "current": digest(current)}
    assert binary_hashes["baseline"] != binary_hashes["current"], "cross-binary qualification cannot use identical executables"
    work = new_artifact_path(args.work)
    assert not work.is_relative_to(corpus), "work must be outside the permanent corpus"
    work.parent.mkdir(parents=True, exist_ok=True)
    work.mkdir()
    commands = []
    versions = {}
    report = {"result": "RUNNING", "corpus": str(corpus), "index_sha256": args.index_sha256,
              "binary_sha256": binary_hashes, "versions": versions, "commands": commands, "arms": []}

    def run(binary, *arguments):
        command = [str(binary), *map(str, arguments)]
        result = subprocess.run(command, text=True, capture_output=True)
        commands.append({"argv": command, "exit_code": result.returncode,
                         "stdout": result.stdout, "stderr": result.stderr})
        assert result.returncode == 0, f"failed {command}:\n{result.stdout}\n{result.stderr}"
        return result.stdout

    try:
        versions["baseline"] = json.loads(run(baseline, "--version"))
        versions["current"] = json.loads(run(current, "--version"))
        assert versions["baseline"]["engine_revision"] == REFERENCE_REVISION, "baseline binary lacks the pinned engine revision tag"
        assert versions["baseline"]["harness"] == versions["current"]["harness"] == "format_compat-v1"
        for fixture in fixtures:
            name = fixture["name"]
            manifest = corpus / name / "MANIFEST.json"
            source_manifest = json.loads(manifest.read_text())
            for boundary in ["checkpointed", "wal-pending"]:
                directory = work / f"{name}--{boundary}"
                directory.mkdir()
                for filename in source_manifest["files"]:
                    shutil.copyfile(corpus / name / filename, directory / filename)
                run(current, "verify", manifest, directory, "original")
                run(current, "mutate", manifest, directory, "upgrade", boundary)
                run(baseline, "verify", manifest, directory, "upgraded")
                run(baseline, "mutate", manifest, directory, "rollback", boundary)
                run(current, "verify", manifest, directory, "rollback")
                assert inventory(corpus) == preserved, "permanent reference corpus modified"
                arm = {"fixture": name, "handoff": boundary, "result": "PASS", "final_files": inventory(directory)}
                report["arms"].append(arm)
                print(json.dumps({"fixture": name, "handoff": boundary, "result": "PASS"}), flush=True)
        assert {"baseline": digest(baseline), "current": digest(current)} == binary_hashes, "binary changed during qualification"
        report["result"] = "PASS"
    finally:
        report["source_unchanged"] = inventory(corpus) == preserved
        if not report["source_unchanged"]:
            report["result"] = "FAIL"
        if report["result"] == "RUNNING":
            report["result"] = "FAIL"
        (work / "REPORT.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    assert report["source_unchanged"]
    print(json.dumps({"result": "PASS", "arms": len(report["arms"]), "report": str(work / "REPORT.json")}, sort_keys=True))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--capture-generator", type=Path)
    parser.add_argument("--capture-cwd", type=Path)
    parser.add_argument("--index-sha256")
    parser.add_argument("--baseline-bin", type=Path)
    parser.add_argument("--current-bin", type=Path)
    parser.add_argument("--work", type=Path)
    args = parser.parse_args()
    if args.capture_generator:
        assert args.capture_cwd, "capture requires the actual clean reference checkout"
        assert not any([args.index_sha256, args.baseline_bin, args.current_bin, args.work]), "capture and qualification are separate actions"
        capture(args)
    else:
        qualify(args)


if __name__ == "__main__":
    main()
