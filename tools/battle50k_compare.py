#!/usr/bin/env python3
"""battle50k_compare.py -- compare the battle50k arms on one corpus.

Usage:
    battle50k_compare.py <report.json> ... [--frozen <dir>] [--md]

One to four reports, in any order: each is routed by its own `arm` field, so
`e4.json pg.json e4-sql.json sqlite.json` and `sqlite.json e4.json` mean the
same thing. The `e4` report is required -- it is the arm every ratio is
measured against.

FROZEN REFERENCES. Postgres and SQLite are CONSTANTS of this battery: they
are not being changed by the work under measurement, so rerunning them per
E4 pass measures the same two engines again. A run of
`$SEKEJAP_BENCH_ROOT/bench50k/frozen` (or `--frozen <dir>`) holds `pg-50k.json` and
`sqlite-50k.json` with a `README.md` and a `manifest.json` recording the
date, the engine versions, the machine, the corpus SHA-256 and the exact
commands that produced them. An invocation that names no postgres and no
sqlite report -- `battle50k_compare.py <e4.json> <e4-sql.json>` -- fills
those two arms from that directory instead of rerunning them. Whenever a
frozen reference is used, its provenance is CHECKED and never assumed: the
corpus the live reports name is hashed and must equal the manifest's
`corpus_sha256`, and no report may be older than the corpus file itself. A
mismatch is REFUSED with a message that says which number differed, because
a frozen Postgres number measured on a different corpus is not a reference,
it is a wrong answer.

Prints, in order:
  1. a stages table (name, one column per arm in ms; the "disk_bytes" stage
     in MiB instead), with the e4/pg ratio.
  2. a CASES table (case, kind, one median per arm, the E4/PG and E4/SQLITE
     ratios, the e4-sql run/prepare split when that arm is present, the
     e4-sql PREPARED median and its ratio to e4 when that arm ran with
     `--prepared`, one row count per arm, and a result). "result" is AGREE/DISAGREE with the row
     count for filter cases -- across EVERY arm that ran the case, not two --
     a first_keys top-k overlap for ranked cases, and each arm's recall_at_k
     for approx cases. A case whose median_us is null in an arm prints
     "n/a: <that arm's note>", which is how a case an engine cannot express
     reaches the table instead of vanishing from it.
  3. for each approximate base (`vec_ann_10`, `vec_ann_10_kind`): the full
     sweep table for every arm (point, recall, median_us, p90_us), then a
     HEADLINE line -- the cheapest point in each of E4 and Postgres with
     recall_at_k >= 0.95 and the E4/PG ratio of their median_us AT THAT
     RECALL, which is the number that means something for approximate vector
     search (equal ef / search_list_size numerals are not the same knob). An
     arm that never reaches 0.95 reports its best recall instead of a ratio.
  4. the deviations of every arm.
  5. ANOMALIES: every case where E4 is SLOWER than Postgres or than SQLite,
     worst ratio first, and every filter case the arms disagree on. This is
     the section the battery exists for.
  6. a one-line verdict.

Exit code 1 if any filter-kind case has a row-count DISAGREE between arms
that both ran it; 0 otherwise.

stdlib only.
"""

import argparse
import hashlib
import json
import os
import sys
import tempfile


UNIT_MS = "ms"
UNIT_US = "us"
UNIT_MIB = "MiB"
BYTES_PER_MIB = 1024.0 * 1024.0

# The two case bases whose single approximate point is a SWEEP: E4 points
# are named "<base>@ef<N>", Postgres points "<base>@sls<N>" (plus one
# "<base>@sls100+resc<N>" rescore probe per base when the server exposes
# diskann.query_rescore). See battle50k.rs's "APPROXIMATE SWEEP" doc.
APPROX_BASES = ["vec_ann_10", "vec_ann_10_kind"]
HEADLINE_RECALL = 0.95

