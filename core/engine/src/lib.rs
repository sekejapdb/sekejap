use kernel::spatial::Geom;
use serde_json::{Map, Value};
pub mod collections;
mod index;
mod query;
pub mod store;

// The public module paths this crate has always offered. The tree below them
// moved; the names did not, and neither did what they export.
pub use crate::index::spatial::{geometry as spatial_geometry, math as spatial_math};
pub use crate::store as collection_backend;
pub use crate::store::{pagewal, recovery};
pub(crate) use crate::index::text::analyzer as text_analyzer;
pub(crate) use crate::index::vector::quant as vector_quant;
pub(crate) use crate::store::{dense_v3, scalar_key};

/// What the layers above reach for by name, and nothing else.
///
/// The SQL slice used to be a module of this crate and read these through
/// `pub(crate)`; it is `sekejap-lang` now and the compiler enforces the
/// boundary, so the few items it needs are re-exported here rather than by
/// making their modules public. Nothing in this module is part of the engine
/// contract (`docs/core/GRAPH_CONTRACT.md`, `docs/core/COLLECTIONS.md`); it is
/// the core -> lang interface, listed in one place so it can be read.
pub mod internal {
    pub use crate::query::EDGE_FIELD_PREFIX;
}
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Text,
    Int,
    Real,
    Bool,
    Json,
    Geo,
    Point,
    Vector(usize),
}
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub id: u64,
    pub fields: Vec<(String, Kind)>,
}
pub struct Encoded {
    pub row: Vec<u8>,
    pub vectors: Vec<(usize, Vec<u8>)>,
}
/// P1 header: layout ID, flags (bit0=state bitmap, bit1=extras map), bodies.
/// Absent/null states are preserved; all-present rows omit the bitmap.
pub fn encode_dense(layout: &Layout, doc: &Value) -> Result<Encoded> {
    let e = encode(layout, doc)?;
    let mut read = Read { b: &e.row, p: 0 };
    let id = read.uv()?;
    let header = read.p;
    let has_states = layout
        .fields
        .iter()
        .any(|(n, _)| doc.get(n).is_none_or(Value::is_null));
    let has_extras = doc
        .as_object()
        .ok_or("object required")?
        .keys()
        .any(|n| !layout.fields.iter().any(|(f, _)| f == n));
    let states_len = layout.fields.len().div_ceil(4);
    let mut row = Vec::new();
    uv(id, &mut row);
    row.push(u8::from(has_states) | (u8::from(has_extras) << 1));
    if has_states {
        row.extend_from_slice(&e.row[header..header + states_len]);
    }
    let end = e.row.len() - if has_extras { 0 } else { 2 };
    row.extend_from_slice(&e.row[header + states_len..end]);
    Ok(Encoded {
        row,
        vectors: e.vectors,
    })
}
pub fn decode_dense(
    layout: &Layout,
    b: &[u8],
    get: impl FnMut(usize) -> Result<Vec<u8>>,
) -> Result<Value> {
    let mut r = Read { b, p: 0 };
    let id = r.uv()?;
    if id != layout.id {
        return Err("wrong layout".into());
    }
    let flags = r.byte()?;
    if flags & !3 != 0 {
        return Err("invalid dense record flags".into());
    }
    let mut original = Vec::new();
    uv(id, &mut original);
    let n = layout.fields.len().div_ceil(4);
    if flags & 1 != 0 {
        original.extend_from_slice(r.take(n)?);
    } else {
        let mut states = vec![0; n];
        for i in 0..layout.fields.len() {
            states[i / 4] |= 2 << ((i % 4) * 2);
        }
        original.extend_from_slice(&states);
    }
    original.extend_from_slice(&b[r.p..]);
    if flags & 2 == 0 {
        original.extend_from_slice(&[8, 0]);
    }
    decode(layout, &original, get)
}
/// P1 revision 2 combines layout ID and presence flags in one varint.
/// Layout IDs are bounded to 32 bits; identifiers are immutable, never reused.
pub fn encode_dense_v2(layout: &Layout, doc: &Value) -> Result<Encoded> {
    if layout.id > u32::MAX as u64 {
        return Err("layout ID domain".into());
    }
    let e = encode_dense(layout, doc)?;
    let mut r = Read { b: &e.row, p: 0 };
    let id = r.uv()?;
    let flags = r.byte()?;
    let mut row = Vec::new();
    uv((id << 2) | flags as u64, &mut row);
    row.extend_from_slice(&e.row[r.p..]);
    Ok(Encoded {
        row,
        vectors: e.vectors,
    })
}
pub fn decode_dense_v2(
    layout: &Layout,
    b: &[u8],
    get: impl FnMut(usize) -> Result<Vec<u8>>,
) -> Result<Value> {
    let mut r = Read { b, p: 0 };
    let h = r.uv()?;
    if h >> 2 > u32::MAX as u64 {
        return Err("layout ID domain".into());
    }
    let mut original = Vec::new();
    uv(h >> 2, &mut original);
    original.push((h & 3) as u8);
    original.extend_from_slice(&b[r.p..]);
    decode_dense(layout, &original, get)
}
fn width_set(bits: &mut [u8], index: usize, width: u8) {
    for j in 0..3 {
        bits[(index * 3 + j) / 8] |= ((width >> j) & 1) << ((index * 3 + j) % 8);
    }
}
fn width_get(bits: &[u8], index: usize) -> u8 {
    let mut v = 0;
    for j in 0..3 {
        v |= ((bits[(index * 3 + j) / 8] >> ((index * 3 + j) % 8)) & 1) << j;
    }
    v
}
#[cfg(test)]
fn skip_slot(r: &mut Read<'_>, kind: &Kind) -> Result<()> {
    match kind {
        Kind::Text | Kind::Geo => {
            r.blob()?;
        }
        Kind::Int => {
            r.int()?;
        }
        Kind::Real => {
            r.float()?;
        }
        Kind::Point => {
            r.take(16)?;
        }
        Kind::Bool => {
            r.byte()?;
        }
        Kind::Json => json_skip(r, 0)?,
        Kind::Vector(_) => {}
    }
    Ok(())
}
/// P1 revision 3: SQLite-inspired width directory, three bits per INTEGER.
/// Every i64 value remains supported. Text lengths and JSON type tags unchanged.
#[cfg(test)]
fn encode_dense_v3_reference(layout: &Layout, doc: &Value) -> Result<Encoded> {
    let e = encode_dense_v2(layout, doc)?;
    let mut r = Read { b: &e.row, p: 0 };
    let h = r.uv()?;
    let states = if h & 1 != 0 {
        Some(r.take(layout.fields.len().div_ceil(4))?)
    } else {
        None
    };
    let mut row = e.row[..r.p].to_vec();
    let at = row.len();
    let integers = layout
        .fields
        .iter()
        .filter(|(_, k)| matches!(k, Kind::Int))
        .count();
    let mut widths = vec![0; (integers * 3).div_ceil(8)];
    row.resize(at + widths.len(), 0);
    let mut index = 0;
    for (i, (_, kind)) in layout.fields.iter().enumerate() {
        let integer = matches!(kind, Kind::Int);
        let has = states.is_none_or(|s| ((s[i / 4] >> ((i % 4) * 2)) & 3) == 2);
        if has {
            if integer {
                let width = r.byte()?;
                width_set(&mut widths, index, width - 1);
                row.extend_from_slice(r.take(width as usize)?);
            } else {
                let start = r.p;
                skip_slot(&mut r, kind)?;
                row.extend_from_slice(&e.row[start..r.p]);
            }
        }
        if integer {
            index += 1;
        }
    }
    row[at..at + widths.len()].copy_from_slice(&widths);
    row.extend_from_slice(&e.row[r.p..]);
    Ok(Encoded {
        row,
        vectors: e.vectors,
    })
}
#[cfg(test)]
fn decode_dense_v3_reference(
    layout: &Layout,
    b: &[u8],
    get: impl FnMut(usize) -> Result<Vec<u8>>,
) -> Result<Value> {
    layout.validate()?;
    let mut r = Read { b, p: 0 };
    let h = r.uv()?;
    if h >> 2 != layout.id {
        return Err("wrong layout".into());
    }
    let states = if h & 1 != 0 {
        Some(r.take(layout.fields.len().div_ceil(4))?)
    } else {
        None
    };
    let mut original = b[..r.p].to_vec();
    let integers = layout
        .fields
        .iter()
        .filter(|(_, k)| matches!(k, Kind::Int))
        .count();
    let widths = r.take((integers * 3).div_ceil(8))?;
    let mut index = 0;
    for (i, (_, kind)) in layout.fields.iter().enumerate() {
        let integer = matches!(kind, Kind::Int);
        let state = states.map_or(2, |s| (s[i / 4] >> ((i % 4) * 2)) & 3);
        if state == 3 {
            return Err("invalid field state".into());
        }
        if state == 2 {
            if integer {
                let width = width_get(widths, index) + 1;
                original.push(width);
                original.extend_from_slice(r.take(width as usize)?);
            } else {
                let start = r.p;
                skip_slot(&mut r, kind)?;
                original.extend_from_slice(&b[start..r.p]);
            }
        }
        if integer {
            index += 1;
        }
    }
    original.extend_from_slice(&b[r.p..]);
    decode_dense_v2(layout, &original, get)
}
fn uv(mut x: u64, out: &mut Vec<u8>) {
    while x >= 128 {
        out.push(x as u8 | 128);
        x >>= 7;
    }
    out.push(x as u8);
}
fn blob(b: &[u8], out: &mut Vec<u8>) {
    uv(b.len() as u64, out);
    out.extend_from_slice(b);
}
fn int(x: i64, out: &mut Vec<u8>) {
    let b = x.to_be_bytes();
    let mut start = 0;
    while start < 7
        && ((b[start] == 0 && b[start + 1] & 128 == 0)
            || (b[start] == 255 && b[start + 1] & 128 != 0))
    {
        start += 1;
    }
    out.push((8 - start) as u8);
    out.extend_from_slice(&b[start..]);
}
struct Read<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Read<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.p.checked_add(n).ok_or("length overflow")?;
        let b = self.b.get(self.p..end).ok_or("truncated record")?;
        self.p = end;
        Ok(b)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn uv(&mut self) -> Result<u64> {
        let mut n = 0u64;
        for shift in (0..70).step_by(7) {
            let b = self.byte()?;
            if shift == 63 && b > 1 {
                return Err("varint overflow".into());
            }
            n |= ((b & 127) as u64) << shift;
            if b < 128 {
                if shift > 0 && b == 0 {
                    return Err("noncanonical varint".into());
                }
                return Ok(n);
            }
        }
        Err("varint overflow".into())
    }
    fn count(&mut self) -> Result<usize> {
        let n = usize::try_from(self.uv()?)?;
        if n > self.b.len() - self.p {
            return Err("length/count exceeds remaining bytes".into());
        }
        Ok(n)
    }
    fn blob(&mut self) -> Result<&'a [u8]> {
        let n = self.count()?;
        self.take(n)
    }
    fn text(&mut self) -> Result<String> {
        Ok(std::str::from_utf8(self.blob()?)?.to_owned())
    }
    fn int(&mut self) -> Result<i64> {
        let n = self.byte()? as usize;
        if !(1..=8).contains(&n) {
            return Err("integer width".into());
        }
        let b = self.take(n)?;
        let mut full = [if b[0] & 128 == 0 { 0 } else { 255 }; 8];
        full[8 - n..].copy_from_slice(b);
        Ok(i64::from_be_bytes(full))
    }
    fn float(&mut self) -> Result<f64> {
        let x = f64::from_le_bytes(self.take(8)?.try_into()?);
        if !x.is_finite() {
            return Err("nonfinite float".into());
        }
        Ok(x)
    }
    fn done(&self) -> Result<()> {
        if self.p != self.b.len() {
            return Err("trailing bytes".into());
        }
        Ok(())
    }
}
fn json_write(v: &Value, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    if depth > 64 {
        return Err("nesting limit".into());
    }
    match v {
        Value::Null => out.push(0),
        Value::Bool(false) => out.push(1),
        Value::Bool(true) => out.push(2),
        Value::Number(n) => {
            if let Some(x) = n.as_i64() {
                out.push(3);
                int(x, out);
            } else if let Some(x) = n.as_u64() {
                out.push(4);
                uv(x, out);
            } else {
                out.push(5);
                out.extend_from_slice(&n.as_f64().ok_or("number range")?.to_le_bytes());
            }
        }
        Value::String(s) => {
            out.push(6);
            blob(s.as_bytes(), out);
        }
        Value::Array(a) => {
            out.push(7);
            uv(a.len() as u64, out);
            for v in a {
                json_write(v, out, depth + 1)?;
            }
        }
        Value::Object(o) => {
            out.push(8);
            uv(o.len() as u64, out);
            for (k, v) in o {
                blob(k.as_bytes(), out);
                json_write(v, out, depth + 1)?;
            }
        }
    }
    Ok(())
}
fn json_read(r: &mut Read<'_>, depth: usize) -> Result<Value> {
    if depth > 64 {
        return Err("nesting limit".into());
    }
    Ok(match r.byte()? {
        0 => Value::Null,
        1 => Value::Bool(false),
        2 => Value::Bool(true),
        3 => Value::from(r.int()?),
        4 => Value::from(r.uv()?),
        5 => Value::from(r.float()?),
        6 => Value::String(r.text()?),
        7 => {
            let n = r.count()?;
            let mut a = Vec::new();
            for _ in 0..n {
                a.push(json_read(r, depth + 1)?);
            }
            Value::Array(a)
        }
        8 => {
            let n = r.count()?;
            let mut o = Map::new();
            let mut previous: Option<String> = None;
            for _ in 0..n {
                let k = r.text()?;
                if previous.as_ref().is_some_and(|p| p >= &k) {
                    return Err("unordered/duplicate object key".into());
                }
                previous = Some(k.clone());
                o.insert(k, json_read(r, depth + 1)?);
            }
            Value::Object(o)
        }
        _ => return Err("unknown binary JSON tag".into()),
    })
}
pub fn binary_json(v: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    json_write(v, &mut out, 0)?;
    Ok(out)
}
pub fn read_binary_json(b: &[u8]) -> Result<Value> {
    let mut r = Read { b, p: 0 };
    let v = json_read(&mut r, 0)?;
    r.done()?;
    Ok(v)
}
fn geo(v: &Value) -> Result<Geom> {
    let c = v.get("coordinates").ok_or("geometry coordinates")?.clone();
    if v.as_object().ok_or("geometry object")?.len() != 2 {
        return Err("geometry extensions unsupported by P0".into());
    }
    let g = match v
        .get("type")
        .and_then(Value::as_str)
        .ok_or("geometry type")?
    {
        "Point" => {
            let p: [f64; 2] = serde_json::from_value(c)?;
            Geom::Point(p[0], p[1])
        }
        "LineString" => Geom::LineString(serde_json::from_value(c)?),
        "Polygon" => Geom::Polygon(serde_json::from_value(c)?),
        "MultiPoint" => Geom::MultiPoint(serde_json::from_value(c)?),
        "MultiLineString" => Geom::MultiLineString(serde_json::from_value(c)?),
        "MultiPolygon" => Geom::MultiPolygon(serde_json::from_value(c)?),
        _ => return Err("unsupported geometry".into()),
    };
    // Kernel codec and spatial methods own detailed geometry semantics.
    if let Some((x0, x1, y0, y1)) = g.bbox() {
        if ![x0, x1, y0, y1].iter().all(|x| x.is_finite())
            || x0 < -180.0
            || x1 > 180.0
            || y0 < -90.0
            || y1 > 90.0
        {
            return Err("coordinate range".into());
        }
    }
    Ok(g)
}
fn geo_json(g: Geom) -> Value {
    match g {
        Geom::Point(x, y) => serde_json::json!({"type":"Point","coordinates":[x,y]}),
        Geom::LineString(c) => serde_json::json!({"type":"LineString","coordinates":c}),
        Geom::Polygon(c) => serde_json::json!({"type":"Polygon","coordinates":c}),
        Geom::MultiPoint(c) => serde_json::json!({"type":"MultiPoint","coordinates":c}),
        Geom::MultiLineString(c) => serde_json::json!({"type":"MultiLineString","coordinates":c}),
        Geom::MultiPolygon(c) => serde_json::json!({"type":"MultiPolygon","coordinates":c}),
    }
}
pub fn encode(layout: &Layout, doc: &Value) -> Result<Encoded> {
    layout.validate()?;
    let obj = doc.as_object().ok_or("entity must be an object")?;
    let mut out = Vec::new();
    uv(layout.id, &mut out);
    let at = out.len();
    out.resize(at + layout.fields.len().div_ceil(4), 0);
    let mut vectors = Vec::new();
    for (i, (name, kind)) in layout.fields.iter().enumerate() {
        let Some(v) = obj.get(name) else { continue };
        out[at + i / 4] |= if v.is_null() { 1 } else { 2 } << ((i % 4) * 2);
        if v.is_null() {
            continue;
        }
        match kind {
            Kind::Text => blob(v.as_str().ok_or("expected text")?.as_bytes(), &mut out),
            Kind::Int => int(v.as_i64().ok_or("expected signed integer")?, &mut out),
            Kind::Real => out.extend_from_slice(
                &v.as_f64()
                    .filter(|x| x.is_finite())
                    .ok_or("expected finite real")?
                    .to_le_bytes(),
            ),
            Kind::Bool => out.push(u8::from(v.as_bool().ok_or("expected bool")?)),
            Kind::Json => json_write(v, &mut out, 0)?,
            Kind::Geo => blob(&geo(v)?.encode(), &mut out),
            Kind::Point => {
                let Geom::Point(x, y) = geo(v)? else {
                    return Err("expected point geometry".into());
                };
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            Kind::Vector(dim) => {
                let a = v.as_array().ok_or("expected vector array")?;
                if a.len() != *dim {
                    return Err("vector dimensions".into());
                }
                let mut b = Vec::with_capacity(dim * 4);
                for x in a {
                    let x = x.as_f64().ok_or("vector coordinate")? as f32;
                    if !x.is_finite() {
                        return Err("nonfinite vector".into());
                    }
                    b.extend_from_slice(&x.to_le_bytes());
                }
                vectors.push((i, b));
            }
        }
    }
    let extra: Map<String, Value> = obj
        .iter()
        .filter(|(k, _)| !layout.fields.iter().any(|(n, _)| n == *k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    json_write(&Value::Object(extra), &mut out, 0)?;
    Ok(Encoded { row: out, vectors })
}
pub fn decode_inline(layout: &Layout, bytes: &[u8]) -> Result<(Value, Vec<usize>)> {
    let mut r = Read { b: bytes, p: 0 };
    if r.uv()? != layout.id {
        return Err("wrong layout".into());
    }
    let states = r.take(layout.fields.len().div_ceil(4))?;
    let mut obj = Map::new();
    let mut vectors = Vec::new();
    for (i, (name, kind)) in layout.fields.iter().enumerate() {
        match (states[i / 4] >> ((i % 4) * 2)) & 3 {
            0 => continue,
            1 => {
                obj.insert(name.clone(), Value::Null);
                continue;
            }
            2 => {}
            _ => return Err("invalid field state".into()),
        }
        let v = match kind {
            Kind::Text => Value::String(r.text()?),
            Kind::Int => Value::from(r.int()?),
            Kind::Real => Value::from(r.float()?),
            Kind::Bool => match r.byte()? {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                _ => return Err("boolean encoding".into()),
            },
            Kind::Json => json_read(&mut r, 0)?,
            Kind::Geo => geo_json(Geom::decode(r.blob()?).ok_or("invalid binary geometry")?),
            Kind::Point => geo_json(Geom::Point(r.float()?, r.float()?)),
            Kind::Vector(_) => {
                vectors.push(i);
                continue;
            }
        };
        obj.insert(name.clone(), v);
    }
    let extra = json_read(&mut r, 0)?;
    let extra = extra.as_object().ok_or("extras must be object")?;
    for (k, v) in extra {
        if layout.fields.iter().any(|(n, _)| n == k) {
            return Err("declared key in extras".into());
        }
        obj.insert(k.clone(), v.clone());
    }
    r.done()?;
    Ok((Value::Object(obj), vectors))
}
pub fn vector_json(b: &[u8], dim: usize) -> Result<Value> {
    if dim.checked_mul(4) != Some(b.len()) {
        return Err("vector byte length".into());
    }
    let mut a = Vec::with_capacity(dim);
    visit_vector(b, dim, |x| a.push(Value::from(x as f64)))?;
    Ok(Value::Array(a))
}
pub(crate) fn visit_vector(b: &[u8], dim: usize, mut visit: impl FnMut(f32)) -> Result<()> {
    if dim.checked_mul(4) != Some(b.len()) {
        return Err("vector byte length".into());
    }
    for b in b.chunks_exact(4) {
        let x = f32::from_le_bytes(b.try_into()?);
        if !x.is_finite() {
            return Err("nonfinite vector".into());
        }
        visit(x);
    }
    Ok(())
}
pub fn decode(
    layout: &Layout,
    bytes: &[u8],
    mut get: impl FnMut(usize) -> Result<Vec<u8>>,
) -> Result<Value> {
    let (mut v, refs) = decode_inline(layout, bytes)?;
    for i in refs {
        let (name, Kind::Vector(dim)) = &layout.fields[i] else {
            unreachable!()
        };
        v[name] = vector_json(&get(i)?, *dim)?;
    }
    Ok(v)
}
impl Layout {
    /// The same three checks as ever, without a heap allocation for a typical
    /// layout.
    ///
    /// This runs on the row-read path -- every single-field decode validates
    /// the layout first -- and the duplicate-name check used to build a
    /// `BTreeSet` of the names to do it, which is one tree-node allocation per
    /// column per row read. A layout of a handful of columns is compared
    /// pairwise instead: no allocation, and fewer comparisons than the set
    /// costs. The set is kept for a wide layout, where the quadratic scan
    /// would be the worse of the two.
    pub fn validate(&self) -> Result<()> {
        if self.fields.len() > 256 {
            return Err("P0 max 256 columns".into());
        }
        for (i, (name, kind)) in self.fields.iter().enumerate() {
            if matches!(kind,Kind::Vector(n) if *n==0 || *n>16384) {
                return Err("vector dimension limit".into());
            }
            if self.fields.len() <= 32 && self.fields[..i].iter().any(|(old, _)| old == name) {
                return Err("duplicate field".into());
            }
        }
        if self.fields.len() > 32 {
            let mut names = std::collections::BTreeSet::new();
            for (name, _) in &self.fields {
                if !names.insert(name) {
                    return Err("duplicate field".into());
                }
            }
        }
        Ok(())
    }
    pub fn descriptor(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut b = b"E4P0LAY\0".to_vec();
        uv(self.id, &mut b);
        uv(self.fields.len() as u64, &mut b);
        for (name, kind) in &self.fields {
            blob(name.as_bytes(), &mut b);
            b.push(match kind {
                Kind::Text => 0,
                Kind::Int => 1,
                Kind::Real => 2,
                Kind::Bool => 3,
                Kind::Json => 4,
                Kind::Geo => 5,
                Kind::Vector(_) => 6,
                Kind::Point => 7,
            });
            if let Kind::Vector(dim) = kind {
                uv(*dim as u64, &mut b);
            }
        }
        if b.len() > 2077 {
            return Err("P0 descriptor size limit".into());
        }
        b.resize(2077, 0);
        b.extend_from_slice(&crc32c::crc32c(&b).to_le_bytes());
        Ok(b)
    }
    pub fn from_descriptor(b: &[u8]) -> Result<Self> {
        if b.len() != 2081
            || &b[..8] != b"E4P0LAY\0"
            || crc32c::crc32c(&b[..2077]) != u32::from_le_bytes(b[2077..].try_into()?)
        {
            return Err("invalid descriptor checksum/magic/length".into());
        }
        let mut r = Read {
            b: &b[..2077],
            p: 8,
        };
        let id = r.uv()?;
        let n = r.count()?;
        if n > 256 {
            return Err("column limit".into());
        }
        let mut fields = Vec::new();
        for _ in 0..n {
            let name = r.text()?;
            let kind = match r.byte()? {
                0 => Kind::Text,
                1 => Kind::Int,
                2 => Kind::Real,
                3 => Kind::Bool,
                4 => Kind::Json,
                5 => Kind::Geo,
                6 => Kind::Vector(usize::try_from(r.uv()?)?),
                7 => Kind::Point,
                _ => return Err("unknown layout type".into()),
            };
            fields.push((name, kind));
        }
        if r.b[r.p..].iter().any(|b| *b != 0) {
            return Err("descriptor padding".into());
        }
        let l = Self { id, fields };
        l.validate()?;
        Ok(l)
    }
}

// Projection walks the wire without allocating skipped JSON containers/strings.
// Requests are (declared column name, nested object path); one per column.
pub fn project(
    layout: &Layout,
    bytes: &[u8],
    requests: &[(&str, &[&str])],
) -> Result<Vec<Option<Value>>> {
    let mut r = Read { b: bytes, p: 0 };
    if r.uv()? != layout.id {
        return Err("wrong layout".into());
    }
    let states = r.take(layout.fields.len().div_ceil(4))?;
    let mut output = vec![None; requests.len()];
    for (i, (name, kind)) in layout.fields.iter().enumerate() {
        let req = requests.iter().position(|(n, _)| *n == name);
        match (states[i / 4] >> ((i % 4) * 2)) & 3 {
            0 => continue,
            1 => {
                if let Some(j) = req {
                    output[j] = Some(Value::Null);
                }
                continue;
            }
            2 => {}
            _ => return Err("invalid state".into()),
        }
        if let Kind::Json = kind {
            if let Some(j) = req {
                output[j] = json_path(&mut r, requests[j].1, 0)?;
            } else {
                json_skip(&mut r, 0)?;
            }
            continue;
        }
        let value = match kind {
            Kind::Text => {
                let b = r.blob()?;
                let s = std::str::from_utf8(b)?;
                req.map(|_| Value::String(s.to_owned()))
            }
            Kind::Int => {
                let v = r.int()?;
                req.map(|_| Value::from(v))
            }
            Kind::Real => {
                let v = r.float()?;
                req.map(|_| Value::from(v))
            }
            Kind::Bool => {
                let b = r.byte()?;
                if b > 1 {
                    return Err("boolean encoding".into());
                }
                req.map(|_| Value::Bool(b != 0))
            }
            Kind::Geo => {
                let b = r.blob()?;
                if req.is_some() {
                    Some(geo_json(Geom::decode(b).ok_or("invalid geometry")?))
                } else {
                    None
                }
            }
            Kind::Point => {
                let x = r.float()?;
                let y = r.float()?;
                req.map(|_| geo_json(Geom::Point(x, y)))
            }
            Kind::Vector(_) => {
                if req.is_some() {
                    return Err("projection requires an external vector fetch".into());
                }
                None
            }
            Kind::Json => unreachable!(),
        };
        if let Some(j) = req {
            if !requests[j].1.is_empty() {
                return Err("nested projection requires JSON".into());
            }
            output[j] = value;
        }
    }
    json_skip(&mut r, 0)?;
    r.done()?;
    Ok(output)
}
fn json_skip(r: &mut Read<'_>, depth: usize) -> Result<()> {
    if depth > 64 {
        return Err("nesting limit".into());
    }
    match r.byte()? {
        0..=2 => {}
        3 => {
            r.int()?;
        }
        4 => {
            r.uv()?;
        }
        5 => {
            r.float()?;
        }
        6 => {
            std::str::from_utf8(r.blob()?)?;
        }
        7 => {
            let n = r.count()?;
            for _ in 0..n {
                json_skip(r, depth + 1)?;
            }
        }
        8 => {
            let n = r.count()?;
            let mut last: Option<&str> = None;
            for _ in 0..n {
                let k = std::str::from_utf8(r.blob()?)?;
                if last.is_some_and(|p| p >= k) {
                    return Err("unordered object".into());
                }
                last = Some(k);
                json_skip(r, depth + 1)?;
            }
        }
        _ => return Err("unknown JSON tag".into()),
    }
    Ok(())
}
fn json_path(r: &mut Read<'_>, path: &[&str], depth: usize) -> Result<Option<Value>> {
    if depth > 64 {
        return Err("nesting limit".into());
    }
    if path.is_empty() {
        return Ok(Some(json_read(r, depth)?));
    }
    if r.b.get(r.p) != Some(&8) {
        json_skip(r, depth)?;
        return Ok(None);
    }
    r.byte()?;
    let n = r.count()?;
    let mut out = None;
    let mut last: Option<&str> = None;
    for _ in 0..n {
        let k = std::str::from_utf8(r.blob()?)?;
        if last.is_some_and(|p| p >= k) {
            return Err("unordered object".into());
        }
        last = Some(k);
        if k == path[0] {
            out = json_path(r, &path[1..], depth + 1)?;
        } else {
            json_skip(r, depth + 1)?;
        }
    }
    Ok(out)
}

/// Encode directly into the dense-v3 wire representation.
pub fn encode_dense_v3(layout: &Layout, doc: &Value) -> Result<Encoded> {
    dense_v3::encode_direct(layout, doc)
}
/// Decode and validate dense-v3 without rebuilding earlier row formats.
pub fn decode_dense_v3(
    layout: &Layout,
    bytes: &[u8],
    get: impl FnMut(usize) -> Result<Vec<u8>>,
) -> Result<Value> {
    dense_v3::decode_direct(layout, bytes, get)
}
