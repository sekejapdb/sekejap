"""A shell over one sekejap directory: ``python -m sekejap``.

Usage::

    python -m sekejap ./data                     # a prompt on ./data
    python -m sekejap ./data "SELECT ..."        # run one statement and exit
    python -m sekejap --service ./data           # open in service mode

sekejap is disk-first, so a directory is required: there is no in-memory
database to fall back to, and this shell does not invent a temporary one.

Inside the prompt, a line that begins with a backslash is a command and
anything else is SQL. A statement that returns rows is printed as a table; one
that writes prints the rows it moved.
"""

from __future__ import annotations

import argparse
import json
import sys
import time

from . import Db, SekejapError, __version__, format_version, version

_MAX_WIDTH = 52


def _cell(value):
    if value is None:
        return ""
    if isinstance(value, str):
        return value
    return json.dumps(value, ensure_ascii=False)


def _clip(text, width):
    return text if len(text) <= width else text[: width - 1] + "…"


def _duration(seconds):
    if seconds < 1e-3:
        return "%.0f us" % (seconds * 1e6)
    if seconds < 1.0:
        return "%.2f ms" % (seconds * 1e3)
    return "%.3f s" % seconds


def print_table(rows, elapsed):
    """Rows as the C ABI hands them over: objects keyed by column name."""
    if not rows:
        print("(0 rows)  [%s]" % _duration(elapsed))
        return
    columns = []
    for row in rows:
        for column in row:
            if column not in columns:
                columns.append(column)
    # A column MISSING in a row is omitted from that row, because missing is
    # not null; an empty cell here is exactly that.
    cells = [[_clip(_cell(row.get(column)), _MAX_WIDTH) for column in columns] for row in rows]
    widths = [
        max(len(_clip(column, _MAX_WIDTH)), *(len(row[index]) for row in cells))
        for index, column in enumerate(columns)
    ]
    line = "-+-".join("-" * width for width in widths)
    print(" | ".join(_clip(c, _MAX_WIDTH).ljust(w) for c, w in zip(columns, widths)))
    print(line)
    for row in cells:
        print(" | ".join(cell.ljust(width) for cell, width in zip(row, widths)))
    print("(%d row%s)  [%s]" % (len(rows), "" if len(rows) == 1 else "s", _duration(elapsed)))


_HELP = """\
\\h, \\?            this help
\\d                 the collections in the catalog
\\d <collection>    the declared shape of one collection, and its rows
\\n <collection> <key> [type]   the rows one hop away
\\e <statement>     the plan the engine would build
\\s                 the bytes on disk
\\q                 quit
anything else      SQL, run with no parameters"""


def run_statement(db, text):
    """One statement: rows if it returns them, a count if it writes."""
    started = time.perf_counter()
    try:
        rows = db.query(text)
    except SekejapError as failure:
        if failure.code.name not in ("INVALID", "UNSUPPORTED"):
            raise
        # A writing statement is not a row-returning one; try it as a write
        # before reporting the first failure, which may have been about that.
        try:
            moved = db.execute(text)
        except SekejapError:
            raise failure
        print("%d row(s)  [%s]" % (moved, _duration(time.perf_counter() - started)))
        return
    print_table(rows, time.perf_counter() - started)


def run_command(db, line):
    """A backslash command. Returns False when the shell should stop."""
    parts = line.split()
    command, arguments = parts[0], parts[1:]
    if command in ("\\q", "\\quit"):
        return False
    elif command in ("\\h", "\\?", "\\help"):
        print(_HELP)
    elif command == "\\d":
        if arguments:
            shape = db.describe(arguments[0])
            if shape is None:
                print("no collection named %r" % arguments[0])
            else:
                print(json.dumps(shape, indent=2, sort_keys=True))
        else:
            for name in db.collections():
                print("%-32s %d rows" % (name, db.count_rows(name)))
    elif command == "\\n":
        if len(arguments) < 2:
            print("usage: \\n <collection> <key> [edge type]")
        else:
            edge_type = arguments[2] if len(arguments) > 2 else None
            for row in db.neighbours(arguments[0], arguments[1], edge_type):
                print("%s/%s %s" % (row["collection"], row["key"], json.dumps(row["document"])))
    elif command == "\\e":
        print(db.explain(line[len(command):].strip()))
    elif command == "\\s":
        print(json.dumps(db.storage(), indent=2, sort_keys=True))
    else:
        print("unknown command %r -- \\h for the list" % command)
    return True


def repl(db):
    print(
        "sekejap %s (wrapper %s), disk format %d, on %s"
        % (version(), __version__, format_version(), db.path)
    )
    print("\\h for help, \\q to quit")
    while True:
        try:
            line = input("sekejap> ").strip()
        except (EOFError, KeyboardInterrupt):
            print()
            return 0
        if not line:
            continue
        try:
            if line.startswith("\\"):
                if not run_command(db, line):
                    return 0
            else:
                run_statement(db, line)
        except SekejapError as failure:
            print("%s: %s" % (failure.code.name.lower(), failure.message))


def main(argv=None):
    parser = argparse.ArgumentParser(prog="python -m sekejap", description=__doc__)
    parser.add_argument("directory", help="the sekejap directory to open")
    parser.add_argument("statement", nargs="?", help="run one statement and exit")
    parser.add_argument(
        "--service",
        action="store_true",
        help="open in service mode: one writer, parallel readers, the change feed",
    )
    arguments = parser.parse_args(argv)

    try:
        db = Db(arguments.directory, service=arguments.service)
    except SekejapError as failure:
        print("%s: %s" % (failure.code.name.lower(), failure.message), file=sys.stderr)
        return 1
    try:
        if arguments.statement:
            try:
                run_statement(db, arguments.statement)
            except SekejapError as failure:
                print("%s: %s" % (failure.code.name.lower(), failure.message), file=sys.stderr)
                return 1
            return 0
        return repl(db)
    finally:
        db.close()


if __name__ == "__main__":
    sys.exit(main())
