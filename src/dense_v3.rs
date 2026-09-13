//! Direct dense-v3 codec. The reference conversion path is retained only for tests.
use super::*;
pub(super) fn encode_direct(layout: &Layout, doc: &Value) -> Result<Encoded> {
    layout.validate()?;
    let obj = doc.as_object().ok_or("entity must be an object")?;
    if layout.id > u32::MAX as u64 {
        return Err("layout ID domain".into());
    }
    let has_states = layout
        .fields
        .iter()
        .any(|(n, _)| obj.get(n).is_none_or(Value::is_null));
    let extras: Vec<_> = obj
        .iter()
        .filter(|(k, _)| !layout.fields.iter().any(|(n, _)| n == *k))
        .collect();
    let mut out = Vec::new();
    uv(
        (layout.id << 2) | u64::from(has_states) | (u64::from(!extras.is_empty()) << 1),
        &mut out,
    );
    let at = out.len();
    if has_states {
        out.resize(at + layout.fields.len().div_ceil(4), 0);
    }
    let width_at = out.len();
    let integers = layout
        .fields
        .iter()
        .filter(|(_, k)| matches!(k, Kind::Int))
        .count();
    out.resize(width_at + (integers * 3).div_ceil(8), 0);
    let mut integer_index = 0;
    let mut vectors = Vec::new();
    for (i, (name, kind)) in layout.fields.iter().enumerate() {
        let width_index = integer_index;
        if matches!(kind, Kind::Int) {
            integer_index += 1;
        }
        let Some(v) = obj.get(name) else { continue };
        if has_states {
            out[at + i / 4] |= if v.is_null() { 1 } else { 2 } << ((i % 4) * 2);
        }
        if v.is_null() {
            continue;
        }
        match kind {
            Kind::Text => blob(v.as_str().ok_or("expected text")?.as_bytes(), &mut out),
            Kind::Int => {
                let bytes = v.as_i64().ok_or("expected signed integer")?.to_be_bytes();
                let mut start = 0;
                while start < 7
                    && ((bytes[start] == 0 && bytes[start + 1] & 128 == 0)
                        || (bytes[start] == 255 && bytes[start + 1] & 128 != 0))
                {
                    start += 1;
                }
                width_set(
                    &mut out[width_at..width_at + (integers * 3).div_ceil(8)],
                    width_index,
                    (7 - start) as u8,
                );
                out.extend_from_slice(&bytes[start..]);
            }
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
    if !extras.is_empty() {
        out.push(8);
        uv(extras.len() as u64, &mut out);
        for (key, value) in extras {
            blob(key.as_bytes(), &mut out);
            json_write(value, &mut out, 1)?;
        }
    }
    Ok(Encoded { row: out, vectors })
}
pub(super) fn decode_direct(
    layout: &Layout,
    bytes: &[u8],
    mut get: impl FnMut(usize) -> Result<Vec<u8>>,
) -> Result<Value> {
    decode_with_vector_values(layout, bytes, |i, dim| {
        Ok(Some(vector_json(&get(i)?, dim)?))
    })
}

// Mutation readers can validate and retain a sidecar without materializing
// thousands of JSON numbers that replacement/deletion will immediately discard.
pub(super) fn decode_with_vector_values(
    layout: &Layout,
    bytes: &[u8],
    mut get: impl FnMut(usize, usize) -> Result<Option<Value>>,
) -> Result<Value> {
    layout.validate()?;
    let mut r = Read { b: bytes, p: 0 };
    let h = r.uv()?;
    if h >> 2 > u32::MAX as u64 {
        return Err("layout ID domain".into());
    }
    if h >> 2 != layout.id {
        return Err("wrong layout".into());
    }
    let states = if h & 1 != 0 {
        Some(r.take(layout.fields.len().div_ceil(4))?)
    } else {
        None
    };
    let integers = layout
        .fields
        .iter()
        .filter(|(_, k)| matches!(k, Kind::Int))
        .count();
    let widths = r.take((integers * 3).div_ceil(8))?;
    let mut integer_index = 0;
    let mut obj = Map::new();
    let mut vectors = Vec::new();
    for (i, (name, kind)) in layout.fields.iter().enumerate() {
        let width_index = integer_index;
        if matches!(kind, Kind::Int) {
            integer_index += 1;
        }
        match states.map_or(2, |s| (s[i / 4] >> ((i % 4) * 2)) & 3) {
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
            Kind::Int => {
                let n = (width_get(widths, width_index) + 1) as usize;
                let bytes = r.take(n)?;
                let mut full = [if bytes[0] & 128 == 0 { 0 } else { 255 }; 8];
                full[8 - n..].copy_from_slice(bytes);
                Value::from(i64::from_be_bytes(full))
            }
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
    if h & 2 != 0 {
        let Value::Object(extra) = json_read(&mut r, 0)? else {
            return Err("extras must be object".into());
        };
        for (k, v) in extra {
            if layout.fields.iter().any(|(n, _)| n == &k) {
                return Err("declared key in extras".into());
            }
            obj.insert(k, v);
        }
    }
    r.done()?;
    // Validate all inline bytes before touching any external vector keyspace.
    for i in vectors {
        let (name, Kind::Vector(dim)) = &layout.fields[i] else {
            unreachable!()
        };
        if let Some(value) = get(i, *dim)? {
            obj.insert(name.clone(), value);
        }
    }
    Ok(Value::Object(obj))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_bytes_vectors_and_malformed_semantics_match_reference() {
        let integers = [
            i64::MIN,
            -(1_i64 << 48),
            -32769,
            -129,
            -128,
            -1,
            0,
            127,
            128,
            32768,
            1_i64 << 48,
            i64::MAX,
        ];
        for round in 0..96 {
            let mut fields: Vec<_> = integers
                .iter()
                .enumerate()
                .map(|(i, _)| (format!("i{i}"), Kind::Int))
                .collect();
            fields.extend([
                ("text".into(), Kind::Text),
                ("real".into(), Kind::Real),
                ("bool".into(), Kind::Bool),
                ("json".into(), Kind::Json),
                ("point".into(), Kind::Point),
                ("geo".into(), Kind::Geo),
                ("vector".into(), Kind::Vector(3)),
            ]);
            let layout = Layout {
                id: if round % 2 == 0 { 17 } else { u32::MAX as u64 },
                fields,
            };
            let mut doc = json!({"text":"hello λ","real":-0.0,"bool":true,"json":{"nested":[null,true,1.5,{"z":u64::MAX}]},"point":{"type":"Point","coordinates":[1.25,-2.5]},"geo":{"type":"LineString","coordinates":[[1.0,2.0],[3.0,4.0]]},"vector":[0.25,-1.0,4.0]});
            for (i, n) in integers.iter().enumerate() {
                doc[format!("i{i}")] = Value::from(*n);
            }
            if round % 3 == 0 {
                doc["extra"] = json!({"a":[1,2,3],"z":u64::MAX});
            }
            for (i, (name, _)) in layout.fields.iter().enumerate() {
                if round % 4 != 0 && (round + i) % 5 == 0 {
                    doc.as_object_mut().unwrap().remove(name);
                } else if round % 4 != 0 && (round + i) % 7 == 0 {
                    doc[name] = Value::Null;
                }
            }
            let old = encode_dense_v3_reference(&layout, &doc).unwrap();
            let new = encode_direct(&layout, &doc).unwrap();
            assert_eq!(new.row, old.row, "round {round}");
            assert_eq!(new.vectors, old.vectors);
            let fetch = |i| -> Result<Vec<u8>> {
                old.vectors
                    .iter()
                    .find(|(j, _)| *j == i)
                    .map(|(_, b)| b.clone())
                    .ok_or_else(|| "missing vector".into())
            };
            assert_eq!(
                decode_direct(&layout, &new.row, fetch).unwrap(),
                decode_dense_v3_reference(&layout, &old.row, fetch).unwrap()
            );
            assert_eq!(decode_direct(&layout, &new.row, fetch).unwrap(), doc);
            for mutation in 0..48 {
                let mut bytes = old.row.clone();
                let at = mutation * 17 % bytes.len();
                match mutation % 3 {
                    0 => bytes.truncate(at),
                    1 => bytes[at] ^= 1 << (mutation % 8),
                    _ => bytes.push(0),
                }
                let a = decode_direct(&layout, &bytes, fetch);
                let b = decode_dense_v3_reference(&layout, &bytes, fetch);
                assert_eq!(a.is_ok(), b.is_ok(), "round {round} mutation {mutation}");
                if let (Ok(a), Ok(b)) = (a, b) {
                    assert_eq!(a, b);
                }
            }
        }
    }

    #[test]
    fn malformed_inline_bytes_never_fetch_external_vectors() {
        let layout = Layout {
            id: 1,
            fields: vec![("v".into(), Kind::Vector(1))],
        };
        let mut row = encode_direct(&layout, &json!({"v":[1.0]})).unwrap().row;
        row.push(0);
        assert!(decode_direct(&layout, &row, |_| panic!(
            "invalid inline row fetched vector"
        ))
        .is_err());
    }
}
