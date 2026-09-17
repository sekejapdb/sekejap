#!/usr/bin/env python3
"""Render candidate Phase-2 multimodel driver-v2 evidence as Markdown."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
from pathlib import Path
import statistics
from typing import Any, Callable, Optional


Arm = dict[str, Any]
Getter = Callable[[Arm], Optional[float]]
ARMS = (("e4", "atomic"), ("e4", "resumable"), ("sqlite", "atomic"))
ARM_LABELS = {
    ("e4", "atomic"): "E4 atomic",
    ("e4", "resumable"): "E4 resumable",
    ("sqlite", "atomic"): "SQLite atomic",
}
MIB = 1024 * 1024
EXPECTED_QUERIES = {
    "queries": {
        "combined_graph_active_bbox_vector",
        "members_active_spatial_vector",
        "scalar_active_age",
        "spatial_bbox",
        "sqlite_native_bm25_k10",
        "text_active_vector",
        "text_positive_bm25_k10",
        "vector_cosine_k10",
    },
    "post_crud_queries": {"scalar_age", "spatial", "text", "vector"},
}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest()


def number(value: Any) -> float | None:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        result = float(value)
        return result if math.isfinite(result) else None
    return None


def nested(value: Any, *path: Any) -> Any:
    for part in path:
        if isinstance(part, int):
            if not isinstance(value, list) or part >= len(value):
                return None
            value = value[part]
        else:
            if not isinstance(value, dict) or part not in value:
                return None
            value = value[part]
    return value


def result_value(arm: Arm, *path: Any) -> float | None:
    return number(nested(arm, "raw", "result", *path))


def peak_value(arm: Arm, position: int, driver_name: str) -> float | None:
    values = [
        result_value(arm, "sampled_peak_bytes", position),
        number(nested(arm, "driver_sampled_peak", driver_name)),
    ]
    present = [value for value in values if value is not None]
    return max(present) if present else None


def rss_value(arm: Arm) -> float | None:
    values = [number(nested(arm, "driver_sampled_peak", "rss_bytes"))]
    text = nested(arm, "raw", "result", "rss_hwm")
    if isinstance(text, str):
        fields = text.split()
        if len(fields) >= 2 and fields[0].startswith("VmHWM:"):
            try:
                values.append(float(fields[1]) * 1024)
            except ValueError:
                pass
    present = [value for value in values if value is not None]
    return max(present) if present else None


def is_refused(arm: Arm) -> bool:
    return bool(nested(arm, "raw", "result", "refused"))


def is_completed(arm: Arm) -> bool:
    result = nested(arm, "raw", "result")
    crud = result.get("crud") if isinstance(result, dict) else None
    return (
        not is_refused(arm)
        and arm.get("returncode") == 0
        and isinstance(result, dict)
        and isinstance(result.get("engine"), str)
        and bool(result["engine"])
        and isinstance(crud, list)
        and len(crud) == 3
        and all(isinstance(record, dict) for record in crud)
    )


def validate_arm(arm: Any, position: int) -> None:
    if not isinstance(arm, dict):
        raise ValueError(f"arm {position} is not an object")
    result = nested(arm, "raw", "result")
    if arm.get("returncode") != 0 or not isinstance(result, dict):
        raise ValueError(
            f"arm {position} lacks explicit returncode=0 and a raw.result object"
        )
    if not is_refused(arm) and not is_completed(arm):
        raise ValueError(
            f"arm {position} is neither a typed refusal nor a valid completed workload"
        )


def escape(value: Any) -> str:
    return str(value).replace("|", "\\|").replace("\n", " ")


def spread(values: list[float], scale: float = 1.0) -> str:
    if not values:
        return "—"
    scaled = [value * scale for value in values]
    return (
        f"{statistics.median(scaled):.3f} "
        f"[{min(scaled):.3f}–{max(scaled):.3f}]"
    )


def metric_cell(arms: list[Arm], getter: Getter, scale: float = 1.0) -> str:
    completed = [arm for arm in arms if is_completed(arm)]
    values = [getter(arm) for arm in completed]
    if not completed or any(value is None for value in values):
        return "—"
    return spread([value for value in values if value is not None], scale)


def metric_rows(group: list[Arm]) -> list[tuple[str, str, Getter, float, bool]]:
    rows: list[tuple[str, str, Getter, float, bool]] = [
        ("Entity load", "s", lambda arm: result_value(arm, "entity_load", "seconds"), 1.0, True),
        ("Graph load", "s", lambda arm: result_value(arm, "graph_load", "seconds"), 1.0, True),
    ]
    for family in ("scalar", "vector", "spatial", "text"):
        rows.append(
            (
                f"{family.title()} late build",
                "s",
                lambda arm, family=family: result_value(arm, "builds", family, "seconds"),
                1.0,
                True,
            )
        )
    for cycle in range(3):
        getters: list[tuple[str, Getter]] = [
            ("update", lambda arm, cycle=cycle: result_value(arm, "crud", cycle, "update_seconds")),
            ("delete", lambda arm, cycle=cycle: result_value(arm, "crud", cycle, "delete_seconds")),
            (
                "reinsert+edges",
                lambda arm, cycle=cycle: result_value(arm, "crud", cycle, "reinsert_edges_seconds"),
            ),
        ]
        for label, getter in getters:
            rows.append((f"CRUD round {cycle + 1} {label}", "s", getter, 1.0, True))
        rows.append(
            (
                f"CRUD round {cycle + 1} total",
                "s",
                lambda arm, cycle=cycle: crud_total(arm, cycle),
                1.0,
                True,
            )
        )
    rows.append(("CRUD all three rounds total", "s", crud_all_rounds_total, 1.0, True))

    completed = [arm for arm in group if is_completed(arm)]
    for section, prefix in (("queries", "Pre-CRUD query"), ("post_crud_queries", "Post-CRUD query")):
        names = sorted(
            EXPECTED_QUERIES[section]
            | {
                name
                for arm in completed
                for name in (nested(arm, "raw", "result", section) or {})
            }
        )
        for name in names:
            rows.append(
                (
                    f"{prefix} `{name}`",
                    "ms",
                    lambda arm, section=section, name=name: result_value(
                        arm, section, name, "seconds"
                    ),
                    1000.0,
                    True,
                )
            )
    rows.extend(
        [
            ("Reopen", "ms", lambda arm: result_value(arm, "reopen_seconds"), 1000.0, True),
            ("Loaded logical", "MiB", lambda arm: result_value(arm, "loaded_bytes", 0), 1 / MIB, False),
            ("Loaded allocated", "MiB", lambda arm: result_value(arm, "loaded_bytes", 1), 1 / MIB, False),
            ("In-process final logical", "MiB", lambda arm: result_value(arm, "final_bytes", 0), 1 / MIB, False),
            ("In-process final allocated", "MiB", lambda arm: result_value(arm, "final_bytes", 1), 1 / MIB, False),
            ("After-close final logical", "MiB", lambda arm: number(nested(arm, "final_tree_bytes", "logical")), 1 / MIB, False),
            ("After-close final allocated", "MiB", lambda arm: number(nested(arm, "final_tree_bytes", "allocated")), 1 / MIB, False),
            ("Sampled peak logical", "MiB", lambda arm: peak_value(arm, 0, "logical_bytes"), 1 / MIB, False),
            ("Sampled peak allocated", "MiB", lambda arm: peak_value(arm, 1, "allocated_bytes"), 1 / MIB, False),
            ("Sampled peak / loaded logical", "×", lambda arm: peak_loaded_factor(arm, 0, "logical_bytes"), 1.0, False),
            ("Sampled peak / loaded allocated", "×", lambda arm: peak_loaded_factor(arm, 1, "allocated_bytes"), 1.0, False),
            ("RSS high-water", "MiB", rss_value, 1 / MIB, False),
        ]
    )
    return rows


def crud_total(arm: Arm, cycle: int) -> float | None:
    values = [
        result_value(arm, "crud", cycle, "update_seconds"),
        result_value(arm, "crud", cycle, "delete_seconds"),
        result_value(arm, "crud", cycle, "reinsert_edges_seconds"),
    ]
    return sum(values) if all(value is not None for value in values) else None


def crud_all_rounds_total(arm: Arm) -> float | None:
    values = [crud_total(arm, cycle) for cycle in range(3)]
    return sum(values) if all(value is not None for value in values) else None


def peak_loaded_factor(arm: Arm, position: int, driver_name: str) -> float | None:
    peak = peak_value(arm, position, driver_name)
    loaded = result_value(arm, "loaded_bytes", position)
    if peak is None or loaded is None or loaded <= 0:
        return None
    return peak / loaded


def paired_ratio(
    numerator: list[Arm], denominator: list[Arm], getter: Getter
) -> str:
    numerator = [arm for arm in numerator if is_completed(arm)]
    denominator = [arm for arm in denominator if is_completed(arm)]
    if len(numerator) != 3 or len(denominator) != 3:
        return "—"
    by_trial_n = {arm.get("trial"): arm for arm in numerator}
    by_trial_d = {arm.get("trial"): arm for arm in denominator}
    if len(by_trial_n) != 3 or by_trial_n.keys() != by_trial_d.keys():
        return "—"
    ratios = []
    for trial in sorted(by_trial_n, key=str):
        left = getter(by_trial_n[trial])
        right = getter(by_trial_d[trial])
        if left is None or right is None or right <= 0:
            return "—"
        ratios.append(left / right)
    return f"{spread(ratios)}×"


def render_group(group: list[Arm], rows: int, dimension: int, reader: str) -> list[str]:
    by_arm = {
        key: [arm for arm in group if (arm.get("engine"), arm.get("policy")) == key]
        for key in ARMS
    }
    scope = next(
        (
            nested(arm, "raw", "result", "reader_scope")
            for arm in group
            if nested(arm, "raw", "result", "reader_scope")
        ),
        reader,
    )
    out = [f"## N={rows:,}, dimension={dimension}, reader=`{escape(reader)}`", "", escape(scope), ""]
    out.extend(
        [
            "| Arm | Captured trials | Completed | Refused | Publication policy |",
            "|---|---:|---:|---:|---|",
        ]
    )
    for key in ARMS:
        members = by_arm[key]
        policies = sorted(
            {
                str(nested(arm, "raw", "result", "publication_policy"))
                for arm in members
                if nested(arm, "raw", "result", "publication_policy") is not None
            }
        )
        out.append(
            f"| {ARM_LABELS[key]} | {len(members)} | "
            f"{sum(is_completed(arm) for arm in members)} | {sum(is_refused(arm) for arm in members)} | "
            f"{escape('; '.join(policies) if policies else '—')} |"
        )
    out.extend(
        [
            "",
            "Values are medians [min–max] from completed arms only.",
            "",
            "| Metric | Unit | E4 atomic | E4 resumable | SQLite atomic |",
            "|---|---:|---:|---:|---:|",
        ]
    )
    rows_for_group = metric_rows(group)
    for label, unit, getter, scale, _ in rows_for_group:
        cells = []
        for key in ARMS:
            if label == "Vector late build" and key == ("sqlite", "atomic"):
                cells.append("N/A")
            else:
                cells.append(metric_cell(by_arm[key], getter, scale))
        out.append(f"| {label} | {unit} | {' | '.join(cells)} |")

    timing_rows = [row for row in rows_for_group if row[4]]
    out.extend(
        [
            "",
            "Ratios pair the same three trial numbers and are emitted only when both arms completed all three. "
            "E4 resumable publishes bounded build steps; E4 atomic and SQLite atomic DDL have different publication work.",
            "",
            "| Timing ratio | E4 atomic / SQLite | E4 resumable / SQLite |",
            "|---|---:|---:|",
        ]
    )
    for label, _, getter, _, _ in timing_rows:
        if label == "Vector late build":
            atomic = resumable = "—"
        else:
            atomic = paired_ratio(by_arm[("e4", "atomic")], by_arm[("sqlite", "atomic")], getter)
            resumable = paired_ratio(
                by_arm[("e4", "resumable")], by_arm[("sqlite", "atomic")], getter
            )
        out.append(f"| {label} | {atomic} | {resumable} |")
    out.append("")
    return out


def compact_json(value: Any) -> str:
    if value is None:
        return "—"
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def render_refusals(arms: list[Arm]) -> list[str]:
    refused = [arm for arm in arms if is_refused(arm)]
    out = ["## Resource refusals", ""]
    if not refused:
        return out + ["No typed resource refusals were recorded.", ""]
    out.extend(
        [
            "| N/dim | Reader | Arm/trial | Failed stage | Last confirmed commit | Confirmed boundaries | Oracle verified | Reason |",
            "|---|---|---|---|---:|---|---:|---|",
        ]
    )
    for arm in sorted(
        refused,
        key=lambda item: (
            item.get("rows", 0),
            item.get("dimension", 0),
            item.get("reader_mode", ""),
            item.get("engine", ""),
            item.get("policy", ""),
            item.get("trial", -1),
        ),
    ):
        result = nested(arm, "raw", "result") or {}
        progress = result.get("progress") or {}
        oracle = result.get("refusal_oracle") or {}
        out.append(
            "| {rows:,}/{dimension} | `{reader}` | {engine} {policy} / {trial} | {stage} | {commit} | "
            "`{boundaries}` | {verified} | {reason} |".format(
                rows=arm.get("rows", 0),
                dimension=arm.get("dimension", "?"),
                reader=escape(arm.get("reader_mode", "?")),
                engine=escape(arm.get("engine", "?")),
                policy=escape(arm.get("policy", "?")),
                trial=escape(arm.get("trial", "?")),
                stage=escape(progress.get("failed_stage", "unknown")),
                commit=escape(progress.get("last_confirmed_commit", "—")),
                boundaries=escape(compact_json(progress.get("committed_boundaries"))),
                verified="yes" if oracle.get("committed_state_verified") is True else "no",
                reason=escape(result.get("reason", "unknown")),
            )
        )
    out.append("")
    return out


def render_report(report: dict[str, Any], raw_sha256: str) -> str:
    if report.get("format") != "phase2-multimodel-driver-v2":
        raise ValueError("only phase2-multimodel-driver-v2 reports are accepted")
    arms = report.get("arms")
    if not isinstance(arms, list):
        raise ValueError("driver-v2 report lacks arms array")
    for position, arm in enumerate(arms):
        validate_arm(arm, position)
    derived_refusals = sum(is_refused(arm) for arm in arms)
    derived_completed = sum(is_completed(arm) for arm in arms)
    counts = report.get("completion_counts") or {}
    capture_complete = report.get("capture_complete") is True
    workload_complete = report.get("workload_complete") is True
    state = (
        "CANDIDATE WORKLOAD CAPTURED — not public acceptance"
        if capture_complete and workload_complete
        else "INCOMPLETE — candidate evidence only, not public acceptance"
    )
    out = [
        "# Phase 2 multimodel benchmark report",
        "",
        f"**Evidence state:** {state}",
        "",
        f"- Raw report SHA-256: `{raw_sha256}`",
        f"- Benchmark binary SHA-256: `{escape(report.get('binary_sha256', 'unknown'))}`",
        f"- Driver format: `{report['format']}`",
        f"- Capture complete: `{str(capture_complete).lower()}`",
        f"- Workload complete: `{str(workload_complete).lower()}`",
        f"- Captured arms: {len(arms)}",
        f"- Driver scheduled arms: {counts.get('scheduled', 'unknown')}",
        f"- Completed workloads: {counts.get('completed_workloads', derived_completed)}",
        f"- Typed resource refusals: {counts.get('resource_refusals', derived_refusals)}",
        f"- Nonzero process exits: {counts.get('nonzero_process_exits', sum(arm.get('returncode', 0) != 0 for arm in arms))}",
        "",
        "All numeric summaries below exclude refused arms. A dash means no complete comparable measurement; it never means zero. "
        "Sampled disk peaks are lower bounds because polling can miss transients and unlinked SQLite temporary files. "
        "The in-process final sample is taken while the benchmark process and live database files still exist; the after-close final "
        "tree is measured by the driver after process exit. RSS includes the benchmark harness, deterministic generator and any "
        "in-process correctness work.",
        "",
    ]
    groups: dict[tuple[int, int, str], list[Arm]] = {}
    for arm in arms:
        key = (int(arm.get("rows", 0)), int(arm.get("dimension", 0)), str(arm.get("reader_mode", "unknown")))
        groups.setdefault(key, []).append(arm)
    for (rows, dimension, reader), group in sorted(groups.items()):
        out.extend(render_group(group, rows, dimension, reader))
    out.extend(render_refusals(arms))
    return "\n".join(out).rstrip() + "\n"


def synthetic_arm(
    engine: str, policy: str, trial: int, value: float, refused: bool = False
) -> Arm:
    result: dict[str, Any] = {
        "engine": engine,
        "reader_mode": "none",
        "reader_scope": "no concurrent reader",
        "publication_policy": policy,
    }
    if refused:
        result.update(
            {
                "refused": True,
                "reason": "typed limit",
                "progress": {
                    "failed_stage": "crud_update",
                    "last_confirmed_commit": 4,
                    "committed_boundaries": {"entities": 8},
                    "completed_stages": {"fake_seconds": 999.0},
                },
                "refusal_oracle": {"committed_state_verified": True},
            }
        )
    else:
        result.update(
            {
                "entity_load": {"seconds": value},
                "graph_load": {"seconds": value},
                "builds": {name: {"seconds": value} for name in ("scalar", "vector", "spatial", "text")},
                "crud": [
                    {"update_seconds": value, "delete_seconds": value, "reinsert_edges_seconds": value}
                    for _ in range(3)
                ],
                "queries": {"q": {"seconds": value}},
                "post_crud_queries": {"q": {"seconds": value}},
                "reopen_seconds": value,
                "loaded_bytes": [MIB, MIB],
                "final_bytes": [2 * MIB, 2 * MIB],
                "sampled_peak_bytes": [3 * MIB, 3 * MIB],
                "rss_hwm": "VmHWM:\t4096 kB",
            }
        )
    return {
        "engine": engine,
        "policy": policy,
        "reader_mode": "none",
        "rows": 8,
        "dimension": 32,
        "trial": trial,
        "returncode": 0,
        "raw": {"result": result},
        "driver_sampled_peak": {
            "logical_bytes": 3 * MIB,
            "allocated_bytes": 3 * MIB,
            "rss_bytes": 4 * MIB,
        },
        "final_tree_bytes": {"logical": 2 * MIB, "allocated": 2 * MIB},
    }


def self_test() -> None:
    completed = [
        synthetic_arm(engine, policy, trial, 1.0 + trial)
        for engine, policy in ARMS
        for trial in range(3)
    ]
    report = {
        "format": "phase2-multimodel-driver-v2",
        "binary_sha256": "binary",
        "capture_complete": True,
        "workload_complete": True,
        "arms": completed,
    }
    rendered = render_report(report, "raw")
    assert "2.000 [1.000–3.000]" in rendered
    assert "SQLite atomic" in rendered and "Vector late build | s |" in rendered
    assert "1.000 [1.000–1.000]×" in rendered
    assert "| CRUD all three rounds total | s | 18.000 [9.000–27.000]" in rendered
    assert "| After-close final logical | MiB | 2.000 [2.000–2.000]" in rendered
    assert "| Sampled peak / loaded logical | × | 3.000 [3.000–3.000]" in rendered

    mixed = completed[:]
    mixed[0] = synthetic_arm("e4", "atomic", 0, 999.0, refused=True)
    report["arms"] = mixed
    report["workload_complete"] = False
    rendered = render_report(report, "raw")
    assert "crud_update" in rendered and "Oracle verified" in rendered
    assert "999.000" not in rendered
    assert "INCOMPLETE" in rendered

    report["arms"] = [
        synthetic_arm(engine, policy, trial, 999.0, refused=True)
        for engine, policy in ARMS
        for trial in range(3)
    ]
    rendered = render_report(report, "raw")
    assert "| Entity load | s | — | — | — |" in rendered
    assert "999.000" not in rendered

    try:
        render_report({"format": "phase2-multimodel-driver-v1", "arms": []}, "raw")
    except ValueError:
        pass
    else:
        raise AssertionError("driver-v1 report was not refused")

    malformed = synthetic_arm("e4", "atomic", 0, 1.0)
    malformed.pop("returncode")
    for bad_arm in (malformed, {"returncode": 0, "raw": {}}):
        try:
            render_report(
                {"format": "phase2-multimodel-driver-v2", "arms": [bad_arm]},
                "raw",
            )
        except ValueError:
            pass
        else:
            raise AssertionError("malformed arm was counted as completed")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("report", type=Path, nargs="?")
    parser.add_argument("output", type=Path, nargs="?")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        if args.report is not None or args.output is not None:
            parser.error("--self-test does not accept report/output paths")
        self_test()
        print("phase2_multimodel_report self-test: PASS")
        return
    if args.report is None or args.output is None:
        parser.error("REPORT and NEW_OUTPUT are required")
    report_path = args.report.resolve(strict=True)
    output_path = args.output.resolve(strict=False)
    if output_path.exists():
        raise SystemExit("NEW_OUTPUT must not exist")
    if not output_path.parent.exists():
        raise SystemExit("NEW_OUTPUT parent directory must exist")
    report = json.loads(report_path.read_text())
    rendered = render_report(report, sha256(report_path))
    output_path.write_text(rendered)


if __name__ == "__main__":
    main()