# Where the frozen Postgres and SQLite references live, and what they are
# called inside it. Postgres and SQLite are constants of this battery; an E4
# pass compares against these instead of rerunning two engines that did not
# change. Rooted at SEKEJAP_BENCH_ROOT (default: the system temp dir);
# overridable with --frozen.
FROZEN_DIR = os.path.join(
    os.environ.get("SEKEJAP_BENCH_ROOT", tempfile.gettempdir()), "bench50k", "frozen"
)
FROZEN_FILES = {"postgres": "pg-50k.json", "sqlite": "sqlite-50k.json"}
FROZEN_MANIFEST = "manifest.json"

# The arms this table knows, in the column order it prints them. `e4` is
# first because every ratio is measured against it.
ARM_ORDER = ["e4", "postgres", "e4-sql", "sqlite"]
ARM_LABEL = {"e4": "e4", "postgres": "pg", "e4-sql": "e4-sql", "sqlite": "sqlite"}

# How much of an arm's `n/a` note one table cell may carry. The whole text is
# always printed in DEVIATIONS; this is only the pointer to it.
NOTE_WIDTH = 46


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


def build_stages_table(reports):
    indexed = [(label, index_by_name(report.get("stages", []))) for label, report in reports]
    names = []
    seen = set()
    for _, report in reports:
        for stage in report.get("stages", []):
            name = stage.get("name")
            if name not in seen:
                names.append(name)
                seen.add(name)

    headers = ["stage"]
    for label, _ in reports:
        headers.append(f"{label} ({UNIT_MS})")
    headers.append("ratio (e4/pg)")
    rows = []
    for name in names:
        entries = [idx.get(name) for _, idx in indexed]
        if name == "disk_bytes":
            values = [
                (e.get("bytes") / BYTES_PER_MIB) if e and e.get("bytes") is not None else None
                for e in entries
            ]
            row = [f"{name} ({UNIT_MIB})"]
        else:
            values = [e.get("ms") if e else None for e in entries]
            row = [name]
        for value in values:
            row.append(fmt_num(value) if value is not None else "n/a")
        row.append(fmt_ratio(values[0], values[1]))
        rows.append(row)
    return headers, rows


def ran(case):
    """Did this arm actually run this case? A case an arm cannot express is
    present in its report with a null median and an `n/a:` note, so the row
    count beside it is not an answer and must not enter an agreement."""
    return case is not None and case.get("median_us") is not None


def case_result(kind, cases):
    """`cases` is a list of (label, case-or-None) in ARM_ORDER, the first two
    being e4 and pg. A filter case agrees only when EVERY arm that ran it
    returned the same row count, and the count is printed with the verdict so
    the table says what they agreed ON."""
    e4_case = cases[0][1]
    pg_case = cases[1][1]
    # A WRITE case agrees the way a filter case does: `total_rows` is the rows
    # the arm PUT, and two arms that wrote the same number wrote the same
    # batch. Nothing else about a write is comparable across engines.
    if kind in ("filter", "write"):
        counts = [(label, c.get("total_rows")) for label, c in cases if ran(c)]
        counts = [(label, v) for label, v in counts if v is not None]
        if len(counts) < 2:
            only = counts[0][0] if counts else "no"
            return f"n/a: only the {only} arm ran this case", None
        values = {v for _, v in counts}
        if len(values) == 1:
            unit = "rows written" if kind == "write" else "rows"
            return f"AGREE ({counts[0][1]} {unit} across {len(counts)} arms)", True
        return (
            "DISAGREE (" + ", ".join(f"{label}={v}" for label, v in counts) + ")",
            False,
        )

    if kind == "ranked":
        e4_keys = set((e4_case or {}).get("first_keys") or [])
        pg_keys = set((pg_case or {}).get("first_keys") or [])
        k = (e4_case or {}).get("k") or (pg_case or {}).get("k") or 10
        text = f"e4/pg overlap {len(e4_keys & pg_keys)}/{k}"
        for label, c in cases[2:]:
            if not ran(c):
                continue
            keys = set((c or {}).get("first_keys") or [])
            text += f"; {label} vs e4 {len(keys & e4_keys)}/{max(len(e4_keys), 1)}"
        return text, None

    if kind == "approx":
        parts = []
        for label, c in cases:
            if c is None:
                continue
            recall = c.get("recall_at_k")
            parts.append(f"{label}={fmt_num(recall) if recall is not None else 'n/a'}")
        return "recall " + " ".join(parts), None

    return "n/a", None


