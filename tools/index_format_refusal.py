#!/usr/bin/env python3
"""Verify explicit index opt-in and frozen typed reader/writer safe refusal.

Linux example (all destinations new, outside the immutable corpus):
  cargo build -p sekejap-bench --release --bin index_format_fixture
  python3 tools/index_format_refusal.py --corpus docs/format-v1-baseline \
    --baseline-bin /bench/reference/bin/format_compat-baseline \
    --baseline-source /bench/reference/source \
    --current-bin target/release/index_format_fixture --work <scratch>

--baseline-source is an extracted baseline-source.tar.gz checkout with .git.
We copy its engine, add ONLY a tiny writer probe, and compile that probe. The
preserved baseline executable separately proves snapshot refusal. Alternatively
supply --baseline-writer-bin pointing at a previously built probe from this script.
No source fixture is opened as a database. This is additive evidence, not an
instruction to replace the frozen corpus or declare these new codecs frozen.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

from format_reference_compat import (REFERENCE_REVISION, digest, inventory,
                                     new_artifact_path, validate_corpus)

INDEX_SHA256 = "6bd933a1a63c6f2c2c3af0ac6f6e4d62c29c011a5ccdcbf66d32835fdb88ec26"
WRITER_PROBE = r'''
use e4_prototype::collections::{Database, Error};
use kernel::{io::IoMode, store::{Config, SyncMode}};
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--version"] {
        println!("{}", serde_json::json!({"harness":"index-writer-refusal-v1",
            "engine_revision":option_env!("E4_COMPAT_ENGINE_REVISION").unwrap_or("unrecorded")}));
        return;
    }
    assert_eq!(args.len(), 1, "usage: index_admission_probe COPIED_DB");
    let cfg = Config { budget_bytes:1<<20, io:IoMode::Buffered, sync:SyncMode::Full };
    match Database::open(&args[0], cfg) {
        Err(Error::Unsupported(why)) if why.contains("typed-collection header version") => {
            eprintln!("Unsupported: {why}"); std::process::exit(42);
        }
        Err(e) => panic!("wrong refusal: {e:?}"),
        Ok(_) => panic!("frozen writer unexpectedly admitted indexed database"),
    }
}
'''


def complete_inventory(root):
    files = inventory(root)
    for p in root.rglob("*"):
        if p.is_dir():
            files[p.relative_to(root).as_posix() + "/"] = {"directory": True}
    return files


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--baseline-bin", type=Path, required=True)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--baseline-source", type=Path)
    source.add_argument("--baseline-writer-bin", type=Path)
    parser.add_argument("--current-bin", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    args = parser.parse_args()
    if sys.platform == "darwin":
        raise SystemExit("Run this database qualification on the authorized Linux hosts, not Mac")
    corpus = args.corpus.resolve(strict=True)
    baseline = args.baseline_bin.resolve(strict=True)
    current = args.current_bin.resolve(strict=True)
    fixtures, preserved = validate_corpus(corpus, INDEX_SHA256)
    work = new_artifact_path(args.work)
    assert not work.is_relative_to(corpus), "work cannot be inside the immutable corpus"
    if args.baseline_source:
        assert not work.is_relative_to(args.baseline_source.resolve(strict=True)), "work cannot be inside baseline source"
    work.mkdir(parents=True)
    report = {"result": "RUNNING", "source_corpus": str(corpus), "source_index_sha256": INDEX_SHA256,
              "commands": [], "arms": [], "binary_sha256": {"baseline": digest(baseline), "current": digest(current)}}

    def run(binary, *arguments, expect=0, **kwargs):
        cmd = [str(binary), *map(str, arguments)]
        proc = subprocess.run(cmd, text=True, capture_output=True, **kwargs)
        report["commands"].append({"argv": cmd, "exit_code": proc.returncode,
                                   "stdout": proc.stdout, "stderr": proc.stderr})
        if expect is not None:
            assert proc.returncode == expect, f"unexpected status {proc.returncode}: {cmd}\n{proc.stdout}\n{proc.stderr}"
        return proc

    try:
        version = json.loads(run(baseline, "--version").stdout)
        assert version["engine_revision"] == REFERENCE_REVISION and version["harness"] == "format_compat-v1"
        if args.baseline_source:
            src = args.baseline_source.resolve(strict=True)
            revision = run("git", "-c", f"safe.directory={src}", "-C", src, "rev-parse", "HEAD").stdout.strip()
            assert revision == REFERENCE_REVISION, "writer probe requires actual frozen engine checkout"
            assert not run("git", "-c", f"safe.directory={src}", "-C", src, "diff", "HEAD").stdout, "frozen engine source has tracked changes"
            source_before = complete_inventory(src)
            probe_source = work / "writer-probe-source"
            shutil.copytree(src, probe_source, ignore=shutil.ignore_patterns(".git", "target"))
            destination = probe_source / "bench/src/bin/index_admission_probe.rs"
            with destination.open("x") as stream:
                stream.write(WRITER_PROBE)
            target = work / "writer-probe-target"
            env = dict(os.environ, CARGO_TARGET_DIR=str(target), E4_COMPAT_ENGINE_REVISION=REFERENCE_REVISION)
            run("cargo", "build", "--locked", "--offline", "--release", "--bin", "index_admission_probe", cwd=probe_source, env=env)
            assert complete_inventory(src) == source_before, "frozen source checkout changed"
            writer = target / "release/index_admission_probe"
            report["writer_probe_source_sha256"] = digest(destination)
        else:
            writer = args.baseline_writer_bin.resolve(strict=True)
        writer_version = json.loads(run(writer, "--version").stdout)
        assert writer_version == {"harness": "index-writer-refusal-v1", "engine_revision": REFERENCE_REVISION}
        report["binary_sha256"]["baseline_writer_probe"] = digest(writer)
        selected = [f for f in fixtures if f["name"] in {
            "compact-off-checkpointed", "compact-on-checkpointed", "limited-checkpointed"}]
        assert len(selected) == 3
        for fixture in selected:
            manifest_path = corpus / fixture["name"] / "MANIFEST.json"
            manifest = json.loads(manifest_path.read_text())
            for boundary in ("checkpointed", "wal-pending"):
                directory = work / f'{fixture["name"]}--index-{boundary}'
                directory.mkdir()
                for name in manifest["files"]:
                    shutil.copyfile(manifest_path.parent / name, directory / name)
                # Prove the actual old engine can open the unchanged source copy.
                run(baseline, "verify", manifest_path, directory, "original")
                run(current, "prepare", manifest_path, directory, boundary)
                before = complete_inventory(directory)
                snapshot = run(baseline, "verify", manifest_path, directory, "upgraded", expect=None)
                error = snapshot.stderr + snapshot.stdout
                assert snapshot.returncode != 0 and "Unsupported" in error and "typed-collection header version" in error, error
                assert complete_inventory(directory) == before, "frozen snapshot refusal changed source"
                probe = run(writer, directory, expect=42)
                assert "Unsupported" in probe.stderr and "typed-collection header version" in probe.stderr
                assert complete_inventory(directory) == before, "frozen writer refusal changed source"
                # Current read-only admission must still retrieve the exact oracle.
                run(current, "verify", manifest_path, directory)
                assert complete_inventory(directory) == before, "current read-only verification changed source"
                assert (directory.joinpath("wal").stat().st_size == 0) == (boundary == "checkpointed")
                assert inventory(corpus) == preserved, "immutable corpus changed"
                arm = {"fixture": fixture["name"], "boundary": boundary, "result": "PASS",
                       "frozen_snapshot_refused": True, "frozen_writer_refused": True,
                       "source_unchanged": True, "files": before}
                report["arms"].append(arm)
                print(json.dumps({k: v for k, v in arm.items() if k != "files"}), flush=True)
        assert len(report["arms"]) == 6
        assert report["binary_sha256"] == {"baseline": digest(baseline), "current": digest(current), "baseline_writer_probe": digest(writer)}
        report["result"] = "PASS"
    finally:
        report["immutable_corpus_unchanged"] = inventory(corpus) == preserved
        if report["result"] == "RUNNING" or not report["immutable_corpus_unchanged"]:
            report["result"] = "FAIL"
        (work / "REPORT.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    assert report["result"] == "PASS"
    print(json.dumps({"result": "PASS", "arms": len(report["arms"]), "report": str(work / "REPORT.json")}))


if __name__ == "__main__":
    main()
