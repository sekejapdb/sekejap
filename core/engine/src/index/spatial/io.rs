//! Geometry I/O: the byte and text forms PostGIS reads and writes, as pure
//! functions over [`Geom`]. `docs/lang/QL_CONTRACT.md` §4.4.
//!
//! Four forms, each one a spelling of the same six shapes:
//!
//! - **WKB**, the OGC Well-Known Binary: one byte-order byte, a `u32` type
//!   code (1 Point .. 6 MultiPolygon), then counts and `f64` coordinates in
//!   that byte order. A multi-geometry holds complete WKB members, each with
//!   its OWN byte-order byte, so one value can mix the two orders.
//! - **EWKB**, PostGIS's extension: the same, with the high bit `0x20000000`
//!   of the type code saying a `u32` SRID follows it. This is what a
//!   PostgreSQL `geometry` column prints as hex.
//! - **WKT**, the text form `POINT(1 2)`, printed the way PostGIS 3's
//!   `ST_AsText` prints it: shortest round-trip numbers capped at 15 decimal
//!   places, no spaces after commas, `MULTIPOINT((1 2),(3 4))`.
//! - **GeoJSON text**, printed the way `ST_AsGeoJSON` prints it: at most
//!   `max_decimals` decimal places, which PostGIS defaults to 9.
//!
//! What is REFUSED rather than guessed at, each with a named reason: Z and M
//! coordinates (a [`Geom`] is two-dimensional), EMPTY geometries (a [`Geom`]
//! has no empty form), `GEOMETRYCOLLECTION`, a non-finite coordinate, an
//! unclosed or too-short ring, a line of fewer than two points, bytes after
//! the geometry, and any count larger than the bytes that remain could hold.
//! That last check runs BEFORE anything is allocated, so a four-byte count
//! off the wire cannot ask for four billion points (Law 1).
//!
//! Longitude and latitude are NOT range-checked here: these functions only
//! translate. Whoever stores or queries the shape applies the WGS84 bounds,
//! exactly as it does for a GeoJSON document.

use kernel::spatial::Geom;
use std::fmt;

/// The byte order of a WKB value. PostgreSQL calls them `NDR` and `XDR`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteOrder {
    /// `NDR`, little-endian: byte-order byte `1`.
    Little,
    /// `XDR`, big-endian: byte-order byte `0`.
    Big,
}

/// A shape read from WKB, EWKB, WKT or EWKT, with the SRID the input named.
/// `None` means the input carried no SRID, which PostGIS calls SRID 0.
#[derive(Clone, Debug, PartialEq)]
pub struct Decoded {
    pub geometry: Geom,
    pub srid: Option<i32>,
}

/// Why an input could not be read as one of the six shapes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeometryIoError(String);

impl GeometryIoError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for GeometryIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GeometryIoError {}

pub type IoResult<T> = std::result::Result<T, GeometryIoError>;

const EWKB_Z: u32 = 0x8000_0000;
const EWKB_M: u32 = 0x4000_0000;
const EWKB_SRID: u32 = 0x2000_0000;

// ── writing ──────────────────────────────────────────────────────────────

/// OGC WKB, as `ST_AsBinary(g, 'NDR' | 'XDR')` writes it.
pub fn to_wkb(geometry: &Geom, order: ByteOrder) -> Vec<u8> {
    let mut out = Vec::with_capacity(wkb_len(geometry));
    Writer { out: &mut out, order }.geometry(geometry, None);
    out
}

/// PostGIS EWKB, as `ST_AsEWKB(g, 'NDR' | 'XDR')` writes it: the SRID, when
/// there is one, rides on the OUTER geometry only.
pub fn to_ewkb(geometry: &Geom, srid: Option<i32>, order: ByteOrder) -> Vec<u8> {
    let mut out = Vec::with_capacity(wkb_len(geometry) + 4);
    Writer { out: &mut out, order }.geometry(geometry, srid);
    out
}

