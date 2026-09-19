#!/usr/bin/env python3
"""Generate tests/fixtures/postgis_conformance.json from a live PostGIS.

Deterministic (seed 20260919). One batched SELECT per family, using
ST_GeomFromGeoJSON(...)::geography for spheroidal predicates and
::geometry for planar ones, matching src/query.rs GeometryFilter and
src/spatial_geometry.rs.
"""
from __future__ import annotations

import json
import math
import os
import random
import subprocess
import sys
from pathlib import Path

SEED = 20260919
ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "tests" / "fixtures" / "postgis_conformance.json"

M_PER_DEG_LAT = 110540.0  # metres per degree of latitude (WGS84-ish mid)


def m_per_deg_lon(lat: float) -> float:
    c = math.cos(math.radians(lat))
    return max(M_PER_DEG_LAT * abs(c), 1.0)


def dlon_m(metres: float, lat: float) -> float:
    return metres / m_per_deg_lon(lat)


def dlat_m(metres: float) -> float:
    return metres / M_PER_DEG_LAT


def wrap_lon(lon: float) -> float:
    while lon > 180.0:
        lon -= 360.0
    while lon < -180.0:
        lon += 360.0
    return lon


def clamp_lat(lat: float) -> float:
    return max(-90.0, min(90.0, lat))


def dumps(obj) -> str:
    return json.dumps(obj, separators=(",", ":"), ensure_ascii=True)


def point(lon: float, lat: float) -> dict:
    return {"type": "Point", "coordinates": [wrap_lon(lon), clamp_lat(lat)]}


def linestring(coords: list) -> dict:
    return {
        "type": "LineString",
        "coordinates": [[wrap_lon(x), clamp_lat(y)] for x, y in coords],
    }


def close_ring(ring: list) -> list:
    if not ring:
        return ring
    if ring[0] != ring[-1]:
        return ring + [ring[0]]
    return ring


def polygon(rings: list, close: bool = True) -> dict:
    out = []
    for ring in rings:
        r = [[float(p[0]), float(p[1])] for p in ring]
        out.append(close_ring(r) if close else r)
    return {"type": "Polygon", "coordinates": out}


def multipoint(coords: list) -> dict:
    return {
        "type": "MultiPoint",
        "coordinates": [[wrap_lon(x), clamp_lat(y)] for x, y in coords],
    }


def multipolygon(parts: list, close: bool = True) -> dict:
    out = []
    for rings in parts:
        pr = []
        for ring in rings:
            r = [[float(p[0]), float(p[1])] for p in ring]
            pr.append(close_ring(r) if close else r)
        out.append(pr)
    return {"type": "MultiPolygon", "coordinates": out}


def square(lon: float, lat: float, half_m: float) -> list:
    dx, dy = dlon_m(half_m, lat), dlat_m(half_m)
    return close_ring(
        [
            [lon - dx, lat - dy],
            [lon + dx, lat - dy],
            [lon + dx, lat + dy],
            [lon - dx, lat + dy],
        ]
    )


def irregular_ngon(lon: float, lat: float, radius_m: float, rng: random.Random, n: int | None = None) -> list:
    n = n or rng.randint(4, 8)
    pts = []
    for i in range(n):
        ang = 2.0 * math.pi * i / n + rng.uniform(-0.15, 0.15) * (2.0 * math.pi / n)
        r = radius_m * rng.uniform(0.75, 1.25)
        pts.append(
            [
                wrap_lon(lon + dlon_m(r * math.cos(ang), lat)),
                clamp_lat(lat + dlat_m(r * math.sin(ang))),
            ]
        )
    return close_ring(pts)


def rect(west: float, south: float, east: float, north: float) -> list:
    return close_ring(
        [
            [west, south],
            [east, south],
            [east, north],
            [west, north],
        ]
    )


def dollar(obj) -> str:
    s = dumps(obj)
    tag = "e4g"
    while f"${tag}$" in s:
        tag += "x"
    return f"${tag}${s}${tag}$"


