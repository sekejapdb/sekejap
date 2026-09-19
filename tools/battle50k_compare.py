#!/usr/bin/env python3
"""battle50k_compare.py -- compare an E4 report and a PostgreSQL report that
both follow the battle50k SHARED JSON REPORT CONTRACT (see
brief-common-b50k.md).

Usage:
    battle50k_compare.py e4.json postgres.json [--md]

Prints, in order:
  1. a stages table (name, e4 (ms), pg (ms), ratio (e4/pg)); the
     "disk_bytes" stage is reported in MiB instead of ms.
  2. a cases table (case, kind, e4 median (us), pg median (us),
     ratio (e4/pg), e4 rows, pg rows, result) where "result" is
     AGREE/DISAGREE for filter cases, a first_keys top-k overlap for
     ranked cases, and each arm's recall_at_k for approx cases; a case
     whose median_us is null in either arm prints "n/a: <note>" instead
     of numeric comparisons.
  3. the deviations of both arms.
  4. a one-line verdict: filter agreement count and how many cases (of
     those with a timing in both arms) E4 was faster on.

Exit code 1 if any filter-kind case has a row-count DISAGREE between the
two arms (both totals present and unequal); 0 otherwise.

stdlib only.
"""

import argparse
import json
import sys


UNIT_MS = "ms"
UNIT_US = "us"
UNIT_MIB = "MiB"
BYTES_PER_MIB = 1024.0 * 1024.0


def load_report(path):
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


def index_by_name(items):
    out = {}
    for item in items or []:
        out[item.get("name")] = item
    return out


def ordered_union_names(a_items, b_items):
    seen = []
    seen_set = set()
    for item in a_items or []:
        n = item.get("name")
        if n not in seen_set:
            seen.append(n)
            seen_set.add(n)
    for item in b_items or []:
        n = item.get("name")
        if n not in seen_set:
            seen.append(n)
            seen_set.add(n)
    return seen


def fmt_num(value, decimals=3):
    if value is None:
        return "n/a"
    return f"{value:.{decimals}f}"


def fmt_ratio(a, b, suffix="x"):
    if a is None or b is None or b == 0:
        return "n/a"
    return f"{(a / b):.3f}{suffix}"


def print_table(headers, rows, md):
    if md:
        print("| " + " | ".join(headers) + " |")
        print("| " + " | ".join(["---"] * len(headers)) + " |")
        for row in rows:
            print("| " + " | ".join(str(c) for c in row) + " |")
        return

    widths = [len(h) for h in headers]
    str_rows = [[str(c) for c in row] for row in rows]
    for row in str_rows:
        for i, cell in enumerate(row):
            widths[i] = max(widths[i], len(cell))

    def fmt_row(cells):
        return "  ".join(cell.ljust(widths[i]) for i, cell in enumerate(cells))

    print(fmt_row(headers))
    print(fmt_row(["-" * w for w in widths]))
    for row in str_rows:
        print(fmt_row(row))


def build_stages_table(e4_report, pg_report):
    e4_stages = e4_report.get("stages", [])
    pg_stages = pg_report.get("stages", [])
    e4_idx = index_by_name(e4_stages)
    pg_idx = index_by_name(pg_stages)
    names = ordered_union_names(e4_stages, pg_stages)

    headers = [
        "stage",
        f"e4 ({UNIT_MS})",
        f"pg ({UNIT_MS})",
        "ratio (e4/pg)",
    ]
    rows = []
    for name in names:
        e4_entry = e4_idx.get(name)
        pg_entry = pg_idx.get(name)
        if name == "disk_bytes":
            e4_v = e4_entry.get("bytes") if e4_entry else None
            pg_v = pg_entry.get("bytes") if pg_entry else None
            e4_mib = (e4_v / BYTES_PER_MIB) if e4_v is not None else None
            pg_mib = (pg_v / BYTES_PER_MIB) if pg_v is not None else None
            rows.append(
                [
                    f"{name} ({UNIT_MIB})",
                    fmt_num(e4_mib) if e4_mib is not None else "n/a",
                    fmt_num(pg_mib) if pg_mib is not None else "n/a",
                    fmt_ratio(e4_mib, pg_mib),
                ]
            )
            continue
        e4_v = e4_entry.get("ms") if e4_entry else None
        pg_v = pg_entry.get("ms") if pg_entry else None
        rows.append(
            [
                name,
                fmt_num(e4_v) if e4_v is not None else "n/a",
                fmt_num(pg_v) if pg_v is not None else "n/a",
                fmt_ratio(e4_v, pg_v),
            ]
        )
    return headers, rows


def case_result(kind, e4_case, pg_case):
    if kind == "filter":
        e4_rows = e4_case.get("total_rows") if e4_case else None
        pg_rows = pg_case.get("total_rows") if pg_case else None
        if e4_rows is None or pg_rows is None:
            return "n/a", None
        if e4_rows == pg_rows:
            return "AGREE", True
        return "DISAGREE", False

    if kind == "ranked":
        e4_keys = (e4_case or {}).get("first_keys") or []
        pg_keys = (pg_case or {}).get("first_keys") or []
        k = (e4_case or {}).get("k") or (pg_case or {}).get("k") or 10
        overlap = len(set(e4_keys) & set(pg_keys))
        return f"overlap {overlap}/{k}", None

    if kind == "approx":
        e4_recall = (e4_case or {}).get("recall_at_k")
        pg_recall = (pg_case or {}).get("recall_at_k")
        e4_txt = fmt_num(e4_recall) if e4_recall is not None else "n/a"
        pg_txt = fmt_num(pg_recall) if pg_recall is not None else "n/a"
        return f"recall e4={e4_txt} pg={pg_txt}", None

    return "n/a", None