def build_cases_table(reports, sql_at, lite_at):
    """`reports` is a list of (label, report) in ARM_ORDER; the first two are
    always e4 and pg. `sql_at` / `lite_at` are the COLUMN INDEX of the e4-sql
    and sqlite arms, or None -- remembered rather than assumed, because a
    table that read the sqlite column as the e4-sql one would print a ratio
    between two arms that never met."""
    indexed = [(label, index_by_name(report.get("cases", []))) for label, report in reports]
    names = []
    seen = set()
    for _, report in reports:
        for name in ordered_union_names(report.get("cases", []), []):
            if name not in seen:
                names.append(name)
                seen.add(name)

    # Did the e4-sql arm run with `--prepared`? One case carrying the field
    # is enough: the flag is per RUN, not per case.
    sql_prepared = False
    if sql_at is not None:
        sql_prepared = any(
            (case or {}).get("prepared_median_us") is not None
            for case in indexed[sql_at][1].values()
        )

    headers = ["case", "kind"]
    for label, _ in reports:
        headers.append(f"{label} median ({UNIT_US})")
    headers.append("ratio (e4/pg)")
    if lite_at is not None:
        headers.append("ratio (e4/sqlite)")
    if sql_at is not None:
        headers.append("ratio (e4-sql run/e4)")
        headers.append(f"e4-sql prepare ({UNIT_US})")
        # Only when the e4-sql report was produced with `--prepared`: the
        # same statement prepared ONCE and re-bound per instance. A report
        # without it prints neither column, so an older report's table is
        # unchanged.
        if sql_prepared:
            headers.append(f"e4-sql prepared ({UNIT_US})")
            headers.append("ratio (e4-sql prepared/e4)")
    for label, _ in reports:
        headers.append(f"{label} rows")
    headers.append("result")

    rows = []
    filter_total = 0
    filter_agree = 0
    timed_total = 0
    e4_faster = 0
    disagreements = []
    # Every case where E4 is SLOWER than an engine that ran it, as
    # (ratio, case, engine label, e4 us, engine us). The ANOMALIES section.
    slower = []

    for name in names:
        cases = [(label, idx.get(name)) for label, idx in indexed]
        kind = next((c.get("kind") for _, c in cases if c), "?")
        medians = [(c or {}).get("median_us") for _, c in cases]
        counts = [(c or {}).get("total_rows") for _, c in cases]

        if kind in ("filter", "write"):
            filter_total += 1

        result_text, agree = case_result(kind, cases)
        if kind in ("filter", "write") and agree is True:
            filter_agree += 1
        if kind in ("filter", "write") and agree is False:
            disagreements.append((name, result_text))

        if medians[0] is not None and medians[1] is not None:
            timed_total += 1
            if medians[0] < medians[1]:
                e4_faster += 1

        # The anomaly hunt: E4 against each OTHER ENGINE, never against the
        # second E4 arm -- e4-sql is the same engine asked in SQL, so a
        # difference there is a parser cost and not an engine losing.
        for at, label in [(1, "pg"), (lite_at, "sqlite")]:
            if at is None or medians[0] is None or medians[at] is None:
                continue
            if medians[at] > 0 and medians[0] > medians[at]:
                slower.append((medians[0] / medians[at], name, label, medians[0], medians[at]))

        row = [name, kind]
        for (label, case), value in zip(cases, medians):
            if value is not None:
                row.append(fmt_num(value))
            elif case is None:
                row.append("n/a")
            else:
                # A null median is a named deviation, never a blank: print
                # the arm's note beside the n/a so the table says WHY. The
                # note is CLIPPED here -- one table column is not where a
                # paragraph belongs; its full text is in DEVIATIONS.
                note = (case.get("note") or "").strip()
                text = note if note.startswith("n/a") else (f"n/a: {note}" if note else "n/a")
                row.append(text if len(text) <= NOTE_WIDTH else text[: NOTE_WIDTH - 1] + "\u2026")
        row.append(fmt_ratio(medians[0], medians[1]))
        if lite_at is not None:
            row.append(fmt_ratio(medians[0], medians[lite_at]))
        if sql_at is not None:
            # The e4-sql arm's RUN median (wall minus its own prepare) against
            # the e4 arm's median: the engine cost on the same footing.
            # Falls back to the wall median when the report predates the field.
            third = cases[sql_at][1] or {}
            run_median = third.get("run_median_us")
            if run_median is None:
                run_median = medians[sql_at]
            row.append(fmt_ratio(run_median, medians[0]))
            prepare = third.get("prepare_median_us")
            row.append(fmt_num(prepare, 1) if prepare is not None else "n/a")
            if sql_prepared:
                prepared = third.get("prepared_median_us")
                if prepared is None:
                    # The case writes its value INTO the statement, so there
                    # is no one statement to prepare. Named, not blank.
                    row.append("n/a: statement text varies per instance")
                    row.append("n/a")
                else:
                    row.append(fmt_num(prepared, 1))
                    row.append(fmt_ratio(prepared, medians[0]))
        for (_, case), value in zip(cases, counts):
            # A row count belongs to an ANSWER. An arm that could not express
            # the case has none, and a zero there would read as "no rows".
            row.append(value if ran(case) and value is not None else "n/a")
        row.append(result_text)
        rows.append(row)

    slower.sort(key=lambda entry: entry[0], reverse=True)
    stats = {
        "filter_total": filter_total,
        "filter_agree": filter_agree,
        "timed_total": timed_total,
        "e4_faster": e4_faster,
        "disagreements": disagreements,
        "slower": slower,
    }
    return headers, rows, stats