def psql(sql: str) -> str:
    """One docker-exec psql call. SQL is a single SELECT (stdin, equivalent to -c)."""
    r = subprocess.run(
        [
            "docker",
            "exec",
            "-i",
            "postgis",
            "psql",
            "-U",
            "postgres",
            "-d",
            "e4_bench",
            "-At",
            "-v",
            "ON_ERROR_STOP=1",
        ],
        input=sql,
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        raise RuntimeError(
            f"psql failed (code {r.returncode}):\nSTDERR:\n{r.stderr}\nSTDOUT:\n{r.stdout[:2000]}\nSQL head:\n{sql[:800]}"
        )
    # json_agg can wrap; join lines.
    return "".join(r.stdout.splitlines())


def psql_c(sql: str) -> str:
    r = subprocess.run(
        [
            "docker",
            "exec",
            "postgis",
            "psql",
            "-U",
            "postgres",
            "-d",
            "e4_bench",
            "-At",
            "-c",
            sql,
        ],
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        raise RuntimeError(f"psql -c failed: {r.stderr}")
    return r.stdout.strip()


def batch_pairs(pairs: list[dict], extra_select: str) -> tuple[str, list]:
    """pairs have i, a, b (geojson dicts) and optional extra SQL columns in pair['_sql_extra']."""
    values = []
    for p in pairs:
        extra = p.get("_sql_extra", "")
        row = f"({int(p['i'])}, {dollar(p['a'])}, {dollar(p['b'])}{extra})"
        values.append(row)
    cols = "i, a, b" + (pairs[0].get("_sql_cols", "") if pairs else "")
    sql = (
        "SELECT json_agg(q ORDER BY i) FROM (\n"
        "  SELECT v.i::int AS i,\n"
        f"{extra_select}\n"
        f"  FROM (VALUES\n    "
        + ",\n    ".join(values)
        + f"\n  ) AS v({cols})\n) q;\n"
    )
    raw = psql(sql)
    rows = json.loads(raw)
    if len(rows) != len(pairs):
        raise RuntimeError(f"expected {len(pairs)} rows, got {len(rows)}")
    by_i = {int(r["i"]): r for r in rows}
    out = []
    for p in pairs:
        rec = {k: v for k, v in p.items() if not k.startswith("_")}
        rec["postgis"] = {k: v for k, v in by_i[p["i"]].items() if k != "i"}
        out.append(rec)
    return sql, out


def batch_singles(items: list[dict], extra_select: str) -> tuple[str, list]:
    values = []
    for p in items:
        values.append(f"({int(p['i'])}, {dollar(p['g'])})")
    sql = (
        "SELECT json_agg(q ORDER BY i) FROM (\n"
        "  SELECT v.i::int AS i,\n"
        f"{extra_select}\n"
        f"  FROM (VALUES\n    "
        + ",\n    ".join(values)
        + "\n  ) AS v(i, g)\n) q;\n"
    )
    raw = psql(sql)
    rows = json.loads(raw)
    if len(rows) != len(items):
        raise RuntimeError(f"expected {len(items)} rows, got {len(rows)}")
    by_i = {int(r["i"]): r for r in rows}
    out = []
    for p in items:
        rec = {k: v for k, v in p.items() if not k.startswith("_")}
        rec["postgis"] = {k: v for k, v in by_i[p["i"]].items() if k != "i"}
        out.append(rec)
    return sql, out


PAIR_PREDICATES = """    ST_Intersects(ST_GeomFromGeoJSON(v.a)::geography, ST_GeomFromGeoJSON(v.b)::geography) AS intersects,
    ST_Within(ST_GeomFromGeoJSON(v.a)::geometry, ST_GeomFromGeoJSON(v.b)::geometry) AS within,
    ST_Contains(ST_GeomFromGeoJSON(v.a)::geometry, ST_GeomFromGeoJSON(v.b)::geometry) AS contains,
    ST_Covers(ST_GeomFromGeoJSON(v.a)::geometry, ST_GeomFromGeoJSON(v.b)::geometry) AS covers,
    ST_DWithin(ST_GeomFromGeoJSON(v.a)::geography, ST_GeomFromGeoJSON(v.b)::geography, 3.0) AS dwithin_3m,
    ST_DWithin(ST_GeomFromGeoJSON(v.a)::geography, ST_GeomFromGeoJSON(v.b)::geography, 1000.0) AS dwithin_1km,
    ST_Distance(ST_GeomFromGeoJSON(v.a)::geography, ST_GeomFromGeoJSON(v.b)::geography) AS distance_m,
    ST_Relate(ST_GeomFromGeoJSON(v.a)::geometry, ST_GeomFromGeoJSON(v.b)::geometry) AS relate,
    ST_IsValid(ST_GeomFromGeoJSON(v.a)::geometry) AS a_valid,
    ST_IsValid(ST_GeomFromGeoJSON(v.b)::geometry) AS b_valid"""


MEASURES = """    ST_Area(ST_GeomFromGeoJSON(v.g)::geography) AS area_m2,
    ST_Length(ST_GeomFromGeoJSON(v.g)::geography) AS length_m,
    ST_Perimeter(ST_GeomFromGeoJSON(v.g)::geography) AS perimeter_m,
    ST_X(ST_Centroid(ST_GeomFromGeoJSON(v.g)::geography)::geometry) AS centroid_lon,
    ST_Y(ST_Centroid(ST_GeomFromGeoJSON(v.g)::geography)::geometry) AS centroid_lat,
    ST_IsValid(ST_GeomFromGeoJSON(v.g)::geometry) AS is_valid"""


def pick_center(rng: random.Random) -> tuple[float, float]:
    # Cluster around a few inhabited latitudes so 1 m ≈ a stable degree scale.
    regions = [
        (106.8, -6.2),    # Java
        (-74.0, 40.7),    # NYC
        (139.7, 35.7),    # Tokyo
        (2.3, 48.9),      # Paris
        (18.4, -33.9),    # Cape Town
        (-46.6, -23.5),   # São Paulo
        (37.6, 55.7),     # Moscow
        (151.2, -33.9),   # Sydney
    ]
    lon0, lat0 = regions[rng.randrange(len(regions))]
    return wrap_lon(lon0 + rng.uniform(-0.4, 0.4)), clamp_lat(lat0 + rng.uniform(-0.3, 0.3))


def family_a(rng: random.Random) -> list[dict]:
    """300 random polygon pairs near each other, covering the named relations."""
    tags = [
        "apart_0_3m",
        "touching",
        "overlapping",
        "one_inside",
        "shared_edge",
        "shared_vertex",
        "vertex_on_edge",
    ]
    pairs = []
    for i in range(300):
        tag = tags[i % 7]
        lon, lat = pick_center(rng)
        half = rng.uniform(25.0, 120.0)
        if tag == "apart_0_3m":
            gap = rng.uniform(0.05, 3.0)
            a = polygon([square(lon, lat, half)])
            shift = 2.0 * half + gap
            b = polygon([square(lon + dlon_m(shift, lat), lat, half)])
        elif tag == "touching":
            # Interiors disjoint, boundaries kiss at one vertex. Copy the NE
            # corner literally so the shared vertex is bit-identical (recomputing
            # square() at a shifted centre drifts by millimetres because
            # metres-to-longitude depends on latitude).
            a_ring = square(lon, lat, half)
            ne = a_ring[2]
            dx, dy = dlon_m(half, lat), dlat_m(half)
            b_ring = close_ring(
                [
                    ne,
                    [ne[0] + 2.0 * dx, ne[1]],
                    [ne[0] + 2.0 * dx, ne[1] + 2.0 * dy],
                    [ne[0], ne[1] + 2.0 * dy],
                ]
            )
            a = polygon([a_ring])
            b = polygon([b_ring])
        elif tag == "overlapping":
            a = polygon([irregular_ngon(lon, lat, half, rng)])
            b = polygon(
                [irregular_ngon(lon + dlon_m(half * 0.6, lat), lat + dlat_m(half * 0.3), half, rng)]
            )
        elif tag == "one_inside":
            a = polygon([square(lon, lat, half * 4.0)])
            b = polygon([irregular_ngon(lon, lat, half * 0.4, rng, n=5)])
        elif tag == "shared_edge":
            a_ring = square(lon, lat, half)
            # East neighbour sharing the east edge exactly.
            dx, dy = dlon_m(half, lat), dlat_m(half)
            east = [lon + dx, lat - dy]
            east_n = [lon + dx, lat + dy]
            b_ring = close_ring(
                [
                    east,
                    [lon + 3.0 * dx, lat - dy],
                    [lon + 3.0 * dx, lat + dy],
                    east_n,
                ]
            )
            a = polygon([a_ring])
            b = polygon([b_ring])
        elif tag == "shared_vertex":
            a_ring = square(lon, lat, half)
            ne = a_ring[2]
            dx, dy = dlon_m(half, lat), dlat_m(half)
            b_ring = close_ring(
                [
                    ne,
                    [ne[0] + 2.0 * dx, ne[1]],
                    [ne[0] + 2.0 * dx, ne[1] + 2.0 * dy],
                    [ne[0], ne[1] + 2.0 * dy],
                ]
            )
            a = polygon([a_ring])
            b = polygon([b_ring])
        else:  # vertex_on_edge
            a_ring = square(lon, lat, half)
            # Midpoint of the north edge of A.
            dx, dy = dlon_m(half, lat), dlat_m(half)
            mid = [lon, lat + dy]
            b_ring = close_ring(
                [
                    mid,
                    [mid[0] + dx, mid[1] + 2.0 * dy],
                    [mid[0] - dx, mid[1] + 2.0 * dy],
                ]
            )
            a = polygon([a_ring])
            b = polygon([b_ring])
        pairs.append({"i": i, "tag": tag, "a": a, "b": b})
    return pairs


def family_b(rng: random.Random) -> list[dict]:
    """100 polygons with holes vs points in hole / on hole boundary / in shell."""
    tags = ["in_hole", "on_hole_boundary", "in_shell", "on_outer_boundary", "on_hole_vertex"]
    pairs = []
    for i in range(100):
        tag = tags[i % 5]
        lon, lat = pick_center(rng)
        outer_h = rng.uniform(400.0, 1200.0)
        hole_h = outer_h * rng.uniform(0.15, 0.35)
        outer = square(lon, lat, outer_h)
        hole = square(lon, lat, hole_h)
        # Hole winding opposite the outer (CW if outer is CCW).
        hole_cw = [hole[0], hole[3], hole[2], hole[1], hole[0]]
        poly = polygon([outer, hole_cw])
        dx_h, dy_h = dlon_m(hole_h, lat), dlat_m(hole_h)
        dx_o, dy_o = dlon_m(outer_h, lat), dlat_m(outer_h)
        if tag == "in_hole":
            pt = point(lon, lat)
        elif tag == "on_hole_boundary":
            pt = point(lon + dx_h, lat)  # east edge midpoint of hole
        elif tag == "in_shell":
            # Halfway between hole and outer on the east.
            pt = point(lon + (dx_h + dx_o) * 0.5, lat)
        elif tag == "on_outer_boundary":
            pt = point(lon, lat + dy_o)  # north edge midpoint
        else:
            pt = point(lon + dx_h, lat + dy_h)  # NE hole vertex
        # Query point vs polygon so Within(point, poly) is the natural question.
        pairs.append({"i": i, "tag": tag, "a": pt, "b": poly})
    return pairs


def family_c(rng: random.Random) -> list[dict]:
    """100 antimeridian-crossing pairs + 50 within 1 degree of a pole."""
    pairs = []
    i = 0
    for k in range(100):
        lat = rng.uniform(-40.0, 40.0)
        half_m = rng.uniform(500.0, 50000.0)
        dy = dlat_m(half_m)
        # Span across 179.9 .. -179.9.
        span = rng.uniform(0.2, 2.5)
        west = 180.0 - span / 2.0
        east = -180.0 + span / 2.0
        a = polygon([rect(west, lat - dy, east, lat + dy)])
        kind = k % 5
        if kind == 0:
            b = point(179.95, lat)
            tag = "antimeridian_point_east"
        elif kind == 1:
            b = point(-179.95, lat)
            tag = "antimeridian_point_west"
        elif kind == 2:
            b = point(0.0, lat)
            tag = "antimeridian_point_elsewhere"
        elif kind == 3:
            b = polygon([square(179.7, lat, half_m * 0.3)])
            tag = "antimeridian_poly_east"
        else:
            b = polygon([square(-179.7, lat, half_m * 0.3)])
            tag = "antimeridian_poly_west"
        pairs.append({"i": i, "tag": tag, "a": a, "b": b})
        i += 1
    for k in range(50):
        north = k % 2 == 0
        pole_lat = 89.5 if north else -89.5
        lat = pole_lat + rng.uniform(-0.45, 0.45)
        lat = clamp_lat(lat)
        # Stay within 1 degree of a pole.
        if north:
            lat = max(89.0, min(89.95, lat))
        else:
            lat = min(-89.0, max(-89.95, lat))
        lon = rng.uniform(-180.0, 180.0)
        half_m = rng.uniform(50.0, 2000.0)
        a = polygon([square(lon, lat, half_m)])
        kind = k % 4
        if kind == 0:
            b = point(lon, lat)
            tag = "pole_point_inside"
        elif kind == 1:
            b = point(wrap_lon(lon + 20.0), lat)
            tag = "pole_point_other_lon"
        elif kind == 2:
            b = polygon([square(wrap_lon(lon + dlon_m(half_m * 1.2, lat)), lat, half_m)])
            tag = "pole_poly_near"
        else:
            b = point(lon, 90.0 if north else -90.0)
            tag = "pole_point_at_pole"
        pairs.append({"i": i, "tag": tag, "a": a, "b": b})
        i += 1
    return pairs


def family_d(rng: random.Random) -> list[dict]:
    """100 degenerate inputs. Second geom is a nearby simple point for predicates."""
    kinds = [
        "collinear_triple",
        "zero_area",
        "repeated_vertices",
        "self_touching_ring",
        "clockwise_ring",
        "counterclockwise_ring",
        "unclosed_ring",
        "cw_outer_ccw_hole",
    ]
    pairs = []
    for i in range(100):
        kind = kinds[i % 8]
        lon, lat = pick_center(rng)
        dx, dy = dlon_m(80.0, lat), dlat_m(80.0)
        pt = point(lon + dx * 0.25, lat + dy * 0.25)
        if kind == "collinear_triple":
            ring = close_ring(
                [
                    [lon - dx, lat],
                    [lon, lat],
                    [lon + dx, lat],
                ]
            )
            g = polygon([ring])
        elif kind == "zero_area":
            # Spike with no area: out and back along one edge, then a duplicate.
            ring = close_ring(
                [
                    [lon, lat],
                    [lon + dx, lat],
                    [lon, lat],
                    [lon, lat + dy],
                ]
            )
            g = polygon([ring])
        elif kind == "repeated_vertices":
            ring = close_ring(
                [
                    [lon - dx, lat - dy],
                    [lon - dx, lat - dy],
                    [lon + dx, lat - dy],
                    [lon + dx, lat - dy],
                    [lon + dx, lat + dy],
                    [lon - dx, lat + dy],
                ]
            )
            g = polygon([ring])
        elif kind == "self_touching_ring":
            # Bowtie.
            g = polygon(
                [
                    close_ring(
                        [
                            [lon - dx, lat - dy],
                            [lon + dx, lat + dy],
                            [lon + dx, lat - dy],
                            [lon - dx, lat + dy],
                        ]
                    )
                ]
            )
        elif kind == "clockwise_ring":
            ring = [
                [lon - dx, lat - dy],
                [lon - dx, lat + dy],
                [lon + dx, lat + dy],
                [lon + dx, lat - dy],
                [lon - dx, lat - dy],
            ]
            g = polygon([ring])
        elif kind == "counterclockwise_ring":
            ring = [
                [lon - dx, lat - dy],
                [lon + dx, lat - dy],
                [lon + dx, lat + dy],
                [lon - dx, lat + dy],
                [lon - dx, lat - dy],
            ]
            g = polygon([ring])
        elif kind == "unclosed_ring":
            ring = [
                [lon - dx, lat - dy],
                [lon + dx, lat - dy],
                [lon + dx, lat + dy],
                [lon - dx, lat + dy],
            ]
            g = polygon([ring], close=False)
        else:  # cw_outer_ccw_hole
            outer = [
                [lon - 2 * dx, lat - 2 * dy],
                [lon - 2 * dx, lat + 2 * dy],
                [lon + 2 * dx, lat + 2 * dy],
                [lon + 2 * dx, lat - 2 * dy],
                [lon - 2 * dx, lat - 2 * dy],
            ]
            hole = [
                [lon - dx, lat - dy],
                [lon + dx, lat - dy],
                [lon + dx, lat + dy],
                [lon - dx, lat + dy],
                [lon - dx, lat - dy],
            ]
            g = polygon([outer, hole])
            pt = point(lon, lat)  # inside the hole
        pairs.append({"i": i, "tag": kind, "a": g, "b": pt})
    return pairs


def family_e(rng: random.Random) -> list[dict]:
    """100 dwithin pairs at a designed threshold, queried at threshold ± offsets."""
    designed = [0.01, 1.0, 100.0, 10_000.0, 1_000_000.0]  # 1 cm, 1 m, 100 m, 10 km, 1000 km
    offsets = [0.01, 1.0, 100.0, 10_000.0, 1_000_000.0]
    pairs = []
    for i in range(100):
        T = designed[i % 5]
        lon, lat = pick_center(rng)
        # Keep 1000 km pairs at lower latitudes so the easting conversion stays sane.
        if T >= 100_000.0:
            lat = rng.uniform(-20.0, 20.0)
            lon = rng.uniform(-160.0, 160.0)
        a = point(lon, lat)
        b = point(lon + dlon_m(T, lat), lat)
        radii = []
        for off in offsets:
            radii.append(max(T - off, 0.0))
            radii.append(T + off)
        radii.append(T)
        # Unique, stable order.
        seen = []
        for r in radii:
            if r not in seen:
                seen.append(r)
        pairs.append(
            {
                "i": i,
                "tag": f"designed_{T}m",
                "a": a,
                "b": b,
                "designed_m": T,
                "radii_m": seen,
                "_sql_cols": ", radii",
                "_sql_extra": f", {dollar(seen)}::jsonb",
            }
        )
    return pairs


E_PREDICATES = """    ST_Distance(ST_GeomFromGeoJSON(v.a)::geography, ST_GeomFromGeoJSON(v.b)::geography) AS distance_m,
    (
      SELECT jsonb_agg(jsonb_build_object(
        'radius_m', r::float8,
        'dwithin', ST_DWithin(
          ST_GeomFromGeoJSON(v.a)::geography,
          ST_GeomFromGeoJSON(v.b)::geography,
          r::float8
        )
      ) ORDER BY r)
      FROM jsonb_array_elements_text(v.radii) AS t(r)
    ) AS dwithin"""


def family_f(rng: random.Random) -> list[dict]:
    """100 MultiPolygon / MultiPoint / LineString vs Polygon."""
    tags = [
        "multipolygon_overlap",
        "multipolygon_inside",
        "multipolygon_disjoint",
        "multipoint_inside",
        "multipoint_mixed",
        "multipoint_outside",
        "linestring_crosses",
        "linestring_inside",
        "linestring_disjoint",
        "linestring_along_edge",
    ]
    pairs = []
    for i in range(100):
        tag = tags[i % 10]
        lon, lat = pick_center(rng)
        half = rng.uniform(80.0, 250.0)
        host = polygon([square(lon, lat, half * 2.0)])
        dx, dy = dlon_m(half, lat), dlat_m(half)
        if tag == "multipolygon_overlap":
            a = multipolygon(
                [
                    [square(lon + dx, lat, half)],
                    [square(lon + 6 * dx, lat, half * 0.5)],
                ]
            )
            b = host
        elif tag == "multipolygon_inside":
            a = multipolygon(
                [
                    [square(lon - dx * 0.4, lat, half * 0.3)],
                    [square(lon + dx * 0.4, lat, half * 0.3)],
                ]
            )
            b = host
        elif tag == "multipolygon_disjoint":
            a = multipolygon(
                [
                    [square(lon + dlon_m(half * 8, lat), lat, half)],
                    [square(lon - dlon_m(half * 8, lat), lat, half)],
                ]
            )
            b = host
        elif tag == "multipoint_inside":
            a = multipoint(
                [
                    [lon, lat],
                    [lon + dx * 0.3, lat],
                    [lon, lat + dy * 0.3],
                ]
            )
            b = host
        elif tag == "multipoint_mixed":
            a = multipoint(
                [
                    [lon, lat],
                    [lon + dlon_m(half * 8, lat), lat],
                ]
            )
            b = host
        elif tag == "multipoint_outside":
            a = multipoint(
                [
                    [lon + dlon_m(half * 8, lat), lat],
                    [lon - dlon_m(half * 8, lat), lat],
                ]
            )
            b = host
        elif tag == "linestring_crosses":
            a = linestring(
                [
                    [lon - 3 * dx, lat],
                    [lon + 3 * dx, lat],
                ]
            )
            b = host
        elif tag == "linestring_inside":
            a = linestring(
                [
                    [lon - dx * 0.4, lat - dy * 0.4],
                    [lon + dx * 0.4, lat + dy * 0.4],
                ]
            )
            b = host
        elif tag == "linestring_disjoint":
            a = linestring(
                [
                    [lon + dlon_m(half * 6, lat), lat],
                    [lon + dlon_m(half * 8, lat), lat + dy],
                ]
            )
            b = host
        else:  # linestring_along_edge
            # North edge of the host square (half*2).
            h = half * 2.0
            a = linestring(
                [
                    [lon - dlon_m(h, lat), lat + dlat_m(h)],
                    [lon + dlon_m(h, lat), lat + dlat_m(h)],
                ]
            )
            b = host
        pairs.append({"i": i, "tag": tag, "a": a, "b": b})
    return pairs


def family_g(rng: random.Random) -> list[dict]:
    """50 polygons spanning > 90 degrees + 20 that contain a pole."""
    pairs = []
    i = 0
    for k in range(50):
        lat0 = rng.uniform(-40.0, 40.0)
        lon0 = rng.uniform(-80.0, 80.0)
        span_lon = rng.uniform(91.0, 140.0)
        span_lat = rng.uniform(5.0, 40.0)
        west, east = lon0, lon0 + span_lon
        south, north = clamp_lat(lat0), clamp_lat(lat0 + span_lat)
        a = polygon([rect(west, south, east, north)])
        kind = k % 4
        if kind == 0:
            b = point((west + east) / 2.0, (south + north) / 2.0)
            tag = "large_point_center"
        elif kind == 1:
            b = point(wrap_lon(west - 5.0), (south + north) / 2.0)
            tag = "large_point_outside"
        elif kind == 2:
            b = polygon(
                [square((west + east) / 2.0, (south + north) / 2.0, 50_000.0)]
            )
            tag = "large_poly_inside"
        else:
            b = linestring([[west + 1.0, south + 1.0], [east - 1.0, north - 1.0]])
            tag = "large_line_diagonal"
        pairs.append({"i": i, "tag": tag, "a": a, "b": b, "span_lon_deg": span_lon})
        i += 1
    for k in range(20):
        north = k % 2 == 0
        cap = rng.uniform(70.0, 85.0)
        inside_lon = rng.uniform(-180, 180)
        if north:
            tri = close_ring([[0.0, cap], [120.0, cap], [-120.0, cap]])
            rect_cap = rect(-180.0, cap, 180.0, 90.0)
            pole = point(0.0, 90.0)
            inside = point(inside_lon, (cap + 90.0) / 2.0)
            outside = point(0.0, cap - 5.0)
        else:
            tri = close_ring([[0.0, -cap], [-120.0, -cap], [120.0, -cap]])
            rect_cap = rect(-180.0, -90.0, 180.0, -cap)
            pole = point(0.0, -90.0)
            inside = point(inside_lon, (-cap - 90.0) / 2.0)
            outside = point(0.0, -cap + 5.0)
        # First four keep the ±180 rectangle (PostGIS geography collapses
        # lon ±180 to one meridian). The rest are geodesic triangles that
        # actually contain the pole under ST_Intersects(geography).
        use_rect = k < 4
        a = polygon([rect_cap if use_rect else tri])
        kind = k % 3
        prefix = "pole_rect" if use_rect else "pole_tri"
        if kind == 0:
            b, tag = pole, f"{prefix}_contains_pole"
        elif kind == 1:
            b, tag = inside, f"{prefix}_inside_point"
        else:
            b, tag = outside, f"{prefix}_outside_point"
        pairs.append({"i": i, "tag": tag, "a": a, "b": b})
        i += 1
    return pairs


def family_h(rng: random.Random) -> list[dict]:
    """100 area/length/perimeter/centroid singles, including a hole-centroid case."""
    items = []
    # Slot 0: the documented mpoly_with_hole (~0.48 deg gap).
    items.append(
        {
            "i": 0,
            "tag": "mpoly_with_hole_documented",
            "g": {
                "type": "MultiPolygon",
                "coordinates": [
                    [
                        [[0, 0], [4, 0], [4, 4], [0, 4], [0, 0]],
                        [[1, 1], [2, 1], [2, 2], [1, 2], [1, 1]],
                    ],
                    [[[6, 0], [8, 0], [8, 2], [6, 2], [6, 0]]],
                ],
            },
        }
    )
    for i in range(1, 100):
        kind = i % 10
        lon, lat = pick_center(rng)
        half = rng.uniform(30.0, 800.0)
        if kind == 0:
            tag, g = "point", point(lon, lat)
        elif kind == 1:
            tag, g = "multipoint", multipoint(
                [[lon, lat], [lon + 0.01, lat], [lon, lat + 0.01]]
            )
        elif kind == 2:
            tag, g = "linestring", linestring(
                [
                    [lon, lat],
                    [lon + dlon_m(half, lat), lat + dlat_m(half * 0.3)],
                    [lon + dlon_m(half * 2, lat), lat],
                ]
            )
        elif kind == 3:
            tag, g = "multilinestring", {
                "type": "MultiLineString",
                "coordinates": [
                    [[lon, lat], [lon + 0.02, lat]],
                    [[lon, lat + 0.01], [lon + 0.02, lat + 0.01]],
                ],
            }
        elif kind == 4:
            tag, g = "polygon_small", polygon([irregular_ngon(lon, lat, half, rng)])
        elif kind == 5:
            tag, g = "polygon_square", polygon([square(lon, lat, half)])
        elif kind == 6:
            outer = square(lon, lat, half * 3)
            hole = square(lon, lat, half)
            tag, g = "polygon_with_hole", polygon([outer, hole])
        elif kind == 7:
            tag, g = "multipolygon", multipolygon(
                [
                    [square(lon, lat, half)],
                    [square(lon + dlon_m(half * 4, lat), lat, half * 0.7)],
                ]
            )
        elif kind == 8:
            tag, g = "polygon_dateline", polygon(
                [rect(179.0, lat - 0.5, -179.0, lat + 0.5)]
            )
        else:
            tag, g = "polygon_large", polygon(
                [rect(lon, lat, lon + 20.0, clamp_lat(lat + 15.0))]
            )
        items.append({"i": i, "tag": tag, "g": g})
    return items


D_PREDICATES = """    ST_IsValid(ST_GeomFromGeoJSON(v.a)::geometry) AS a_valid,
    ST_IsValidReason(ST_GeomFromGeoJSON(v.a)::geometry) AS a_valid_reason,
    ST_IsValid(ST_GeomFromGeoJSON(v.b)::geometry) AS b_valid,
    ST_Area(ST_GeomFromGeoJSON(v.a)::geography) AS a_area_m2,
    ST_Perimeter(ST_GeomFromGeoJSON(v.a)::geography) AS a_perimeter_m,
    ST_X(ST_Centroid(ST_GeomFromGeoJSON(v.a)::geography)::geometry) AS a_centroid_lon,
    ST_Y(ST_Centroid(ST_GeomFromGeoJSON(v.a)::geography)::geometry) AS a_centroid_lat,
    ST_Intersects(ST_GeomFromGeoJSON(v.a)::geography, ST_GeomFromGeoJSON(v.b)::geography) AS intersects,
    ST_Within(ST_GeomFromGeoJSON(v.b)::geometry, ST_GeomFromGeoJSON(v.a)::geometry) AS point_within_a,
    ST_Contains(ST_GeomFromGeoJSON(v.a)::geometry, ST_GeomFromGeoJSON(v.b)::geometry) AS a_contains_point,
    ST_Distance(ST_GeomFromGeoJSON(v.a)::geography, ST_GeomFromGeoJSON(v.b)::geography) AS distance_m"""


def main() -> int:
    rng = random.Random(SEED)
    version = psql_c("SELECT PostGIS_Full_Version();")
    print(f"PostGIS: {version}", file=sys.stderr)

    families = {}

    print("family a: nearby polygons (300)", file=sys.stderr)
    sql, rows = batch_pairs(family_a(rng), PAIR_PREDICATES)
    families["a_nearby_polygons"] = {"n": len(rows), "sql": sql, "pairs": rows}

    print("family b: holes vs points (100)", file=sys.stderr)
    sql, rows = batch_pairs(family_b(rng), PAIR_PREDICATES)
    families["b_holes"] = {"n": len(rows), "sql": sql, "pairs": rows}

    print("family c: antimeridian + poles (150)", file=sys.stderr)
    sql, rows = batch_pairs(family_c(rng), PAIR_PREDICATES)
    families["c_antimeridian_poles"] = {"n": len(rows), "sql": sql, "pairs": rows}

    print("family d: degenerates (100)", file=sys.stderr)
    sql, rows = batch_pairs(family_d(rng), D_PREDICATES)
    families["d_degenerate"] = {"n": len(rows), "sql": sql, "pairs": rows}

    print("family e: dwithin thresholds (100)", file=sys.stderr)
    sql, rows = batch_pairs(family_e(rng), E_PREDICATES)
    families["e_dwithin"] = {"n": len(rows), "sql": sql, "pairs": rows}

    print("family f: multi / linestring (100)", file=sys.stderr)
    sql, rows = batch_pairs(family_f(rng), PAIR_PREDICATES)
    families["f_multi"] = {"n": len(rows), "sql": sql, "pairs": rows}

    print("family g: large + pole-containing (70)", file=sys.stderr)
    sql, rows = batch_pairs(family_g(rng), PAIR_PREDICATES)
    families["g_large_pole"] = {"n": len(rows), "sql": sql, "pairs": rows}

    print("family h: measures (100)", file=sys.stderr)
    sql, rows = batch_singles(family_h(rng), MEASURES)
    families["h_measures"] = {"n": len(rows), "sql": sql, "singles": rows}

    fixture = {
        "seed": SEED,
        "postgis_version": version,
        "unit_semantics": {
            "geography": [
                "intersects",
                "dwithin_m",
                "distance_m",
                "area_m2",
                "length_m",
                "perimeter_m",
                "centroid",
            ],
            "geometry_planar": ["within", "contains", "covers"],
            "source": "src/query.rs GeometryFilter; src/spatial_geometry.rs header",
        },
        "families": families,
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(fixture, indent=2) + "\n")
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
