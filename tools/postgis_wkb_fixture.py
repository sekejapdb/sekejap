#!/usr/bin/env python3
"""Generate core/engine/tests/fixtures/postgis_wkb.json from a live PostGIS.

The oracle for the geometry I/O functions (`docs/lang/QL_CONTRACT.md` §4.4):
for every case PostGIS is asked, in one batched SELECT, what it prints for
ST_AsBinary (both byte orders), ST_AsEWKB (both byte orders, SRID 4326),
ST_AsText, ST_AsGeoJSON and, for points, ST_X / ST_Y. A second family asks
`a && b` over pairs whose boxes touch, nearly touch and overlap, because
PostGIS answers `&&` on float4 boxes rounded outward and a double-precision
test would disagree with it at the edges.

Deterministic (seed 20260925). The PostGIS server is reached with
`docker exec <container> psql`; set PGCONTAINER, PGUSER and PGDATABASE.
The test that reads the fixture is offline and never talks to Postgres.
"""
from __future__ import annotations

import json
import os
import random
import subprocess
import sys
from pathlib import Path

SEED = 20260925
ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "core" / "engine" / "tests" / "fixtures" / "postgis_wkb.json"

PGCONTAINER = os.environ.get("PGCONTAINER", "postgis")
PGUSER = os.environ.get("PGUSER", "postgres")
PGDATABASE = os.environ.get("PGDATABASE", "postgres")


def psql(sql: str) -> str:
    r = subprocess.run(
        ["docker", "exec", "-i", PGCONTAINER, "psql", "-U", PGUSER, "-d", PGDATABASE,
         "-At", "-v", "ON_ERROR_STOP=1"],
        input=sql, capture_output=True, text=True,
    )
    if r.returncode != 0:
        raise RuntimeError(f"psql failed (code {r.returncode}):\n{r.stderr}\n{sql[:800]}")
    return "".join(r.stdout.splitlines())


def sql_str(text: str) -> str:
    return "'" + text.replace("'", "''") + "'"


def num(rng: random.Random, lo: float, hi: float) -> str:
    """A coordinate with many significant digits, so number printing is tested."""
    return repr(rng.uniform(lo, hi))