/// Lower-case hexadecimal, the digits of a PostgreSQL `bytea` in hex output.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// A `bytea` in its text form: hex digits in either case, with or without
/// PostgreSQL's `\x` prefix.
pub fn from_hex(text: &str) -> IoResult<Vec<u8>> {
    let digits = text.strip_prefix("\\x").unwrap_or(text).as_bytes();
    if digits.len() % 2 != 0 {
        return Err(GeometryIoError::new("hex text has an odd number of digits"));
    }
    let nibble = |c: u8| -> IoResult<u8> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(GeometryIoError::new(format!(
                "`{}` is not a hex digit",
                c as char
            ))),
        }
    };
    digits
        .chunks_exact(2)
        .map(|pair| Ok(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}

fn wkb_len(geometry: &Geom) -> usize {
    const HEAD: usize = 5;
    match geometry {
        Geom::Point(..) => HEAD + 16,
        Geom::LineString(c) => HEAD + 4 + 16 * c.len(),
        Geom::Polygon(rings) => HEAD + 4 + rings.iter().map(|r| 4 + 16 * (r.len() + 1)).sum::<usize>(),
        Geom::MultiPoint(c) => HEAD + 4 + (HEAD + 16) * c.len(),
        Geom::MultiLineString(lines) => {
            HEAD + 4 + lines.iter().map(|l| HEAD + 4 + 16 * l.len()).sum::<usize>()
        }
        Geom::MultiPolygon(polys) => {
            HEAD + 4
                + polys
                    .iter()
                    .map(|p| HEAD + 4 + p.iter().map(|r| 4 + 16 * (r.len() + 1)).sum::<usize>())
                    .sum::<usize>()
        }
    }
}

struct Writer<'a> {
    out: &'a mut Vec<u8>,
    order: ByteOrder,
}

impl Writer<'_> {
    fn u32(&mut self, v: u32) {
        match self.order {
            ByteOrder::Little => self.out.extend_from_slice(&v.to_le_bytes()),
            ByteOrder::Big => self.out.extend_from_slice(&v.to_be_bytes()),
        }
    }

    fn f64(&mut self, v: f64) {
        match self.order {
            ByteOrder::Little => self.out.extend_from_slice(&v.to_le_bytes()),
            ByteOrder::Big => self.out.extend_from_slice(&v.to_be_bytes()),
        }
    }

    fn head(&mut self, code: u32, srid: Option<i32>) {
        self.out.push(match self.order {
            ByteOrder::Little => 1,
            ByteOrder::Big => 0,
        });
        match srid {
            Some(srid) => {
                self.u32(code | EWKB_SRID);
                self.u32(srid as u32);
            }
            None => self.u32(code),
        }
    }

    fn count(&mut self, n: usize) {
        self.u32(u32::try_from(n).expect("a geometry holds fewer than 2^32 elements"));
    }

    fn point(&mut self, p: [f64; 2]) {
        self.f64(p[0]);
        self.f64(p[1]);
    }

    fn points(&mut self, points: &[[f64; 2]]) {
        self.count(points.len());
        for p in points {
            self.point(*p);
        }
    }

    fn rings(&mut self, rings: &[Vec<[f64; 2]>]) {
        self.count(rings.len());
        for ring in rings {
            let closed = closed(ring);
            self.count(ring.len() + usize::from(!closed));
            for p in ring {
                self.point(*p);
            }
            if !closed {
                self.point(ring[0]);
            }
        }
    }

    fn geometry(&mut self, geometry: &Geom, srid: Option<i32>) {
        match geometry {
            Geom::Point(x, y) => {
                self.head(1, srid);
                self.point([*x, *y]);
            }
            Geom::LineString(points) => {
                self.head(2, srid);
                self.points(points);
            }
            Geom::Polygon(rings) => {
                self.head(3, srid);
                self.rings(rings);
            }
            Geom::MultiPoint(points) => {
                self.head(4, srid);
                self.count(points.len());
                for p in points {
                    self.head(1, None);
                    self.point(*p);
                }
            }
            Geom::MultiLineString(lines) => {
                self.head(5, srid);
                self.count(lines.len());
                for line in lines {
                    self.head(2, None);
                    self.points(line);
                }
            }
            Geom::MultiPolygon(polygons) => {
                self.head(6, srid);
                self.count(polygons.len());
                for rings in polygons {
                    self.head(3, None);
                    self.rings(rings);
                }
            }
        }
    }
}

