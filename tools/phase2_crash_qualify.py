#!/usr/bin/env python3
"""Linux process-kill qualification for the Phase-2 multi-family candidate.

The PageWAL checkpoint arms and the barriers around lifecycle commits are
deterministic. Commit-window SIGKILL delays are timing samples and are reported
as such; they are not a substitute for the in-crate exhaustive write-fault
tests, whose injection seam is deliberately unavailable to normal binaries.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import stat
import subprocess
import sys
import threading
import time
from typing import Any


TIMEOUT = 30.0
FAMILIES = ("scalar", "vector", "quantized", "spatial", "text")


class Child:
    def __init__(self, argv: list[str]):
        self.argv = argv
        self.lines: list[str] = []
        self.stderr_lines: list[str] = []
        self.stdout_queue: queue.Queue[str | None] = queue.Queue()
        self.process = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        self.stdout_thread = threading.Thread(
            target=self._pump_stdout, name=f"stdout-{self.process.pid}", daemon=True
        )
        self.stderr_thread = threading.Thread(
            target=self._pump_stderr, name=f"stderr-{self.process.pid}", daemon=True
        )
        self.stdout_thread.start()
        self.stderr_thread.start()

    def _pump_stdout(self) -> None:
        assert self.process.stdout is not None
        for raw in iter(self.process.stdout.readline, b""):
            self.stdout_queue.put(raw.decode("utf-8", errors="replace").rstrip("\n"))
        self.stdout_queue.put(None)

    def _pump_stderr(self) -> None:
        assert self.process.stderr is not None
        for raw in iter(self.process.stderr.readline, b""):
            self.stderr_lines.append(
                raw.decode("utf-8", errors="replace").rstrip("\n")
            )

    def expect(self, prefix: str, timeout: float = TIMEOUT) -> str:
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"timed out waiting for {prefix!r}: {self.argv!r}")
            try:
                line = self.stdout_queue.get(timeout=remaining)
            except queue.Empty as error:
                raise TimeoutError(
                    f"timed out waiting for {prefix!r}: {self.argv!r}"
                ) from error
            if line is None:
                self.process.wait(timeout=TIMEOUT)
                raise RuntimeError(
                    f"child exited {self.process.returncode} before {prefix!r}: "
                    f"{self.argv!r}\nstdout={self.lines!r}\n"
                    f"stderr={self.stderr_lines!r}"
                )
            self.lines.append(line)
            if line.startswith(prefix):
                return line

    def send(self, value: str = "continue") -> None:
        assert self.process.stdin is not None
        self.process.stdin.write((value + "\n").encode())
        self.process.stdin.flush()

    def kill(self) -> int:
        if self.process.poll() is None:
            os.kill(self.process.pid, signal.SIGKILL)
        return self.process.wait(timeout=TIMEOUT)

    def finish(self) -> tuple[int, str]:
        returncode = self.process.wait(timeout=TIMEOUT)
        self.stdout_thread.join(timeout=TIMEOUT)
        self.stderr_thread.join(timeout=TIMEOUT)
        return returncode, "\n".join(self.stderr_lines)

    def cleanup(self) -> None:
        if self.process.poll() is None:
            self.kill()
        self.finish()


def run(binary: Path, *args: str, expected: int = 0) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(
        [str(binary), *args], capture_output=True, text=True, timeout=TIMEOUT
    )
    if completed.returncode != expected:
        raise RuntimeError(
            f"command returned {completed.returncode}, expected {expected}: "
            f"{completed.args!r}\nstdout={completed.stdout}\nstderr={completed.stderr}"
        )
    return completed


def last_json(completed: subprocess.CompletedProcess[str]) -> dict[str, Any]:
    lines = [line for line in completed.stdout.splitlines() if line.strip()]
    if not lines:
        raise RuntimeError(f"command emitted no JSON: {completed.args!r}")
    return json.loads(lines[-1])


def inventory(root: Path) -> list[dict[str, Any]]:
    result = []
    for path in sorted(root.rglob("*")):
        relative = str(path.relative_to(root))
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode):
            raise RuntimeError(f"fixture inventory entry is not a regular file: {path}")
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        result.append(
            {"path": relative, "bytes": metadata.st_size, "sha256": digest}
        )
    return result


def binary_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest()


def fresh_copy(source: Path, destination: Path) -> None:
    if destination.exists():
        raise RuntimeError(f"arm destination already exists: {destination}")
    shutil.copytree(source, destination, copy_function=shutil.copy2)


def check_holder(holder: Child) -> dict[str, Any]:
    holder.send("check")
    holder.expect("SNAPSHOT_STILL_EXACT")
    holder.send("release")
    returncode, stderr = holder.finish()
    if returncode != 0:
        raise RuntimeError(f"held snapshot failed: {stderr}")
    return {"exact_old_snapshot": True, "barriers": holder.lines}


def classify(binary: Path, database: Path) -> dict[str, Any]:
    return last_json(run(binary, "classify", str(database)))


def commit_arm(
    binary: Path,
    source: Path,
    arm_dir: Path,
    boundary: str,
    delay: float | None,
) -> dict[str, Any]:
    fresh_copy(source, arm_dir)
    holder: Child | None = None
    writer: Child | None = None
    try:
        holder = Child([str(binary), "hold-snapshot", str(arm_dir), "original"])
        holder.expect("SNAPSHOT_READY")
        writer = Child([str(binary), "commit-child", str(arm_dir)])
        writer.expect("MUTATIONS_READY")

        if boundary == "precommit":
            returncode = writer.kill()
        else:
            writer.send()
            writer.expect("COMMIT_STARTING")
            if boundary == "timing":
                assert delay is not None
                time.sleep(delay)
            elif boundary == "postcommit":
                writer.expect("COMMIT_RETURNED")
            else:
                raise AssertionError(boundary)
            returncode = writer.kill()
        if returncode != -signal.SIGKILL:
            raise RuntimeError(f"commit child was not SIGKILLed: {returncode}")

        observed = classify(binary, arm_dir)
        if boundary == "precommit" and observed["state"] != "original":
            raise RuntimeError(f"precommit kill published state: {observed}")
        if boundary == "postcommit" and observed["state"] != "updated":
            raise RuntimeError(f"returned commit was not durable: {observed}")
        held = check_holder(holder)
        return {
            "kind": "commit",
            "boundary": boundary,
            "delay_seconds": delay,
            "classification": observed,
            "writer_barriers": writer.lines,
            "held_snapshot": held,
        }
    finally:
        if writer is not None:
            writer.cleanup()
        if holder is not None:
            holder.cleanup()


def lifecycle_arm(
    binary: Path,
    source: Path,
    arm_dir: Path,
    family: str,
    mode: str,
    boundary: str,
    delay: float | None,
) -> dict[str, Any]:
    fresh_copy(source, arm_dir)
    prepared = last_json(
        run(binary, "prepare-lifecycle", str(arm_dir), family, mode)
    )
    index = str(prepared["index"])
    old_signature = last_json(
        run(binary, "lifecycle-signature", str(arm_dir), family, index)
    )
    reference = arm_dir.with_name(arm_dir.name + "-committed-reference")
    fresh_copy(arm_dir, reference)
    run(binary, "lifecycle-step-commit", str(reference), index, mode)
    new_signature = last_json(
        run(binary, "lifecycle-signature", str(reference), family, index)
    )
    if old_signature == new_signature:
        raise RuntimeError(f"bounded {family} {mode} step made no observable transition")
    lifecycle_state = "building" if mode == "build" else "dropping"
    holder: Child | None = None
    writer: Child | None = None
    try:
        holder = Child(
            [
                str(binary),
                "hold-snapshot",
                str(arm_dir),
                "original",
                index,
                lifecycle_state,
            ]
        )
        holder.expect("SNAPSHOT_READY")
        writer = Child(
            [str(binary), "lifecycle-child", str(arm_dir), index, mode]
        )
        writer.expect("STEP_READY")
        writer.send()
        writer.expect("STEP_MUTATED")
        if boundary == "precommit":
            returncode = writer.kill()
        else:
            writer.send()
            writer.expect("STEP_COMMIT_STARTING")
            if boundary == "timing":
                assert delay is not None
                time.sleep(delay)
            elif boundary == "postcommit":
                writer.expect("STEP_COMMITTED")
            else:
                raise AssertionError(boundary)
            returncode = writer.kill()
        if returncode != -signal.SIGKILL:
            raise RuntimeError(f"lifecycle child was not SIGKILLed: {returncode}")

        # Base entity/graph/index queries must remain one exact original snapshot.
        base = last_json(run(binary, "verify", str(arm_dir), "original"))
        observed_signature = last_json(
            run(binary, "lifecycle-signature", str(arm_dir), family, index)
        )
        if observed_signature not in (old_signature, new_signature):
            raise RuntimeError(
                f"torn {family} {mode} step: {observed_signature!r} is neither exact transition endpoint"
            )
        endpoint = "old" if observed_signature == old_signature else "committed-new"
        if boundary == "precommit" and endpoint != "old":
            raise RuntimeError(f"uncommitted lifecycle step became visible: {observed_signature}")
        if boundary == "postcommit" and endpoint != "committed-new":
            raise RuntimeError(f"returned lifecycle commit was not durable: {observed_signature}")
        held = check_holder(holder)
        resumed = last_json(
            run(binary, "inspect-resume", str(arm_dir), family, mode, index)
        )
        last_json(run(binary, "verify", str(arm_dir), "original"))
        return {
            "kind": "lifecycle",
            "family": family,
            "mode": mode,
            "boundary": boundary,
            "delay_seconds": delay,
            "base_classification": base,
            "transition_endpoint": endpoint,
            "old_signature": old_signature,
            "committed_new_signature": new_signature,
            "observed_signature": observed_signature,
            "initial_lifecycle_after_reopen": resumed["observed"],
            "resume_and_oracle": resumed,
            "writer_barriers": writer.lines,
            "held_snapshot": held,
        }
    finally:
        if writer is not None:
            writer.cleanup()
        if holder is not None:
            holder.cleanup()


def checkpoint_arm(
    binary: Path, source: Path, arm_dir: Path, stage: int
) -> dict[str, Any]:
    fresh_copy(source, arm_dir)
    run(binary, "apply-commit", str(arm_dir))
    completed = run(
        binary, "checkpoint-crash", str(arm_dir), str(stage), expected=86
    )
    verified = last_json(run(binary, "verify", str(arm_dir), "updated"))
    return {
        "kind": "deterministic-checkpoint",
        "stage": stage,
        "exit_code": completed.returncode,
        "classification": verified,
    }


def qualify_binary(
    label: str,
    binary: Path,
    root: Path,
    delays: list[float],
    report: dict[str, Any],
) -> None:
    binary = binary.resolve(strict=True)
    version = last_json(run(binary, "--version"))
    if version.get("harness") != "phase2-crash-probe-v2":
        raise RuntimeError(f"unexpected crash probe version: {version!r}")
    build_root = root / label
    build_root.mkdir()
    source = build_root / "immutable-source"
    run(binary, "seed", str(source))
    source_before = inventory(source)
    build_report: dict[str, Any] = {
        "binary": str(binary),
        "binary_sha256": binary_sha256(binary),
        "version": version,
        "source_inventory": source_before,
        "arms": [],
    }
    report["builds"][label] = build_report

    arms: list[dict[str, Any]] = build_report["arms"]
    arms.append(commit_arm(binary, source, build_root / "commit-pre", "precommit", None))
    arms.append(commit_arm(binary, source, build_root / "commit-post", "postcommit", None))
    for number, delay in enumerate(delays):
        arms.append(
            commit_arm(
                binary,
                source,
                build_root / f"commit-timing-{number:02d}",
                "timing",
                delay,
            )
        )

    lifecycle_boundaries = (
        ("precommit", None),
        ("timing", delays[0] if delays else 0.0),
        ("postcommit", None),
    )
    for family in FAMILIES:
        for mode in ("build", "drop"):
            for boundary, delay in lifecycle_boundaries:
                arms.append(
                    lifecycle_arm(
                        binary,
                        source,
                        build_root / f"{family}-{mode}-{boundary}",
                        family,
                        mode,
                        boundary,
                        delay,
                    )
                )

    for stage in range(1, 7):
        arms.append(
            checkpoint_arm(
                binary, source, build_root / f"checkpoint-stage-{stage}", stage
            )
        )

    expected_cases = 2 + len(delays) + len(FAMILIES) * 2 * 3 + 6
    if len(arms) != expected_cases:
        raise RuntimeError(
            f"internal crash matrix mismatch: got {len(arms)}, expected {expected_cases}"
        )
    build_report["expected_case_count"] = expected_cases
    build_report["completed_case_count"] = len(arms)

    source_after = inventory(source)
    if source_after != source_before:
        raise RuntimeError(f"immutable source changed during {label} qualification")
    build_report["source_unchanged"] = True


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--default-bin", type=Path, required=True)
    parser.add_argument("--retained-bin", type=Path)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument(
        "--commit-delays",
        default="0,0.0001,0.001,0.005",
        help="comma-separated observational SIGKILL delays after COMMIT_STARTING",
    )
    arguments = parser.parse_args()
    if not sys.platform.startswith("linux"):
        raise SystemExit("phase2 crash qualification runs on Linux only")
    work = arguments.work_dir.resolve()
    if work.exists():
        raise SystemExit("--work-dir must be new")
    work.mkdir(parents=True)
    delays = [float(value) for value in arguments.commit_delays.split(",") if value]
    if any(delay < 0 or delay > 1 for delay in delays):
        raise SystemExit("commit delays must be between 0 and 1 second")

    report: dict[str, Any] = {
        "format": "e4-phase2-crash-qualification-v2",
        "status": "candidate-evidence-not-public-acceptance",
        "started_unix": time.time(),
        "limits": {
            "commit_window": "SIGKILL timing samples; no public deterministic commit-stage hook",
            "checkpoint": "deterministic PageWAL pilot stages 1..6, expected abrupt exit 86",
            "write_faults": "normal binaries cannot arm cfg(test) PageWAL write faults; exhaustive in-crate family fault tests are companion evidence",
            "transaction_oracle": "one commit atomically changes rows, authoritative f32 sidecars, exact and quantized vector entries, scalar/spatial/text postings, and graph edges",
            "quantized_oracle": "full exact rerank plus an ef=1 winner that changes when persisted int8 code changes",
            "peak_or_randomness_claim": False,
        },
        "families": list(FAMILIES),
        "expected_case_count_per_build": 2
        + len(delays)
        + len(FAMILIES) * 2 * 3
        + 6,
        "builds": {},
        "complete": False,
    }
    report_path = work / "phase2-crash-report.json"
    try:
        qualify_binary("default", arguments.default_bin, work, delays, report)
        if arguments.retained_bin:
            qualify_binary("retained", arguments.retained_bin, work, delays, report)
        report["complete"] = True
    except BaseException as error:
        report["failure"] = {"type": type(error).__name__, "message": str(error)}
        raise
    finally:
        report["finished_unix"] = time.time()
        report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    main()