def ring(rng: random.Random, cx: float, cy: float, r: float, n: int) -> str:
    pts = []
    for i in range(n):
        x = cx + r * (0.5 + rng.random()) * (1 if i % 2 else -1)
        y = cy + r * (0.5 + rng.random()) * (1 if i < n // 2 else -1)
        pts.append(f"{x!r} {y!r}")
    # A convex-enough order is not needed: WKB and WKT carry any ring.
    pts.append(pts[0])
    return "(" + ",".join(pts) + ")"


def written_cases() -> list[str]:
    return [
        "POINT(1 2)",
        "POINT(-73.5 40.25)",
        "POINT(0.30000000000000004 -0)",
        "POINT(179.999999 -89.5)",
        "POINT(-180 90)",
        "POINT(12.123456789012345 -45.000000000000001)",
        "LINESTRING(0 0,1 1)",
        "LINESTRING(-10.5 20.25,30 -40,50.125 60)",
        "POLYGON((0 0,10 0,10 10,0 10,0 0))",
        "POLYGON((0 0,10 0,10 10,0 10,0 0),(2 2,4 2,4 4,2 4,2 2))",
        "MULTIPOINT(1 2,3 4)",
        "MULTIPOINT(-1.5 -2.5)",
        "MULTILINESTRING((0 0,1 1),(2 2,3 3,4 5))",
        "MULTIPOLYGON(((0 0,1 0,1 1,0 1,0 0)),((5 5,6 5,6 6,5 6,5 5),(5.2 5.2,5.4 5.2,5.4 5.4,5.2 5.2)))",
    ]


def random_cases(rng: random.Random) -> list[str]:
    out = []
    for _ in range(6):
        out.append(f"POINT({num(rng, -180, 180)} {num(rng, -90, 90)})")
    for _ in range(4):
        pts = ",".join(f"{num(rng, -170, 170)} {num(rng, -80, 80)}" for _ in range(rng.randint(2, 6)))
        out.append(f"LINESTRING({pts})")
    for _ in range(4):
        cx, cy = rng.uniform(-150, 150), rng.uniform(-60, 60)
        out.append("POLYGON(" + ring(rng, cx, cy, 5.0, rng.randint(3, 7)) + ")")
    for _ in range(3):
        pts = ",".join(f"{num(rng, -170, 170)} {num(rng, -80, 80)}" for _ in range(rng.randint(1, 4)))
        out.append(f"MULTIPOINT({pts})")
    for _ in range(3):
        parts = []
        for _ in range(rng.randint(1, 3)):
            pts = ",".join(f"{num(rng, -170, 170)} {num(rng, -80, 80)}" for _ in range(rng.randint(2, 4)))
            parts.append(f"({pts})")
        out.append("MULTILINESTRING(" + ",".join(parts) + ")")
    for _ in range(3):
        polys = []
        for _ in range(rng.randint(1, 3)):
            cx, cy = rng.uniform(-150, 150), rng.uniform(-60, 60)
            polys.append("(" + ring(rng, cx, cy, 3.0, rng.randint(3, 6)) + ")")
        out.append("MULTIPOLYGON(" + ",".join(polys) + ")")
    return out


def overlap_cases(rng: random.Random) -> list[tuple[str, str]]:
    box = "POLYGON((0 0,1 0,1 1,0 1,0 0))"
    pairs = [
        (box, "POINT(0.5 0.5)"),
        (box, "POINT(1 1)"),                    # touches a corner
        (box, "POINT(1.0000000001 0.5)"),       # outside in double, inside in float4
        (box, "POINT(1.001 0.5)"),              # outside in both
        (box, "POLYGON((1 0,2 0,2 1,1 1,1 0))"),  # shares an edge
        (box, "LINESTRING(-1 -1,-0.5 -0.5)"),
        (box, "LINESTRING(-1 2,2 -1)"),         # box overlaps, geometry crosses
        (box, "LINESTRING(1.5 1.5,3 3)"),
        ("LINESTRING(170 0,-170 1)", "POINT(0 0.5)"),  # planar box spans the map
        ("POINT(-180 -90)", "POLYGON((-180 -90,-179 -90,-179 -89,-180 -89,-180 -90))"),
    ]
    for _ in range(40):
        x0, y0 = rng.uniform(-10, 10), rng.uniform(-10, 10)
        a = f"POLYGON(({x0!r} {y0!r},{x0 + 2!r} {y0!r},{x0 + 2!r} {y0 + 2!r},{x0!r} {y0 + 2!r},{x0!r} {y0!r}))"
        b = f"POINT({rng.uniform(-12, 12)!r} {rng.uniform(-12, 12)!r})"
        pairs.append((a, b))
    return pairs


def main() -> int:
    rng = random.Random(SEED)
    cases = written_cases() + random_cases(rng)
    values = ",".join(f"({i},{sql_str(w)})" for i, w in enumerate(cases))
    rows = json.loads(psql(f"""
        SELECT json_agg(json_build_object(
            'wkt', w,
            'wkb_ndr', encode(ST_AsBinary(g, 'NDR'), 'hex'),
            'wkb_xdr', encode(ST_AsBinary(g, 'XDR'), 'hex'),
            'ewkb_ndr', encode(ST_AsEWKB(ST_SetSRID(g, 4326), 'NDR'), 'hex'),
            'ewkb_xdr', encode(ST_AsEWKB(ST_SetSRID(g, 4326), 'XDR'), 'hex'),
            'text', ST_AsText(g),
            'geojson', ST_AsGeoJSON(g),
            'x', CASE WHEN GeometryType(g) = 'POINT' THEN ST_X(g) END,
            'y', CASE WHEN GeometryType(g) = 'POINT' THEN ST_Y(g) END
        ) ORDER BY i)
        FROM (SELECT i, w, ST_GeomFromText(w) AS g FROM (VALUES {values}) v(i, w)) s;
    """))
    pairs = overlap_cases(rng)
    values = ",".join(f"({i},{sql_str(a)},{sql_str(b)})" for i, (a, b) in enumerate(pairs))
    overlaps = json.loads(psql(f"""
        SELECT json_agg(json_build_object('a', a, 'b', b,
            'overlaps', ST_GeomFromText(a, 4326) && ST_GeomFromText(b, 4326)) ORDER BY i)
        FROM (VALUES {values}) v(i, a, b);
    """))
    version = psql("SELECT postgis_lib_version();")
    OUT.write_text(json.dumps({
        "generator": "tools/postgis_wkb_fixture.py",
        "seed": SEED,
        "postgis": version,
        "cases": rows,
        "overlaps": overlaps,
    }, indent=1) + "\n")
    print(f"wrote {OUT.relative_to(ROOT)}: {len(rows)} cases, {len(overlaps)} overlap pairs")
    return 0


if __name__ == "__main__":
    sys.exit(main())