/// A ring is closed when its first and last points are the same. An empty
/// ring counts as closed: it has no first point to repeat.
fn closed(ring: &[[f64; 2]]) -> bool {
    ring.first() == ring.last()
}

/// WKT, as PostGIS 3's `ST_AsText` prints it.
pub fn to_wkt(geometry: &Geom) -> String {
    let mut out = String::new();
    let coords = |out: &mut String, points: &[[f64; 2]], close: bool| {
        out.push('(');
        for (at, p) in points.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            push_pair(out, *p);
        }
        if close && !points.is_empty() && !closed(points) {
            out.push(',');
            push_pair(out, points[0]);
        }
        out.push(')');
    };
    let rings = |out: &mut String, rings: &[Vec<[f64; 2]>]| {
        out.push('(');
        for (at, ring) in rings.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            coords(out, ring, true);
        }
        out.push(')');
    };
    match geometry {
        Geom::Point(x, y) => {
            out.push_str("POINT(");
            push_pair(&mut out, [*x, *y]);
            out.push(')');
        }
        Geom::LineString(points) => {
            out.push_str("LINESTRING");
            coords(&mut out, points, false);
        }
        Geom::Polygon(r) => {
            out.push_str("POLYGON");
            rings(&mut out, r);
        }
        Geom::MultiPoint(points) => {
            out.push_str("MULTIPOINT(");
            for (at, p) in points.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                out.push('(');
                push_pair(&mut out, *p);
                out.push(')');
            }
            out.push(')');
        }
        Geom::MultiLineString(lines) => {
            out.push_str("MULTILINESTRING(");
            for (at, line) in lines.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                coords(&mut out, line, false);
            }
            out.push(')');
        }
        Geom::MultiPolygon(polygons) => {
            out.push_str("MULTIPOLYGON(");
            for (at, polygon) in polygons.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                rings(&mut out, polygon);
            }
            out.push(')');
        }
    }
    out
}

fn push_pair(out: &mut String, p: [f64; 2]) {
    out.push_str(&number(p[0], 15));
    out.push(' ');
    out.push_str(&number(p[1], 15));
}

/// A coordinate the way PostGIS prints one: the shortest decimal that reads
/// back to the same `f64`, rounded to at most `max_decimals` places when it
/// is longer, trailing zeros dropped, and no negative zero.
///
/// The rounding is of that shortest DECIMAL, half away from zero -- not of
/// the exact binary value. The two differ: `-6.8963276777320175` is stored
/// as a double a hair below `...0175`, so rounding the binary gives `...017`
/// where PostGIS prints `...018`. The fixture holds that case.
fn number(value: f64, max_decimals: usize) -> String {
    // Rust's `{}` for an `f64` is the shortest round-trip decimal and never
    // uses an exponent.
    let shortest = format!("{value}");
    let (negative, unsigned) = match shortest.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, shortest.as_str()),
    };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let mut text = if fraction.len() > max_decimals {
        let mut digits: Vec<u8> = whole.bytes().chain(fraction.bytes().take(max_decimals)).collect();
        if fraction.as_bytes()[max_decimals] >= b'5' {
            let mut at = digits.len();
            loop {
                if at == 0 {
                    digits.insert(0, b'1');
                    break;
                }
                at -= 1;
                if digits[at] == b'9' {
                    digits[at] = b'0';
                } else {
                    digits[at] += 1;
                    break;
                }
            }
        }
        let split = digits.len() - max_decimals;
        let mut out = String::from_utf8(digits[..split].to_vec()).expect("ascii digits");
        let decimals = std::str::from_utf8(&digits[split..]).expect("ascii digits");
        let decimals = decimals.trim_end_matches('0');
        if !decimals.is_empty() {
            out.push('.');
            out.push_str(decimals);
        }
        out
    } else {
        unsigned.to_owned()
    };
    if negative && text.bytes().any(|c| matches!(c, b'1'..=b'9')) {
        text.insert(0, '-');
    }
    text
}

