#!/usr/bin/env python3
"""Qualify candidate multi-model fixtures across current E4 builds.

The artifacts remain candidate evidence, not a released or frozen format.
Every database mutation targets a fresh copy. Runtime use is Linux-only.
"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys

from format_reference_compat import digest, inventory, new_artifact_path


PROFILES = {"vector": 7, "quantized": 35, "spatial": 11, "text": 19, "all": 63}
BOUNDARIES = ("checkpointed", "wal-pending")
MANIFEST = "MULTIMODEL_FIXTURE.json"


def mask(value):
    parsed = int(value, 0)
    if parsed < 0 or parsed > (1 << 64) - 1:
        raise argparse.ArgumentTypeError("feature mask must be a u64")
    return parsed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--default-bin", type=Path, required=True)
    parser.add_argument("--retained-bin", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument(
        "--older-probe",
        nargs=3,
        action="append",
        default=[],
        metavar=("LABEL", "BINARY", "SUPPORTED_MASK"),
        help=(
            "preserved typed-admission probe and its logical feature mask; "
            "at least one five-family mask-31 probe is required"
        ),
    )
    args = parser.parse_args()
    if sys.platform != "linux":
        raise SystemExit("Run database fixture qualification on the authorized Linux host")

    binaries = {
        "default": args.default_bin.resolve(strict=True),
        "retained": args.retained_bin.resolve(strict=True),
    }
    probes = []
    labels = set()
    for label, binary, supported in args.older_probe:
        assert label and all(char.isalnum() or char in "-_" for char in label), (
            f"older probe label must use only letters, digits, '-' or '_': {label!r}"
        )
        assert label not in labels, f"duplicate older probe label: {label!r}"
        labels.add(label)
        supported = mask(supported)
        assert supported & 1, "an older typed probe must declare base logical feature bit 1"
        probes.append(
            {
                "label": label,
                "path": Path(binary).resolve(strict=True),
                "supported_logical_features": supported,
            }
        )
    old_five_family_probes = [
        probe for probe in probes if probe["supported_logical_features"] == 31
    ]
    assert old_five_family_probes, (
        "provide the preserved five-family query-drivers engine as "
        "--older-probe LABEL BINARY 31"
    )

    work = new_artifact_path(args.work)
    work.mkdir(parents=True)
    corpus = work / "corpus"
    corpus.mkdir()
    report = {
        "result": "RUNNING",
        "status": "candidate-not-released-or-frozen",
        "driver": {
            "path": str(Path(__file__).resolve()),
            "sha256": digest(Path(__file__).resolve()),
        },
        "binaries": {
            label: {"path": str(path), "sha256": digest(path)}
            for label, path in binaries.items()
        },
        "older_probes": [
            {
                "label": probe["label"],
                "path": str(probe["path"]),
                "sha256": digest(probe["path"]),
                "supported_logical_features": probe["supported_logical_features"],
            }
            for probe in probes
        ],
        "profiles": PROFILES,
        "fixtures": [],
        "arms": [],
        "admission_arms": [],
        "quantized_coverage": {},
        "old_five_family_gate": {},
        "guard_checks": [],
        "commands": [],
    }

    def run(binary, *argv, expected=0):
        command = [str(binary), *map(str, argv)]
        result = subprocess.run(command, text=True, capture_output=True)
        report["commands"].append(
            {
                "argv": command,
                "exit_code": result.returncode,
                "stdout": result.stdout,
                "stderr": result.stderr,
            }
        )
        if expected == "nonzero":
            assert result.returncode != 0, f"negative guard admitted: {command}"
        else:
            expected_codes = {expected} if isinstance(expected, int) else set(expected)
            assert result.returncode in expected_codes, (
                f"unexpected exit {result.returncode}, expected {sorted(expected_codes)}: "
                f"{command}\n{result.stdout}\n{result.stderr}"
            )
        return result

    sources = []
    try:
        for label, binary in binaries.items():
            version = json.loads(run(binary, "--version").stdout)
            assert version["harness"] == "multimodel-format-fixture-v2"
            assert version["status"] == "candidate-not-released-or-frozen"
            assert version["supported_logical_features"] == 63
            report["binaries"][label]["version"] = version
        for probe, recorded in zip(probes, report["older_probes"]):
            version = json.loads(run(probe["path"], "--version").stdout)
            assert version["harness"] == "typed-admission-probe-v1"
            if probe["supported_logical_features"] == 31:
                assert version["engine_revision"] != "unrecorded", (
                    "the five-family query-drivers probe must be built with "
                    "E4_COMPAT_ENGINE_REVISION provenance"
                )
            recorded["version"] = version
        assert report["binaries"]["default"]["version"]["create_compact_cells"] is False
        assert report["binaries"]["retained"]["version"]["create_compact_cells"] is True
        assert not any(
            report["binaries"]["default"]["version"]["compile_features"].values()
        )
        assert all(
            report["binaries"]["retained"]["version"]["compile_features"].values()
        )

        for generator_label, generator in binaries.items():
            reader_label = "retained" if generator_label == "default" else "default"
            reader = binaries[reader_label]
            for profile, required_mask in PROFILES.items():
                for initial_boundary in BOUNDARIES:
                    name = f"{generator_label}-{profile}-{initial_boundary}"
                    source = corpus / name
                    run(generator, "generate", profile, source, initial_boundary)
                    before = inventory(source)
                    manifest_path = source / MANIFEST
                    manifest = json.loads(manifest_path.read_text())
                    assert manifest["status"] == "candidate-not-released-or-frozen"
                    assert manifest["profile"] == profile
                    assert manifest["required_logical_features"] == required_mask
                    run(reader, "verify", source, "original")
                    assert inventory(source) == before, "cross-build read changed source fixture"

                    refusal = run(
                        reader,
                        "upgrade",
                        manifest_path,
                        source,
                        "wal-pending",
                        "--confirm-copy",
                        expected="nonzero",
                    )
                    assert "refusing to mutate source fixture" in refusal.stderr
                    assert inventory(source) == before
                    report["guard_checks"].append(
                        {"fixture": name, "guard": "source-overlap", "result": "PASS"}
                    )

                    symlink = work / f"{name}-symlink-negative"
                    symlink.symlink_to(source, target_is_directory=True)
                    refusal = run(
                        reader,
                        "upgrade",
                        manifest_path,
                        symlink,
                        "wal-pending",
                        "--confirm-copy",
                        expected="nonzero",
                    )
                    assert "ordinary directory, not a symlink" in refusal.stderr
                    symlink.unlink()
                    assert inventory(source) == before
                    report["guard_checks"].append(
                        {"fixture": name, "guard": "target-symlink", "result": "PASS"}
                    )

                    hardlink = work / f"{name}-hardlink-negative"
                    shutil.copytree(source, hardlink)
                    (hardlink / "data").unlink()
                    (hardlink / "data").hardlink_to(source / "data")
                    refusal = run(
                        reader,
                        "upgrade",
                        manifest_path,
                        hardlink,
                        "wal-pending",
                        "--confirm-copy",
                        expected="nonzero",
                    )
                    assert "multiple hard links" in refusal.stderr
                    assert inventory(source) == before
                    (hardlink / "data").unlink()
                    shutil.copyfile(source / "data", hardlink / "data")
                    report["guard_checks"].append(
                        {"fixture": name, "guard": "source-hardlink", "result": "PASS"}
                    )

                    fixture_record = {
                        "name": name,
                        "generator": generator_label,
                        "profile": profile,
                        "required_logical_features": required_mask,
                        "source_generation_boundary": initial_boundary,
                        "manifest_sha256": digest(manifest_path),
                        "files": before,
                    }
                    report["fixtures"].append(fixture_record)
                    sources.append((name, source, before, manifest_path, required_mask))

                    for handoff_boundary in BOUNDARIES:
                        target = work / f"{name}-upgrade-{handoff_boundary}"
                        shutil.copytree(source, target)
                        run(
                            reader,
                            "upgrade",
                            manifest_path,
                            target,
                            handoff_boundary,
                            "--confirm-copy",
                        )
                        upgraded = inventory(target)
                        run(generator, "verify", target, "updated")
                        assert inventory(target) == upgraded, (
                            "original generator read modified upgraded target"
                        )
                        assert inventory(source) == before, "upgrade changed source fixture"
                        arm = {
                            "fixture": name,
                            "profile": profile,
                            "required_logical_features": required_mask,
                            "source_generation_boundary": initial_boundary,
                            "writer": reader_label,
                            "reader": generator_label,
                            "handoff_boundary": handoff_boundary,
                            "source_unchanged": True,
                            "result": "PASS",
                            "upgraded_files": upgraded,
                        }
                        report["arms"].append(arm)
                        print(
                            json.dumps(
                                {key: value for key, value in arm.items() if key != "upgraded_files"}
                            ),
                            flush=True,
                        )

        expected_fixtures = len(binaries) * len(PROFILES) * len(BOUNDARIES)
        expected_arms = expected_fixtures * len(BOUNDARIES)
        assert len(report["fixtures"]) == expected_fixtures
        assert len(report["arms"]) == expected_arms

        quantized_arms = [
            arm
            for arm in report["arms"]
            if arm["required_logical_features"] & 32
        ]
        expected_quantized_arms = (
            len(binaries) * 2 * len(BOUNDARIES) * len(BOUNDARIES)
        )
        assert len(quantized_arms) == expected_quantized_arms
        assert all(arm["result"] == "PASS" for arm in quantized_arms)
        assert any(
            arm["source_generation_boundary"] == "wal-pending"
            for arm in quantized_arms
        )
        assert any(arm["handoff_boundary"] == "wal-pending" for arm in quantized_arms)
        report["quantized_coverage"] = {
            "feature": 32,
            "profiles": ["quantized", "all"],
            "cross_build_upgrade_arms": len(quantized_arms),
            "source_wal_pending": True,
            "handoff_wal_pending": True,
            "result": "PASS",
        }

        for probe in probes:
            for name, source, source_before, _, required_mask in sources:
                expectation = (
                    "accept"
                    if required_mask & ~probe["supported_logical_features"] == 0
                    else "refuse"
                )
                expected_exit = 0 if expectation == "accept" else 42
                for open_mode in ("snapshot", "writer"):
                    target = work / (
                        f"admission-{probe['label']}-{name}-{expectation}-{open_mode}"
                    )
                    shutil.copytree(source, target)
                    target_before = inventory(target)
                    result = run(
                        probe["path"],
                        expectation,
                        open_mode,
                        target,
                        expected=expected_exit,
                    )
                    target_after = inventory(target)
                    if expectation == "refuse":
                        assert target_after == target_before, (
                            "unsupported typed admission changed disposable copy"
                        )
                        assert result.returncode == 42
                    assert inventory(source) == source_before, (
                        "typed admission probe changed immutable source"
                    )
                    record = {
                        "probe": probe["label"],
                        "fixture": name,
                        "required_logical_features": required_mask,
                        "supported_logical_features": probe["supported_logical_features"],
                        "expectation": expectation,
                        "mode": open_mode,
                        "exit_code": result.returncode,
                        "source_unchanged": True,
                        "target_before": target_before,
                        "target_after": target_after,
                        "result": "PASS",
                    }
                    report["admission_arms"].append(record)

        old_five_labels = {probe["label"] for probe in old_five_family_probes}
        old_five_refusals = [
            arm
            for arm in report["admission_arms"]
            if arm["probe"] in old_five_labels
            and arm["required_logical_features"] & 32
        ]
        expected_old_five_refusals = (
            len(old_five_family_probes)
            * len(binaries)
            * 2
            * len(BOUNDARIES)
            * 2
        )
        assert len(old_five_refusals) == expected_old_five_refusals
        assert all(
            arm["expectation"] == "refuse"
            and arm["exit_code"] == 42
            and arm["target_before"] == arm["target_after"]
            and arm["source_unchanged"]
            for arm in old_five_refusals
        )
        report["old_five_family_gate"] = {
            "supported_logical_features": 31,
            "new_quantized_feature": 32,
            "probes": sorted(old_five_labels),
            "snapshot_and_writer_refusals": len(old_five_refusals),
            "all_refusals_byte_preserving": True,
            "result": "PASS",
        }

        for label, binary in binaries.items():
            assert digest(binary) == report["binaries"][label]["sha256"]
        for probe, recorded in zip(probes, report["older_probes"]):
            assert digest(probe["path"]) == recorded["sha256"]
        for _, source, before, _, _ in sources:
            assert inventory(source) == before
        assert digest(Path(__file__).resolve()) == report["driver"]["sha256"]
        report["result"] = "PASS"
    finally:
        if report["result"] == "RUNNING":
            report["result"] = "FAIL"
        (work / "REPORT.json").write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n"
        )

    print(
        json.dumps(
            {
                "result": report["result"],
                "fixtures": len(report["fixtures"]),
                "arms": len(report["arms"]),
                "admission_arms": len(report["admission_arms"]),
                "report": str(work / "REPORT.json"),
            }
        )
    )


if __name__ == "__main__":
    main()