def print_anomalies(stats, md):
    """The section the battery exists for: every case where E4 lost, worst
    ratio first, then every case the arms answered differently."""
    print("ANOMALIES")
    slower = stats["slower"]
    if not slower:
        print("  E4 is at least as fast as every other engine on every case both ran.")
    else:
        headers = [
            "case",
            "slower than",
            f"e4 ({UNIT_US})",
            f"engine ({UNIT_US})",
            "e4 / engine",
        ]
        rows = [
            [name, label, fmt_num(e4_us, 1), fmt_num(other_us, 1), f"{ratio:.3f}x"]
            for ratio, name, label, e4_us, other_us in slower
        ]
        print_table(headers, rows, md)
    print()
    if not stats["disagreements"]:
        print("  No filter case disagrees: every arm that ran one returned the same rows.")
    else:
        for name, text in stats["disagreements"]:
            print(f"  DISAGREE {name}: {text}")
    print()


def sweep_points(cases, base):
    """(label, recall, median_us, p90_us) for every case in `cases` named
    `<base>@<label>`, in the report's own order."""
    prefix = base + "@"
    out = []
    for c in cases or []:
        name = c.get("name", "")
        if not name.startswith(prefix):
            continue
        out.append(
            (
                name[len(prefix):],
                c.get("recall_at_k"),
                c.get("median_us"),
                c.get("p90_us"),
            )
        )
    return out