/// GeoJSON text, as `ST_AsGeoJSON(g, max_decimals)` prints it -- no spaces,
/// `type` before `coordinates`, rings written closed.
pub fn to_geojson(geometry: &Geom, max_decimals: usize) -> String {
    let pair = |out: &mut String, p: [f64; 2]| {
        out.push('[');
        out.push_str(&number(p[0], max_decimals));
        out.push(',');
        out.push_str(&number(p[1], max_decimals));
        out.push(']');
    };
    let list = |out: &mut String, points: &[[f64; 2]], close: bool| {
        out.push('[');
        for (at, p) in points.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            pair(out, *p);
        }
        if close && !points.is_empty() && !closed(points) {
            out.push(',');
            pair(out, points[0]);
        }
        out.push(']');
    };
    let rings = |out: &mut String, rings: &[Vec<[f64; 2]>]| {
        out.push('[');
        for (at, ring) in rings.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            list(out, ring, true);
        }
        out.push(']');
    };
    let (kind, mut out) = match geometry {
        Geom::Point(..) => ("Point", String::new()),
        Geom::LineString(_) => ("LineString", String::new()),
        Geom::Polygon(_) => ("Polygon", String::new()),
        Geom::MultiPoint(_) => ("MultiPoint", String::new()),
        Geom::MultiLineString(_) => ("MultiLineString", String::new()),
        Geom::MultiPolygon(_) => ("MultiPolygon", String::new()),
    };
    out.push_str("{\"type\":\"");
    out.push_str(kind);
    out.push_str("\",\"coordinates\":");
    match geometry {
        Geom::Point(x, y) => pair(&mut out, [*x, *y]),
        Geom::LineString(points) | Geom::MultiPoint(points) => list(&mut out, points, false),
        Geom::Polygon(r) => rings(&mut out, r),
        Geom::MultiLineString(lines) => {
            out.push('[');
            for (at, line) in lines.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                list(&mut out, line, false);
            }
            out.push(']');
        }
        Geom::MultiPolygon(polygons) => {
            out.push('[');
            for (at, polygon) in polygons.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                rings(&mut out, polygon);
            }
            out.push(']');
        }
    }
    out.push('}');
    out
}

// ── reading WKB and EWKB ─────────────────────────────────────────────────