def build_cases_table(e4_report, pg_report):
    e4_cases = e4_report.get("cases", [])
    pg_cases = pg_report.get("cases", [])
    e4_idx = index_by_name(e4_cases)
    pg_idx = index_by_name(pg_cases)
    names = ordered_union_names(e4_cases, pg_cases)

    headers = [
        "case",
        "kind",
        f"e4 median ({UNIT_US})",
        f"pg median ({UNIT_US})",
        "ratio (e4/pg)",
        "e4 rows",
        "pg rows",
        "result",
    ]
    rows = []
    filter_total = 0
    filter_agree = 0
    timed_total = 0
    e4_faster = 0
    disagreements = []

    for name in names:
        e4_case = e4_idx.get(name)
        pg_case = pg_idx.get(name)
        kind = (e4_case or pg_case or {}).get("kind", "?")

        e4_median = (e4_case or {}).get("median_us")
        pg_median = (pg_case or {}).get("median_us")
        e4_rows = (e4_case or {}).get("total_rows")
        pg_rows = (pg_case or {}).get("total_rows")

        if kind == "filter":
            filter_total += 1

        if e4_median is None or pg_median is None:
            note = (e4_case or {}).get("note") or (pg_case or {}).get("note") or ""
            median_cell = f"n/a: {note}" if note else "n/a"
            rows.append(
                [
                    name,
                    kind,
                    median_cell,
                    median_cell,
                    "n/a",
                    e4_rows if e4_rows is not None else "n/a",
                    pg_rows if pg_rows is not None else "n/a",
                    case_result(kind, e4_case, pg_case)[0],
                ]
            )
            result_text, agree = case_result(kind, e4_case, pg_case)
            if kind == "filter" and agree is True:
                filter_agree += 1
            if kind == "filter" and agree is False:
                disagreements.append(name)
            continue

        timed_total += 1
        if e4_median < pg_median:
            e4_faster += 1

        result_text, agree = case_result(kind, e4_case, pg_case)
        if kind == "filter" and agree is True:
            filter_agree += 1
        if kind == "filter" and agree is False:
            disagreements.append(name)

        rows.append(
            [
                name,
                kind,
                fmt_num(e4_median),
                fmt_num(pg_median),
                fmt_ratio(e4_median, pg_median),
                e4_rows if e4_rows is not None else "n/a",
                pg_rows if pg_rows is not None else "n/a",
                result_text,
            ]
        )

    stats = {
        "filter_total": filter_total,
        "filter_agree": filter_agree,
        "timed_total": timed_total,
        "e4_faster": e4_faster,
        "disagreements": disagreements,
    }
    return headers, rows, stats


def print_deviations(label, report):
    deviations = report.get("deviations", [])
    if not deviations:
        print(f"  {label}: (none)")
        return
    for d in deviations:
        case = d.get("case", "*")
        text = d.get("text", "")
        print(f"  {label}: [{case}] {text}")


def main():
    parser = argparse.ArgumentParser(description="Compare battle50k E4/PG reports")
    parser.add_argument("e4_report", help="path to the e4 arm's JSON report")
    parser.add_argument("pg_report", help="path to the postgres arm's JSON report")
    parser.add_argument(
        "--md", action="store_true", help="print GitHub markdown tables"
    )
    args = parser.parse_args()

    e4_report = load_report(args.e4_report)
    pg_report = load_report(args.pg_report)

    if e4_report.get("arm") != "e4":
        print(f"warning: {args.e4_report} arm field is {e4_report.get('arm')!r}, expected 'e4'", file=sys.stderr)
    if pg_report.get("arm") != "postgres":
        print(f"warning: {args.pg_report} arm field is {pg_report.get('arm')!r}, expected 'postgres'", file=sys.stderr)

    print(f"rows: e4={e4_report.get('rows')} pg={pg_report.get('rows')}")
    print(f"commit: e4={e4_report.get('commit')} pg={pg_report.get('commit')}")
    print()

    print("STAGES")
    s_headers, s_rows = build_stages_table(e4_report, pg_report)
    print_table(s_headers, s_rows, args.md)
    print()

    print("CASES")
    c_headers, c_rows, stats = build_cases_table(e4_report, pg_report)
    print_table(c_headers, c_rows, args.md)
    print()

    print("DEVIATIONS")
    print_deviations("e4", e4_report)
    print_deviations("postgres", pg_report)
    print()

    filter_total = stats["filter_total"]
    filter_agree = stats["filter_agree"]
    timed_total = stats["timed_total"]
    e4_faster = stats["e4_faster"]

    print(
        f"VERDICT: filter cases agreeing {filter_agree}/{filter_total}; "
        f"E4 faster in {e4_faster}/{timed_total} cases with both medians present ({UNIT_US})."
    )

    if stats["disagreements"]:
        print(
            "DISAGREEMENTS: " + ", ".join(stats["disagreements"]),
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