def cheapest_at_recall(points, threshold=HEADLINE_RECALL):
    """(label, median_us, recall, hit) for the cheapest point with
    recall_at_k >= threshold; if none clears it, the point with the HIGHEST
    recall instead (ties broken by lower median_us), with hit=False. All
    four fields are None when `points` has no recall at all."""
    cleared = [
        p for p in points if p[1] is not None and p[1] >= threshold and p[2] is not None
    ]
    if cleared:
        label, recall, median_us, _p90 = min(cleared, key=lambda p: p[2])
        return label, median_us, recall, True
    scored = [p for p in points if p[1] is not None]
    if not scored:
        return None, None, None, False
    label, recall, median_us, _p90 = max(
        scored,
        key=lambda p: (p[1], -(p[2] if p[2] is not None else float("inf"))),
    )
    return label, median_us, recall, False


def print_sweep_table(base, reports, md):
    per_arm = [(label, sweep_points(report.get("cases", []), base)) for label, report in reports]
    e4_points = per_arm[0][1]
    pg_points = per_arm[1][1]
    headers = ["arm", "point", "recall", f"median ({UNIT_US})", f"p90 ({UNIT_US})"]
    rows = []
    for arm_label, points in per_arm:
        for label, recall, median_us, p90_us in points:
            rows.append(
                [
                    arm_label,
                    label,
                    fmt_num(recall),
                    fmt_num(median_us, 1),
                    fmt_num(p90_us, 1),
                ]
            )
    print(f"SWEEP {base}")
    print_table(headers, rows, md)
    return e4_points, pg_points


def sweep_headline(base, e4_points, pg_points, threshold=HEADLINE_RECALL):
    """Print the HEADLINE line for one approximate base and return its
    stats, so a caller (or a test) can check the numbers rather than just
    the text."""
    e4_label, e4_us, e4_recall, e4_hit = cheapest_at_recall(e4_points, threshold)
    pg_label, pg_us, pg_recall, pg_hit = cheapest_at_recall(pg_points, threshold)

    if e4_hit and pg_hit:
        ratio = fmt_ratio(e4_us, pg_us)
        print(
            f"HEADLINE {base}: recall>={threshold} -- "
            f"e4 {e4_label} = {fmt_num(e4_us, 1)} {UNIT_US}; "
            f"pg {pg_label} = {fmt_num(pg_us, 1)} {UNIT_US}; e4/pg = {ratio}"
        )
    else:

        def describe(name, label, us, recall, hit):
            if label is None:
                return f"{name}: no sweep points"
            if hit:
                return f"{name} cheapest at recall>={threshold}: {label} (recall={fmt_num(recall)}, median_us={fmt_num(us, 1)})"
            return (
                f"{name} never reached recall>={threshold}; best is {label} "
                f"(recall={fmt_num(recall)}, median_us={fmt_num(us, 1)})"
            )

        print(
            f"HEADLINE {base}: no cross-arm ratio at recall>={threshold} -- "
            + describe("e4", e4_label, e4_us, e4_recall, e4_hit)
            + "; "
            + describe("pg", pg_label, pg_us, pg_recall, pg_hit)
        )
    print()
    return {
        "e4_label": e4_label,
        "e4_us": e4_us,
        "e4_recall": e4_recall,
        "e4_hit": e4_hit,
        "pg_label": pg_label,
        "pg_us": pg_us,
        "pg_recall": pg_recall,
        "pg_hit": pg_hit,
    }


def print_deviations(label, report):
    deviations = report.get("deviations", [])
    if not deviations:
        print(f"  {label}: (none)")
        return
    for d in deviations:
        case = d.get("case", "*")
        text = d.get("text", "")
        print(f"  {label}: [{case}] {text}")


def sha256_of(path):
    """The SHA-256 of a file, streamed, or None when it cannot be read."""
    try:
        digest = hashlib.sha256()
        with open(path, "rb") as handle:
            for block in iter(lambda: handle.read(1 << 20), b""):
                digest.update(block)
        return digest.hexdigest()
    except OSError:
        return None