/// WKB or EWKB, in either byte order, with members in either order. The whole
/// input must be one geometry.
pub fn from_wkb(bytes: &[u8]) -> IoResult<Decoded> {
    let mut reader = Reader { bytes, at: 0 };
    let (geometry, srid) = reader.geometry(None)?;
    if reader.at != bytes.len() {
        return Err(GeometryIoError::new(format!(
            "{} bytes follow the geometry",
            bytes.len() - reader.at
        )));
    }
    Ok(Decoded { geometry, srid })
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    fn take<const N: usize>(&mut self) -> IoResult<[u8; N]> {
        let slice = self
            .bytes
            .get(self.at..self.at + N)
            .ok_or_else(|| GeometryIoError::new("the WKB ends inside a geometry"))?;
        self.at += N;
        Ok(slice.try_into().expect("slice of N bytes"))
    }

    fn u32(&mut self, order: ByteOrder) -> IoResult<u32> {
        let raw = self.take::<4>()?;
        Ok(match order {
            ByteOrder::Little => u32::from_le_bytes(raw),
            ByteOrder::Big => u32::from_be_bytes(raw),
        })
    }

    fn f64(&mut self, order: ByteOrder) -> IoResult<f64> {
        let raw = self.take::<8>()?;
        let value = match order {
            ByteOrder::Little => f64::from_le_bytes(raw),
            ByteOrder::Big => f64::from_be_bytes(raw),
        };
        if value.is_nan() {
            return Err(GeometryIoError::new(
                "a NaN coordinate: PostGIS writes POINT EMPTY that way, and an EMPTY geometry has no form here",
            ));
        }
        if !value.is_finite() {
            return Err(GeometryIoError::new("a coordinate is not finite"));
        }
        Ok(value)
    }

    /// A count, checked against the bytes left BEFORE it is used to size
    /// anything: each element needs at least `min_bytes`.
    fn count(&mut self, order: ByteOrder, min_bytes: usize, what: &str) -> IoResult<usize> {
        let n = self.u32(order)? as usize;
        if n == 0 {
            return Err(GeometryIoError::new(format!(
                "an EMPTY {what}: an empty geometry has no form here"
            )));
        }
        if n > self.remaining() / min_bytes {
            return Err(GeometryIoError::new(format!(
                "the {what} count {n} is larger than the {} bytes left can hold",
                self.remaining()
            )));
        }
        Ok(n)
    }

    fn point(&mut self, order: ByteOrder) -> IoResult<[f64; 2]> {
        Ok([self.f64(order)?, self.f64(order)?])
    }

    fn points(&mut self, order: ByteOrder, what: &str) -> IoResult<Vec<[f64; 2]>> {
        let n = self.count(order, 16, what)?;
        let mut points = Vec::with_capacity(n);
        for _ in 0..n {
            points.push(self.point(order)?);
        }
        Ok(points)
    }

    fn rings(&mut self, order: ByteOrder) -> IoResult<Vec<Vec<[f64; 2]>>> {
        let n = self.count(order, 4, "ring")?;
        let mut rings = Vec::with_capacity(n);
        for _ in 0..n {
            let ring = self.points(order, "ring point")?;
            check_ring(&ring)?;
            rings.push(ring);
        }
        Ok(rings)
    }

    /// One geometry. `member` is the simple type a multi-geometry's member
    /// must be, and a member carries no SRID.
    fn geometry(&mut self, member: Option<u32>) -> IoResult<(Geom, Option<i32>)> {
        let order = match self.take::<1>()?[0] {
            0 => ByteOrder::Big,
            1 => ByteOrder::Little,
            other => {
                return Err(GeometryIoError::new(format!(
                    "byte-order byte {other} is neither 0 (XDR) nor 1 (NDR)"
                )))
            }
        };
        let raw = self.u32(order)?;
        if raw & (EWKB_Z | EWKB_M) != 0 {
            return Err(GeometryIoError::new(
                "a Z or M coordinate: shapes here are two-dimensional (lon, lat)",
            ));
        }
        let srid = if raw & EWKB_SRID != 0 {
            if member.is_some() {
                return Err(GeometryIoError::new("a member of a multi-geometry carries an SRID"));
            }
            Some(self.u32(order)? as i32)
        } else {
            None
        };
        let code = raw & !EWKB_SRID;
        if code >= 1000 {
            return Err(GeometryIoError::new(format!(
                "ISO WKB type {code} is a Z, M or ZM shape: shapes here are two-dimensional (lon, lat)"
            )));
        }
        if let Some(want) = member {
            if code != want {
                return Err(GeometryIoError::new(format!(
                    "a multi-geometry member has type {code} where type {want} belongs"
                )));
            }
        }
        let geometry = match code {
            1 => {
                let p = self.point(order)?;
                Geom::Point(p[0], p[1])
            }
            2 => {
                let points = self.points(order, "line point")?;
                check_line(&points)?;
                Geom::LineString(points)
            }
            3 => Geom::Polygon(self.rings(order)?),
            4 => {
                let n = self.count(order, 21, "multipoint member")?;
                let mut points = Vec::with_capacity(n);
                for _ in 0..n {
                    let (Geom::Point(x, y), _) = self.geometry(Some(1))? else {
                        unreachable!("member type checked")
                    };
                    points.push([x, y]);
                }
                Geom::MultiPoint(points)
            }
            5 => {
                let n = self.count(order, 9, "multilinestring member")?;
                let mut lines = Vec::with_capacity(n);
                for _ in 0..n {
                    let (Geom::LineString(line), _) = self.geometry(Some(2))? else {
                        unreachable!("member type checked")
                    };
                    lines.push(line);
                }
                Geom::MultiLineString(lines)
            }
            6 => {
                let n = self.count(order, 9, "multipolygon member")?;
                let mut polygons = Vec::with_capacity(n);
                for _ in 0..n {
                    let (Geom::Polygon(rings), _) = self.geometry(Some(3))? else {
                        unreachable!("member type checked")
                    };
                    polygons.push(rings);
                }
                Geom::MultiPolygon(polygons)
            }
            7 => {
                return Err(GeometryIoError::new(
                    "GEOMETRYCOLLECTION: the six shapes here are Point, LineString, Polygon and their Multi forms",
                ))
            }
            other => {
                return Err(GeometryIoError::new(format!(
                    "WKB type {other} is not one of the six shapes (1..=6)"
                )))
            }
        };
        Ok((geometry, srid))
    }
}

