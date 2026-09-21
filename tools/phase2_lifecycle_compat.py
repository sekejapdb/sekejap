#!/usr/bin/env python3
"""Qualify graph-independent candidate index lifecycle fixtures on Linux.

Default/retained arms are same-revision physical-codec cross-build checks, not
cross-version evidence. Preserved older probes provide typed admission only.
The captured corpus is intended to become input to future accepted-version
regression runs; it is not a released or frozen format baseline.
"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys

from format_reference_compat import digest, inventory, new_artifact_path


# The logical feature mask a generated fixture of each family REQUIRES of its
# reader, as the fixture writes it into the manifest. Loop 4 gave scalar and
# spatial indexes a B-tree each, announced by 0x80, so a fixture of either
# family now requires its family bit plus 128: 1 -> 129 and 9 -> 137.
FAMILIES = {
    "scalar": 129,
    "exact-vector": 5,
    "spatial": 137,
    "text": 17,
    "quantized": 33,
}
# What the preserved five-family candidate implements. Every mask with a bit
# outside this is refused whole by it, which is the admission evidence.
OLD_FIVE_MASK = 31
LIFECYCLES = ("ready", "building", "dropping", "post-drop")
BOUNDARIES = ("checkpointed", "wal-pending")
MANIFEST = "PHASE2_LIFECYCLE_FIXTURE.json"
REPORT_FORMAT = "phase2-lifecycle-compat-report-v1"


def mask(value):
    parsed = int(value, 0)
    if parsed < 0 or parsed > (1 << 64) - 1:
        raise argparse.ArgumentTypeError("feature mask must be a u64")
    return parsed


def checked_wal_bytes(database, boundary):
    wal_bytes = (database / "wal").stat().st_size
    assert (wal_bytes == 0) == (boundary == "checkpointed"), (
        f"WAL bytes/boundary mismatch for {database}: {wal_bytes} bytes, {boundary}"
    )
    return wal_bytes


def main():
    if sys.flags.optimize != 0:
        raise SystemExit("Python optimization is forbidden: assertions are qualification gates")
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
            "preserved typed-admission probe label, binary and supported logical mask; "
            "a preserved five-family mask-31 probe is required"
        ),
    )
    args = parser.parse_args()
    if sys.platform != "linux":
        raise SystemExit("Run lifecycle database qualification on the authorized Linux host")

    binaries = {
        "default": args.default_bin.resolve(strict=True),
        "retained": args.retained_bin.resolve(strict=True),
    }
    assert digest(binaries["default"]) != digest(binaries["retained"]), (
        "physical-codec arms require distinct default and retained executables"
    )

    probes = []
    labels = set()
    for label, binary, supported in args.older_probe:
        assert label and all(char.isalnum() or char in "-_" for char in label), (
            f"invalid older-probe label: {label!r}"
        )
        assert label not in labels, f"duplicate older-probe label: {label!r}"
        labels.add(label)
        supported = mask(supported)
        assert supported & 1, "typed probes must support the base logical feature bit"
        probes.append(
            {
                "label": label,
                "path": Path(binary).resolve(strict=True),
                "supported_logical_features": supported,
            }
        )
    old_five = [probe for probe in probes if probe["supported_logical_features"] == OLD_FIVE_MASK]
    assert old_five, (
        "provide the preserved five-family candidate as "
        "--older-probe LABEL BINARY 31"
    )

    work = new_artifact_path(args.work)
    work.mkdir(parents=True)
    corpus = work / "corpus"
    corpus.mkdir()
    driver_path = Path(__file__).resolve()
    helper_source = driver_path.parent.parent / "bench/src/bin/phase2_lifecycle_fixture.rs"
    helper_source = helper_source.resolve(strict=True)
    report = {
        "format": REPORT_FORMAT,
        "result": "RUNNING",
        "status": "candidate-not-released-or-frozen",
        "claim_scope": {
            "same_revision_physical_codec_cross_build": True,
            "cross_version_semantic_compatibility": False,
            "released_baseline": False,
            "released_baseline_relationship": (
                "Phase 1 remains the actual released baseline and this candidate does not replace it"
            ),
            "older_probe_scope": "typed admission only",
            "future_role": (
                "permanent candidate lifecycle corpus for future accepted-version regression"
            ),
        },
        "driver": {"path": str(driver_path), "sha256": digest(driver_path)},
        "fixture_helper_source": {
            "path": str(helper_source),
            "sha256": digest(helper_source),
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
        "families": FAMILIES,
        "lifecycles": list(LIFECYCLES),
        "boundaries": list(BOUNDARIES),
        "fixtures": [],
        "same_revision_cross_build_arms": [],
        "admission_arms": [],
        "old_five_mask31_gate": {},
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
            assert version["harness"] == "phase2-lifecycle-fixture-v1"
            assert version["status"] == "candidate-not-released-or-frozen"
            # 255 = 0xff: loop 4 added 0x40 (packed text posting segments) and
            # 0x80 (per-index B-trees) to the five-family mask, which was 63.
            assert version["supported_logical_features"] == 255
            assert version["graph_enabled"] is False
            report["binaries"][label]["version"] = version
        assert report["binaries"]["default"]["version"]["create_compact_cells"] is False
        assert report["binaries"]["retained"]["version"]["create_compact_cells"] is True
        assert not any(report["binaries"]["default"]["version"]["compile_features"].values())
        assert all(report["binaries"]["retained"]["version"]["compile_features"].values())
        default_revision = report["binaries"]["default"]["version"]["engine_revision"]
        retained_revision = report["binaries"]["retained"]["version"]["engine_revision"]
        assert default_revision == retained_revision, (
            "default/retained fixtures must be built from the same engine revision"
        )
        assert default_revision != "unrecorded", (
            "candidate corpus generation requires recorded engine provenance"
        )

        for probe, recorded in zip(probes, report["older_probes"]):
            version = json.loads(run(probe["path"], "--version").stdout)
            assert version["harness"] == "typed-admission-probe-v1"
            if probe["supported_logical_features"] == 31:
                assert version["engine_revision"] != "unrecorded", (
                    "the preserved five-family candidate needs recorded provenance"
                )
            recorded["version"] = version

        for generator_label, generator in binaries.items():
            reader_label = "retained" if generator_label == "default" else "default"
            reader = binaries[reader_label]
            for family, required_mask in FAMILIES.items():
                for lifecycle in LIFECYCLES:
                    for initial_boundary in BOUNDARIES:
                        name = f"{generator_label}-{family}-{lifecycle}-{initial_boundary}"
                        source = corpus / name
                        run(
                            generator,
                            "generate",
                            family,
                            lifecycle,
                            source,
                            initial_boundary,
                        )
                        source_wal_bytes = checked_wal_bytes(source, initial_boundary)
                        before = inventory(source)
                        manifest_path = source / MANIFEST
                        manifest = json.loads(manifest_path.read_text())
                        assert manifest["format"] == "e4-phase2-index-lifecycle-candidate-v1"
                        assert manifest["status"] == "candidate-not-released-or-frozen"
                        assert manifest["family"] == family
                        assert manifest["lifecycle"] == lifecycle
                        assert manifest["source_generation_boundary"] == initial_boundary
                        assert manifest["required_logical_features"] == required_mask
                        assert manifest["graph_enabled"] is False
                        assert "future accepted-version" in manifest["regression_role"]

                        run(reader, "verify", source, "original")
                        assert inventory(source) == before, (
                            "same-revision cross-build read changed source fixture"
                        )

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

                        if family == "scalar" and lifecycle == "ready" and initial_boundary == "checkpointed":
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
                            assert "hard-linked" in refusal.stderr
                            assert inventory(source) == before
                            (hardlink / "data").unlink()
                            shutil.rmtree(hardlink)
                            report["guard_checks"].append(
                                {"fixture": name, "guard": "source-hardlink", "result": "PASS"}
                            )

                        fixture_record = {
                            "name": name,
                            "generator": generator_label,
                            "family": family,
                            "lifecycle": lifecycle,
                            "required_logical_features": required_mask,
                            "source_generation_boundary": initial_boundary,
                            "source_wal_bytes": source_wal_bytes,
                            "manifest_sha256": digest(manifest_path),
                            "files": before,
                        }
                        report["fixtures"].append(fixture_record)
                        sources.append(
                            (
                                name,
                                source,
                                before,
                                manifest_path,
                                family,
                                lifecycle,
                                required_mask,
                            )
                        )

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
                            upgraded_wal_bytes = checked_wal_bytes(target, handoff_boundary)
                            upgraded = inventory(target)
                            run(generator, "verify", target, "updated")
                            assert inventory(target) == upgraded, (
                                "original generator read changed upgraded target"
                            )
                            assert inventory(source) == before, (
                                "same-revision upgrade changed source fixture"
                            )
                            arm = {
                                "fixture": name,
                                "family": family,
                                "lifecycle": lifecycle,
                                "required_logical_features": required_mask,
                                "source_generation_boundary": initial_boundary,
                                "writer": reader_label,
                                "reader": generator_label,
                                "handoff_boundary": handoff_boundary,
                                "upgraded_wal_bytes": upgraded_wal_bytes,
                                "claim": "same-revision physical-codec cross-build",
                                "source_unchanged": True,
                                "result": "PASS",
                                "upgraded_files": upgraded,
                            }
                            report["same_revision_cross_build_arms"].append(arm)
                            print(
                                json.dumps(
                                    {
                                        key: value
                                        for key, value in arm.items()
                                        if key != "upgraded_files"
                                    },
                                    sort_keys=True,
                                ),
                                flush=True,
                            )

        expected_fixtures = len(binaries) * len(FAMILIES) * len(LIFECYCLES) * len(BOUNDARIES)
        expected_arms = expected_fixtures * len(BOUNDARIES)
        assert len(report["fixtures"]) == expected_fixtures == 80
        assert len(report["same_revision_cross_build_arms"]) == expected_arms == 160

        for probe in probes:
            for (
                name,
                source,
                source_before,
                _,
                family,
                lifecycle,
                required_mask,
            ) in sources:
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
                    byte_preserving = target_after == target_before
                    if expectation == "refuse":
                        assert result.returncode == 42
                        assert byte_preserving, (
                            "unsupported typed admission changed disposable copy"
                        )
                    assert inventory(source) == source_before, (
                        "typed admission probe changed immutable source"
                    )
                    report["admission_arms"].append(
                        {
                            "probe": probe["label"],
                            "fixture": name,
                            "family": family,
                            "lifecycle": lifecycle,
                            "required_logical_features": required_mask,
                            "supported_logical_features": probe[
                                "supported_logical_features"
                            ],
                            "expectation": expectation,
                            "mode": open_mode,
                            "exit_code": result.returncode,
                            "byte_preserving": byte_preserving,
                            "source_unchanged": True,
                            "target_before": target_before,
                            "target_after": target_after,
                            "result": "PASS",
                        }
                    )

        old_five_labels = {probe["label"] for probe in old_five}
        old_five_arms = [
            arm
            for arm in report["admission_arms"]
            if arm["probe"] in old_five_labels
        ]
        # Which families the preserved candidate can read is arithmetic over
        # the masks the fixtures actually declare, not a fixed list: loop 4
        # moved scalar and spatial out of its reach alongside quantized, and
        # writing the partition this way keeps the gate honest the next time a
        # family gains a bit.
        accepted_masks = sorted({m for m in FAMILIES.values() if m & ~OLD_FIVE_MASK == 0})
        refused_masks = sorted({m for m in FAMILIES.values() if m & ~OLD_FIVE_MASK != 0})
        assert accepted_masks and refused_masks, (
            "the mask-31 gate proves nothing unless the corpus has both an "
            "admissible and an inadmissible family"
        )
        supported = [
            arm for arm in old_five_arms
            if arm["required_logical_features"] in accepted_masks
        ]
        refused = [
            arm for arm in old_five_arms
            if arm["required_logical_features"] in refused_masks
        ]
        post_drop_refused = [arm for arm in refused if arm["lifecycle"] == "post-drop"]
        arms_each = len(LIFECYCLES) * len(BOUNDARIES) * 2
        per_probe_supported = 2 * len(accepted_masks) * arms_each
        per_probe_refused = 2 * len(refused_masks) * arms_each
        per_probe_post_drop = 2 * len(refused_masks) * len(BOUNDARIES) * 2
        assert len(supported) + len(refused) == len(old_five_arms)
        assert len(supported) == len(old_five) * per_probe_supported
        assert len(refused) == len(old_five) * per_probe_refused
        assert len(post_drop_refused) == len(old_five) * per_probe_post_drop
        assert all(arm["expectation"] == "accept" and arm["exit_code"] == 0 for arm in supported)
        assert all(
            arm["expectation"] == "refuse"
            and arm["exit_code"] == 42
            and arm["byte_preserving"]
            and arm["source_unchanged"]
            for arm in refused
        )
        assert all(arm["byte_preserving"] for arm in post_drop_refused)
        report["old_five_mask31_gate"] = {
            "supported_logical_features": OLD_FIVE_MASK,
            "probes": sorted(old_five_labels),
            "accepted_masks": accepted_masks,
            "refused_masks": refused_masks,
            "supported_snapshot_and_writer_acceptances": len(supported),
            "unsupported_snapshot_and_writer_refusals": len(refused),
            "post_drop_refusals": len(post_drop_refused),
            "post_drop_feature_bit_remained_required": True,
            "all_refusals_byte_preserving": True,
            "result": "PASS",
        }

        for label, binary in binaries.items():
            assert digest(binary) == report["binaries"][label]["sha256"]
        for probe, recorded in zip(probes, report["older_probes"]):
            assert digest(probe["path"]) == recorded["sha256"]
        for _, source, before, _, _, _, _ in sources:
            assert inventory(source) == before
        assert digest(driver_path) == report["driver"]["sha256"]
        assert digest(helper_source) == report["fixture_helper_source"]["sha256"]
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
                "format": REPORT_FORMAT,
                "result": report["result"],
                "fixtures": len(report["fixtures"]),
                "same_revision_cross_build_arms": len(
                    report["same_revision_cross_build_arms"]
                ),
                "admission_arms": len(report["admission_arms"]),
                "report": str(work / "REPORT.json"),
            },
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