def read_frozen_manifest(frozen_dir):
    """The frozen references' manifest, or (None, message) saying why not."""
    path = os.path.join(frozen_dir, FROZEN_MANIFEST)
    if not os.path.isfile(path):
        return None, (
            f"no frozen references at {frozen_dir}: {FROZEN_MANIFEST} is missing. "
            "Name a postgres and a sqlite report on the command line, or produce the "
            "frozen directory first (see docs/core/V2_BENCHMARK_PROTOCOL.md, "
            '"The 50K battle").'
        )
    try:
        with open(path, "r", encoding="utf-8") as handle:
            return json.load(handle), None
    except (OSError, ValueError) as error:
        return None, f"{path} cannot be read as JSON: {error}"


def check_frozen_provenance(manifest, frozen_dir, live_reports, used):
    """REFUSE rather than compare when the frozen references do not describe
    the corpus in front of us.

    Two independent checks, both fatal:

      * IDENTITY. The corpus every live report names is hashed and must equal
        the manifest's `corpus_sha256`. A frozen Postgres median measured on
        a different 50,000 rows is not a reference, it is a wrong answer.
      * AGE. No report -- live or frozen -- may be older than the corpus file
        itself. A report that predates the corpus was produced against
        something else, whatever its hash says today.

    Returns a list of refusal messages; empty means the references stand.
    """
    problems = []
    expected = manifest.get("corpus_sha256")
    corpus_path = manifest.get("corpus")
    for label, path, report in live_reports:
        named = report.get("data")
        if not named:
            continue
        corpus_path = named
        actual = sha256_of(named)
        if actual is None:
            problems.append(
                f"the {label} report names the corpus {named}, which cannot be read, so the "
                f"frozen references in {frozen_dir} cannot be shown to describe it"
            )
            continue
        if expected and actual != expected:
            problems.append(
                f"corpus mismatch: {named} hashes to {actual}, the frozen references in "
                f"{frozen_dir} were measured on {expected}. Rerun the postgres and sqlite "
                "arms on this corpus and refreeze, or name their reports on the command line."
            )
        break

    if corpus_path and os.path.isfile(corpus_path):
        corpus_mtime = os.path.getmtime(corpus_path)
        checked = list(live_reports) + [
            (label, os.path.join(frozen_dir, FROZEN_FILES[arm]), report)
            for arm, (label, report) in used.items()
        ]
        for label, path, _ in checked:
            if not path or not os.path.isfile(path):
                continue
            if os.path.getmtime(path) < corpus_mtime:
                problems.append(
                    f"the {label} report {path} is OLDER than the corpus {corpus_path} "
                    "it claims to measure; rerun that arm."
                )
    return problems


def route(paths):
    """Each report into the arm slot its own `arm` field names. Order on the
    command line is not meaning: a report says what it is."""
    slots = {}
    where = {}
    for path in paths:
        report = load_report(path)
        arm = report.get("arm")
        if arm not in ARM_ORDER:
            raise SystemExit(
                f"{path}: arm field is {arm!r}; expected one of {', '.join(ARM_ORDER)}"
            )
        if arm in slots:
            raise SystemExit(f"two reports name the arm {arm!r}: {where[arm]} and {path}")
        slots[arm] = report
        where[arm] = path
    return slots, where


