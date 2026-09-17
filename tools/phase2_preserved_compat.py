#!/usr/bin/env python3
"""Replay immutable Phase 2 candidate corpora against a future engine.

This Linux-only gate never generates fixtures. It accepts only known complete
qualification reports, caller-supplied preserved binaries whose hashes and
versions match those reports, and a new work directory outside every input.
Every mutation is performed on a fresh copy of one preserved source.
"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys

from format_reference_compat import digest, inventory, new_artifact_path


BOUNDARIES = ("checkpointed", "wal-pending")
BASELINE_LABELS = ("default", "retained")
GRAPH_MANIFEST = "PHASE2_FIXTURE.json"
MULTIMODEL_MANIFEST = "MULTIMODEL_FIXTURE.json"
LIFECYCLE_MANIFEST = "PHASE2_LIFECYCLE_FIXTURE.json"
OUTPUT_FORMAT = "phase2-preserved-compat-report-v1"

MULTIMODEL_PROFILES = {
    "multimodel-format-fixture-v1": {
        "vector": 7,
        "spatial": 11,
        "text": 19,
        "all": 31,
    },
    "multimodel-format-fixture-v2": {
        "vector": 7,
        "quantized": 35,
        "spatial": 11,
        "text": 19,
        "all": 63,
    },
}
LIFECYCLE_FAMILIES = {
    "scalar": 1,
    "exact-vector": 5,
    "spatial": 9,
    "text": 17,
    "quantized": 33,
}
LIFECYCLES = ("ready", "building", "dropping", "post-drop")


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def parse_sha256(value):
    if len(value) != 64 or value != value.lower():
        raise argparse.ArgumentTypeError(
            "report SHA-256 must contain 64 lowercase hexadecimal digits"
        )
    try:
        int(value, 16)
    except ValueError as error:
        raise argparse.ArgumentTypeError("report SHA-256 is not hexadecimal") from error
    return value


def parse_baselines(values):
    baselines = {}
    for label, path in values:
        require(label in BASELINE_LABELS, "baseline label must be default or retained")
        require(label not in baselines, f"duplicate baseline label: {label}")
        baselines[label] = Path(path).resolve(strict=True)
    require(set(baselines) == set(BASELINE_LABELS), "both default and retained baselines are required")
    return baselines


def source_boundary(name, fixture):
    recorded = fixture.get("source_generation_boundary")
    if recorded is not None:
        require(recorded in BOUNDARIES, f"{name}: invalid recorded source boundary")
        return recorded
    for boundary in BOUNDARIES:
        if name.endswith(f"-{boundary}"):
            return boundary
    raise AssertionError(f"{name}: source boundary is not recorded in its name")


def expected_graph_names():
    return {
        f"{label}-{boundary}"
        for label in BASELINE_LABELS
        for boundary in BOUNDARIES
    }


def expected_multimodel_names(profiles):
    return {
        f"{label}-{profile}-{boundary}"
        for label in BASELINE_LABELS
        for profile in profiles
        for boundary in BOUNDARIES
    }


def expected_lifecycle_names():
    return {
        f"{label}-{family}-{lifecycle}-{boundary}"
        for label in BASELINE_LABELS
        for family in LIFECYCLE_FAMILIES
        for lifecycle in LIFECYCLES
        for boundary in BOUNDARIES
    }


def validate_prior_arms(report, fixtures, field):
    arms = report.get(field)
    require(isinstance(arms, list), f"missing prior {field}")
    expected = {
        (fixture["name"], boundary)
        for fixture in fixtures
        for boundary in BOUNDARIES
    }
    actual = set()
    boundary_field = "handoff" if field == "arms" and len(fixtures) == 4 else "handoff_boundary"
    for arm in arms:
        require(isinstance(arm, dict), f"invalid prior {field} entry")
        key = (arm.get("fixture"), arm.get(boundary_field))
        require(key not in actual, f"duplicate prior handoff arm: {key}")
        actual.add(key)
        require(arm.get("result") == "PASS", f"prior handoff did not pass: {key}")
        require(arm.get("source_unchanged") is True, f"prior handoff changed source: {key}")
    require(actual == expected, f"prior {field} coverage is incomplete or unexpected")


def validate_report(report):
    require(isinstance(report, dict), "qualification report must be a JSON object")
    require(report.get("result") == "PASS", "qualification report did not pass")
    require(
        report.get("status") == "candidate-not-released-or-frozen",
        "qualification report has an unknown status",
    )
    recorded_binaries = report.get("binaries")
    require(isinstance(recorded_binaries, dict), "qualification report lacks binaries")
    require(set(recorded_binaries) == set(BASELINE_LABELS), "report baseline set is not exact")
    harnesses = {
        recorded_binaries[label].get("version", {}).get("harness")
        for label in BASELINE_LABELS
    }
    require(len(harnesses) == 1, "preserved baseline harnesses disagree")
    harness = next(iter(harnesses))
    fixtures = report.get("fixtures")
    require(isinstance(fixtures, list), "qualification report lacks fixtures")

    if harness == "phase2-format-fixture-v1":
        kind = "graph"
        manifest = GRAPH_MANIFEST
        expected_names = expected_graph_names()
        require(len(fixtures) == 4, "graph report must contain four source fixtures")
        validate_prior_arms(report, fixtures, "arms")
    elif harness in MULTIMODEL_PROFILES:
        kind = "multimodel"
        manifest = MULTIMODEL_MANIFEST
        profiles = MULTIMODEL_PROFILES[harness]
        require(report.get("profiles") == profiles, f"{harness} profile map is not exact")
        expected_names = expected_multimodel_names(profiles)
        require(len(fixtures) == len(expected_names), f"{harness} fixture count is incomplete")
        validate_prior_arms(report, fixtures, "arms")
    elif harness == "phase2-lifecycle-fixture-v1":
        kind = "lifecycle"
        manifest = LIFECYCLE_MANIFEST
        require(
            report.get("format") == "phase2-lifecycle-compat-report-v1",
            "lifecycle report format is unknown",
        )
        require(report.get("families") == LIFECYCLE_FAMILIES, "lifecycle family map is not exact")
        require(report.get("lifecycles") == list(LIFECYCLES), "lifecycle set is not exact")
        require(report.get("boundaries") == list(BOUNDARIES), "lifecycle boundary set is not exact")
        expected_names = expected_lifecycle_names()
        require(len(fixtures) == len(expected_names), "lifecycle fixture count is incomplete")
        validate_prior_arms(report, fixtures, "same_revision_cross_build_arms")
    else:
        raise AssertionError(f"unsupported preserved helper harness: {harness!r}")

    names = [fixture.get("name") for fixture in fixtures]
    require(len(set(names)) == len(names), "qualification report repeats a fixture name")
    require(set(names) == expected_names, "qualification report fixture-name coverage is not exact")
    by_name = {}
    for fixture in fixtures:
        require(isinstance(fixture, dict), "fixture record must be an object")
        name = fixture["name"]
        generator = fixture.get("generator")
        require(generator in BASELINE_LABELS, f"{name}: unknown originating generator")
        boundary = source_boundary(name, fixture)
        require(name.startswith(f"{generator}-"), f"{name}: generator/name mismatch")
        files = fixture.get("files")
        require(isinstance(files, dict) and files, f"{name}: missing source inventory")
        require(manifest in files, f"{name}: manifest absent from recorded inventory")
        manifest_record = files[manifest]
        require(
            isinstance(manifest_record, dict)
            and isinstance(manifest_record.get("bytes"), int)
            and isinstance(manifest_record.get("sha256"), str),
            f"{name}: malformed manifest inventory record",
        )
        if kind != "graph":
            require(
                fixture.get("manifest_sha256") == manifest_record["sha256"],
                f"{name}: recorded manifest hashes disagree",
            )
        if kind == "multimodel":
            profiles = MULTIMODEL_PROFILES[harness]
            require(fixture.get("profile") in profiles, f"{name}: unknown profile")
            require(
                name == f"{generator}-{fixture['profile']}-{boundary}",
                f"{name}: profile/name mismatch",
            )
            require(
                fixture.get("required_logical_features") == profiles[fixture["profile"]],
                f"{name}: profile feature mask mismatch",
            )
        if kind == "lifecycle":
            family = fixture.get("family")
            require(family in LIFECYCLE_FAMILIES, f"{name}: unknown family")
            require(fixture.get("lifecycle") in LIFECYCLES, f"{name}: unknown lifecycle")
            require(
                name == f"{generator}-{family}-{fixture['lifecycle']}-{boundary}",
                f"{name}: family/lifecycle/name mismatch",
            )
            require(
                fixture.get("required_logical_features") == LIFECYCLE_FAMILIES[family],
                f"{name}: family feature mask mismatch",
            )
            require(
                fixture.get("source_wal_bytes") == files.get("wal", {}).get("bytes"),
                f"{name}: source WAL byte record mismatch",
            )
        if kind == "graph":
            require(name == f"{generator}-{boundary}", f"{name}: boundary/name mismatch")
        by_name[name] = {
            "record": fixture,
            "generator": generator,
            "boundary": boundary,
            "files": files,
        }
    return {
        "kind": kind,
        "harness": harness,
        "manifest": manifest,
        "fixtures": by_name,
        "recorded_binaries": recorded_binaries,
    }


def checked_wal_bytes(database, boundary):
    wal = database / "wal"
    require(wal.is_file() and not wal.is_symlink(), f"missing ordinary WAL file: {wal}")
    size = wal.stat().st_size
    require(
        (size == 0) == (boundary == "checkpointed"),
        f"WAL bytes/boundary mismatch for {database}: {size} bytes, {boundary}",
    )
    return size


def validate_corpus(corpus, specification):
    require(corpus.is_dir() and not corpus.is_symlink(), "corpus must be an ordinary directory")
    children = {entry.name: entry for entry in corpus.iterdir()}
    require(
        set(children) == set(specification["fixtures"]),
        "corpus fixture directories do not exactly match the pinned report",
    )
    for name, expected in specification["fixtures"].items():
        source = children[name]
        require(source.is_dir() and not source.is_symlink(), f"{name}: source is not an ordinary directory")
        actual = inventory(source)
        require(actual == expected["files"], f"{name}: source inventory differs from report")
        manifest = source / specification["manifest"]
        require(
            digest(manifest) == expected["files"][specification["manifest"]]["sha256"],
            f"{name}: manifest hash differs from report",
        )
        checked_wal_bytes(source, expected["boundary"])
    return inventory(corpus)


def validate_work_path(requested, protected):
    work = new_artifact_path(requested)
    for label, path in protected.items():
        resolved = path.resolve(strict=True)
        require(work != resolved, f"work aliases protected {label}")
        require(not work.is_relative_to(resolved), f"work is inside protected {label}")
        require(not resolved.is_relative_to(work), f"work would contain protected {label}")
    return work


def main():
    if sys.flags.optimize != 0:
        raise SystemExit("Python optimization is forbidden: assertions are qualification gates")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qualification-report", type=Path, required=True)
    parser.add_argument("--report-sha256", type=parse_sha256, required=True)
    parser.add_argument(
        "--corpus",
        type=Path,
        required=True,
        help="immutable directory whose immediate children are the reported source fixtures",
    )
    parser.add_argument(
        "--baseline-bin",
        nargs=2,
        action="append",
        required=True,
        metavar=("LABEL", "PATH"),
        help="repeat exactly for labels default and retained",
    )
    parser.add_argument("--current-bin", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    args = parser.parse_args()
    if sys.platform != "linux":
        raise SystemExit("Run preserved compatibility qualification on the authorized Linux host")

    qualification_report = args.qualification_report.resolve(strict=True)
    corpus = args.corpus.resolve(strict=True)
    current = args.current_bin.resolve(strict=True)
    baselines = parse_baselines(args.baseline_bin)
    require(digest(qualification_report) == args.report_sha256, "qualification report hash mismatch")
    preserved_report = json.loads(qualification_report.read_text())
    specification = validate_report(preserved_report)
    preserved_corpus = validate_corpus(corpus, specification)

    baseline_hashes = {label: digest(path) for label, path in baselines.items()}
    require(len(set(baseline_hashes.values())) == 2, "preserved baseline executables are not distinct")
    for label in BASELINE_LABELS:
        require(
            baseline_hashes[label] == specification["recorded_binaries"][label].get("sha256"),
            f"{label} baseline hash differs from qualification report",
        )
    current_hash = digest(current)
    require(current_hash not in baseline_hashes.values(), "current binary is a preserved baseline executable")

    protected = {
        "qualification report": qualification_report,
        "corpus": corpus,
        "default baseline": baselines["default"],
        "retained baseline": baselines["retained"],
        "current binary": current,
    }
    work = validate_work_path(args.work, protected)

    work.parent.mkdir(parents=True, exist_ok=True)
    work.mkdir()
    report = {
        "format": OUTPUT_FORMAT,
        "result": "RUNNING",
        "claim": (
            "two independent fresh-copy handoffs; a preserved writer never writes a copy "
            "previously written by current"
        ),
        "fixture_generation_invoked": False,
        "preserved": {
            "qualification_report": str(qualification_report),
            "qualification_report_sha256": args.report_sha256,
            "schema": specification["kind"],
            "helper_harness": specification["harness"],
            "corpus": str(corpus),
            "source_inventory": preserved_corpus,
            "binaries": {
                label: {
                    "path": str(baselines[label]),
                    "sha256": baseline_hashes[label],
                }
                for label in BASELINE_LABELS
            },
        },
        "current": {
            "path": str(current),
            "sha256": current_hash,
        },
        "commands": [],
        "source_verifications": [],
        "handoffs": [],
    }

    def preflight_version(label, binary):
        command = [str(binary), "--version"]
        result = subprocess.run(command, text=True, capture_output=True)
        report["commands"].append(
            {
                "purpose": f"version:{label}",
                "argv": command,
                "exit_code": result.returncode,
                "stdout": result.stdout,
                "stderr": result.stderr,
            }
        )
        require(result.returncode == 0, f"version command failed for {label}")
        return json.loads(result.stdout)

    def run(purpose, binary, *arguments):
        command = [str(binary), *map(str, arguments)]
        result = subprocess.run(command, text=True, capture_output=True)
        report["commands"].append(
            {
                "purpose": purpose,
                "argv": command,
                "exit_code": result.returncode,
                "stdout": result.stdout,
                "stderr": result.stderr,
            }
        )
        require(
            result.returncode == 0,
            f"{purpose} failed ({result.returncode}): {command}\n{result.stdout}\n{result.stderr}",
        )

    try:
        baseline_versions = {
            label: preflight_version(f"baseline-{label}", path)
            for label, path in baselines.items()
        }
        for label in BASELINE_LABELS:
            require(
                baseline_versions[label]
                == specification["recorded_binaries"][label].get("version"),
                f"{label} baseline version differs from qualification report",
            )
            report["preserved"]["binaries"][label]["version"] = baseline_versions[label]
        current_version = preflight_version("current", current)
        report["current"]["version"] = current_version
        require(
            current_version.get("harness") == specification["harness"],
            "current binary helper harness does not match preserved report family",
        )
        current_revision = current_version.get("engine_revision")
        require(
            isinstance(current_revision, str) and current_revision and current_revision != "unrecorded",
            "current binary lacks recorded engine provenance",
        )
        baseline_revisions = {
            baseline_versions[label].get("engine_revision") for label in BASELINE_LABELS
        }
        require(
            current_revision not in baseline_revisions,
            "current binary engine revision is not newer/different from preserved provenance",
        )

        for name in sorted(specification["fixtures"]):
            fixture = specification["fixtures"][name]
            source = corpus / name
            source_before = fixture["files"]
            source_manifest = source / specification["manifest"]
            run(f"current-verify-original:{name}", current, "verify", source, "original")
            require(inventory(source) == source_before, f"{name}: current original verify changed source")
            report["source_verifications"].append(
                {"fixture": name, "reader": "current", "state": "original", "result": "PASS"}
            )

            preserved_writer = baselines[fixture["generator"]]
            for boundary in BOUNDARIES:
                directions = (
                    (
                        "current-writer-to-preserved-reader",
                        current,
                        preserved_writer,
                        "current",
                        fixture["generator"],
                    ),
                    (
                        "preserved-writer-to-current-reader",
                        preserved_writer,
                        current,
                        fixture["generator"],
                        "current",
                    ),
                )
                for direction, writer, reader, writer_label, reader_label in directions:
                    target = work / f"{name}--{boundary}--{direction}"
                    shutil.copytree(source, target)
                    run(
                        f"upgrade:{direction}:{name}:{boundary}",
                        writer,
                        "upgrade",
                        source_manifest,
                        target,
                        boundary,
                        "--confirm-copy",
                    )
                    wal_bytes = checked_wal_bytes(target, boundary)
                    upgraded = inventory(target)
                    run(
                        f"verify-updated:{direction}:{name}:{boundary}",
                        reader,
                        "verify",
                        target,
                        "updated",
                    )
                    require(
                        inventory(target) == upgraded,
                        f"{name}: {direction} reader changed upgraded copy",
                    )
                    require(inventory(source) == source_before, f"{name}: copied handoff changed source")
                    arm = {
                        "fixture": name,
                        "source_generator": fixture["generator"],
                        "source_generation_boundary": fixture["boundary"],
                        "handoff_boundary": boundary,
                        "direction": direction,
                        "writer": writer_label,
                        "reader": reader_label,
                        "wal_bytes": wal_bytes,
                        "source_unchanged": True,
                        "upgraded_files": upgraded,
                        "result": "PASS",
                    }
                    report["handoffs"].append(arm)
                    print(
                        json.dumps(
                            {key: value for key, value in arm.items() if key != "upgraded_files"},
                            sort_keys=True,
                        ),
                        flush=True,
                    )

        expected_handoffs = len(specification["fixtures"]) * len(BOUNDARIES) * 2
        require(len(report["handoffs"]) == expected_handoffs, "preserved handoff coverage incomplete")
        report["result"] = "PASS"
    finally:
        protected_final = {
            "qualification_report_sha256": digest(qualification_report),
            "corpus_inventory": inventory(corpus),
            "baseline_sha256": {label: digest(path) for label, path in baselines.items()},
            "current_sha256": digest(current),
        }
        protected_unchanged = (
            protected_final["qualification_report_sha256"] == args.report_sha256
            and protected_final["corpus_inventory"] == preserved_corpus
            and protected_final["baseline_sha256"] == baseline_hashes
            and protected_final["current_sha256"] == current_hash
        )
        report["protected_final"] = protected_final
        report["protected_unchanged"] = protected_unchanged
        if not protected_unchanged or report["result"] == "RUNNING":
            report["result"] = "FAIL"
        (work / "REPORT.json").write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n"
        )
        require(protected_unchanged, "preserved report, corpus, or binary changed")

    print(
        json.dumps(
            {
                "format": OUTPUT_FORMAT,
                "result": report["result"],
                "schema": specification["kind"],
                "fixtures": len(specification["fixtures"]),
                "handoffs": len(report["handoffs"]),
                "report": str(work / "REPORT.json"),
            },
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