fn check_line(points: &[[f64; 2]]) -> IoResult<()> {
    if points.len() < 2 {
        return Err(GeometryIoError::new("a line needs at least two points"));
    }
    Ok(())
}

/// PostGIS refuses a ring that does not end where it starts, and one of fewer
/// than four points; the same two rules apply here.
fn check_ring(ring: &[[f64; 2]]) -> IoResult<()> {
    if ring.len() < 4 {
        return Err(GeometryIoError::new("a polygon ring needs at least four points"));
    }
    if !closed(ring) {
        return Err(GeometryIoError::new("a polygon ring does not end where it starts"));
    }
    Ok(())
}

// ── reading WKT and EWKT ─────────────────────────────────────────────────

/// WKT, or PostGIS's EWKT with an `SRID=n;` prefix. Keywords are read in
/// either case; `MULTIPOINT(1 2,3 4)` and `MULTIPOINT((1 2),(3 4))` are both
/// accepted, as PostGIS accepts both.
pub fn from_wkt(text: &str) -> IoResult<Decoded> {
    let mut srid = None;
    let mut body = text.trim();
    if body.len() >= 5 && body[..5].eq_ignore_ascii_case("SRID=") {
        let (head, rest) = body
            .split_once(';')
            .ok_or_else(|| GeometryIoError::new("EWKT `SRID=n` must be followed by `;`"))?;
        srid = Some(
            head[5..]
                .trim()
                .parse::<i32>()
                .map_err(|_| GeometryIoError::new(format!("`{}` is not an SRID", &head[5..])))?,
        );
        body = rest;
    }
    let mut parser = Wkt { text: body.as_bytes(), at: 0 };
    let geometry = parser.geometry()?;
    parser.space();
    if parser.at != parser.text.len() {
        return Err(parser.error("text follows the geometry"));
    }
    Ok(Decoded { geometry, srid })
}

struct Wkt<'a> {
    text: &'a [u8],
    at: usize,
}