def main():
    parser = argparse.ArgumentParser(
        description="Compare the battle50k arms on one corpus"
    )
    parser.add_argument(
        "reports",
        nargs="+",
        help="one to four battle50k JSON reports, in any order; each is routed "
        "by its own `arm` field. The e4 report is required.",
    )
    parser.add_argument(
        "--frozen",
        default=FROZEN_DIR,
        help="directory holding the frozen postgres and sqlite references "
        f"(default: {FROZEN_DIR})",
    )
    parser.add_argument(
        "--md", action="store_true", help="print GitHub markdown tables"
    )
    args = parser.parse_args()

    if len(args.reports) > len(ARM_ORDER):
        raise SystemExit(f"at most {len(ARM_ORDER)} reports, one per arm")
    slots, where = route(args.reports)
    if "e4" not in slots:
        raise SystemExit(
            "no `e4` report among the arguments: it is the arm every ratio is measured against"
        )

    # Postgres and SQLite are CONSTANTS of this battery. An invocation that
    # does not name them reads them from the frozen directory rather than
    # rerunning two engines that did not change -- and only after their
    # provenance is checked against the corpus in front of us.
    missing = [arm for arm in ("postgres", "sqlite") if arm not in slots]
    used_frozen = {}
    if missing:
        manifest, problem = read_frozen_manifest(args.frozen)
        if manifest is None:
            raise SystemExit(problem)
        for arm in missing:
            path = os.path.join(args.frozen, FROZEN_FILES[arm])
            if not os.path.isfile(path):
                raise SystemExit(
                    f"the frozen references at {args.frozen} hold no {FROZEN_FILES[arm]}, "
                    f"so the {arm} arm has nothing to compare against"
                )
            report = load_report(path)
            if report.get("arm") != arm:
                raise SystemExit(
                    f"{path}: arm field is {report.get('arm')!r}, expected {arm!r}"
                )
            slots[arm] = report
            where[arm] = path
            used_frozen[arm] = (ARM_LABEL[arm], report)
        live = [
            (ARM_LABEL[arm], where[arm], slots[arm])
            for arm in ARM_ORDER
            if arm in slots and arm not in used_frozen
        ]
        problems = check_frozen_provenance(manifest, args.frozen, live, used_frozen)
        if problems:
            for text in problems:
                print(f"REFUSED: {text}", file=sys.stderr)
            return 2
        print(
            "frozen references: "
            + " ".join(f"{arm}={where[arm]}" for arm in sorted(used_frozen))
            + f" (frozen {manifest.get('date', 'date unknown')}, corpus "
            + f"{(manifest.get('corpus_sha256') or 'unknown')[:16]}…)"
        )

    if "postgres" not in slots:
        raise SystemExit("no `postgres` report and no frozen reference for it")

    reports = [(ARM_LABEL[arm], slots[arm]) for arm in ARM_ORDER if arm in slots]
    present = [arm for arm in ARM_ORDER if arm in slots]
    sql_at = present.index("e4-sql") if "e4-sql" in present else None
    lite_at = present.index("sqlite") if "sqlite" in present else None

    print("rows: " + " ".join(f"{label}={r.get('rows')}" for label, r in reports))
    print("commit: " + " ".join(f"{label}={r.get('commit')}" for label, r in reports))
    print()

    print("STAGES")
    s_headers, s_rows = build_stages_table(reports)
    print_table(s_headers, s_rows, args.md)
    print()

    print("CASES")
    c_headers, c_rows, stats = build_cases_table(reports, sql_at, lite_at)
    print_table(c_headers, c_rows, args.md)
    print()

    for base in APPROX_BASES:
        e4_points, pg_points = print_sweep_table(base, reports, args.md)
        sweep_headline(base, e4_points, pg_points)

    print("DEVIATIONS")
    for label, report in reports:
        print_deviations(label, report)
    print()

    print_anomalies(stats, args.md)

    filter_total = stats["filter_total"]
    filter_agree = stats["filter_agree"]
    timed_total = stats["timed_total"]
    e4_faster = stats["e4_faster"]

    print(
        f"VERDICT: filter cases agreeing across {len(reports)} arm(s) "
        f"{filter_agree}/{filter_total}; "
        f"E4 faster than Postgres in {e4_faster}/{timed_total} cases with both medians "
        f"present ({UNIT_US}); {len(stats['slower'])} case/engine pairs where E4 is slower."
    )

    if stats["disagreements"]:
        print(
            "DISAGREEMENTS: " + ", ".join(name for name, _ in stats["disagreements"]),
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