impl Wkt<'_> {
    fn error(&self, message: &str) -> GeometryIoError {
        GeometryIoError::new(format!("WKT at byte {}: {message}", self.at))
    }

    fn space(&mut self) {
        while self.text.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        self.space();
        if self.text.get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8) -> IoResult<()> {
        if self.eat(byte) {
            Ok(())
        } else {
            Err(self.error(&format!("expected `{}`", byte as char)))
        }
    }

    fn word(&mut self) -> String {
        self.space();
        let start = self.at;
        while self.text.get(self.at).is_some_and(u8::is_ascii_alphabetic) {
            self.at += 1;
        }
        String::from_utf8_lossy(&self.text[start..self.at]).to_ascii_uppercase()
    }

    fn number(&mut self) -> IoResult<f64> {
        self.space();
        let start = self.at;
        while self
            .text
            .get(self.at)
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E'))
        {
            self.at += 1;
        }
        let written = std::str::from_utf8(&self.text[start..self.at]).unwrap_or("");
        // Only digits reach `parse`, so `NaN` and `inf` are never numbers.
        if !written.bytes().any(|c| c.is_ascii_digit()) {
            return Err(self.error("expected a number"));
        }
        let value: f64 = written
            .parse()
            .map_err(|_| self.error(&format!("`{written}` is not a number")))?;
        if !value.is_finite() {
            return Err(self.error("a coordinate is not finite"));
        }
        Ok(value)
    }

    fn pair(&mut self) -> IoResult<[f64; 2]> {
        let x = self.number()?;
        let y = self.number()?;
        self.space();
        if self
            .text
            .get(self.at)
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.'))
        {
            return Err(self.error(
                "a third coordinate: shapes here are two-dimensional (lon, lat)",
            ));
        }
        Ok([x, y])
    }

    /// `(x y, x y, ...)`.
    fn list(&mut self) -> IoResult<Vec<[f64; 2]>> {
        self.expect(b'(')?;
        let mut points = vec![self.pair()?];
        while self.eat(b',') {
            points.push(self.pair()?);
        }
        self.expect(b')')?;
        Ok(points)
    }

    /// `((...), (...))`.
    fn rings(&mut self) -> IoResult<Vec<Vec<[f64; 2]>>> {
        self.expect(b'(')?;
        let mut rings = Vec::new();
        loop {
            let ring = self.list()?;
            check_ring(&ring).map_err(|e| self.error(&e.to_string()))?;
            rings.push(ring);
            if !self.eat(b',') {
                break;
            }
        }
        self.expect(b')')?;
        Ok(rings)
    }

    fn geometry(&mut self) -> IoResult<Geom> {
        let kind = self.word();
        let dimension = self.word();
        match dimension.as_str() {
            "" => {}
            "EMPTY" => {
                return Err(self.error("an EMPTY geometry has no form here"));
            }
            "Z" | "M" | "ZM" => {
                return Err(self.error(
                    "a Z or M coordinate: shapes here are two-dimensional (lon, lat)",
                ))
            }
            other => return Err(self.error(&format!("unexpected `{other}`"))),
        }
        Ok(match kind.as_str() {
            "POINT" => {
                self.expect(b'(')?;
                let p = self.pair()?;
                self.expect(b')')?;
                Geom::Point(p[0], p[1])
            }
            "LINESTRING" => {
                let points = self.list()?;
                check_line(&points).map_err(|e| self.error(&e.to_string()))?;
                Geom::LineString(points)
            }
            "POLYGON" => Geom::Polygon(self.rings()?),
            "MULTIPOINT" => {
                self.expect(b'(')?;
                let mut points = Vec::new();
                loop {
                    // Both spellings: `(1 2)` members and bare `1 2` pairs.
                    if self.eat(b'(') {
                        points.push(self.pair()?);
                        self.expect(b')')?;
                    } else {
                        points.push(self.pair()?);
                    }
                    if !self.eat(b',') {
                        break;
                    }
                }
                self.expect(b')')?;
                Geom::MultiPoint(points)
            }
            "MULTILINESTRING" => {
                self.expect(b'(')?;
                let mut lines = Vec::new();
                loop {
                    let line = self.list()?;
                    check_line(&line).map_err(|e| self.error(&e.to_string()))?;
                    lines.push(line);
                    if !self.eat(b',') {
                        break;
                    }
                }
                self.expect(b')')?;
                Geom::MultiLineString(lines)
            }
            "MULTIPOLYGON" => {
                self.expect(b'(')?;
                let mut polygons = Vec::new();
                loop {
                    polygons.push(self.rings()?);
                    if !self.eat(b',') {
                        break;
                    }
                }
                self.expect(b')')?;
                Geom::MultiPolygon(polygons)
            }
            "GEOMETRYCOLLECTION" => {
                return Err(self.error(
                    "GEOMETRYCOLLECTION: the six shapes here are Point, LineString, Polygon and their Multi forms",
                ))
            }
            "" => return Err(self.error("expected a geometry type")),
            other => return Err(self.error(&format!("`{other}` is not a geometry type"))),
        })
    }
}
