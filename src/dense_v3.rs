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

/// One selected logical field from a fully validated dense-v3 row. Vector
/// coordinates remain in their authoritative sidecar and are never fetched.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FieldValue {
    Missing,
    Null,
    Inline(Value),
    Vector { ordinal: usize, dimension: usize },
}

/// Validate the complete row while materializing only `field`. A field absent
/// from this immutable layout may still be present in its extras object; such a
/// numeric JSON array remains ordinary JSON rather than becoming a vector.
pub(crate) fn read_field(layout: &Layout, bytes: &[u8], field: &str) -> Result<FieldValue> {
    layout.validate()?;
    let declared = layout.fields.iter().position(|(name, _)| name == field);
    let mut selected = FieldValue::Missing;
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
        .filter(|(_, kind)| matches!(kind, Kind::Int))
        .count();
    let widths = r.take((integers * 3).div_ceil(8))?;
    let mut integer_index = 0;
    for (ordinal, (_, kind)) in layout.fields.iter().enumerate() {
        let width_index = integer_index;
        if matches!(kind, Kind::Int) {
            integer_index += 1;
        }
        let state = states.map_or(2, |states| (states[ordinal / 4] >> ((ordinal % 4) * 2)) & 3);
        match state {
            0 => continue,
            1 => {
                if declared == Some(ordinal) {
                    selected = FieldValue::Null;
                }
                continue;
            }
            2 => {}
            _ => return Err("invalid field state".into()),
        }
        let requested = declared == Some(ordinal);
        match kind {
            Kind::Text => {
                let value = std::str::from_utf8(r.blob()?)?;
                if requested {
                    selected = FieldValue::Inline(Value::String(value.to_owned()));
                }
            }
            Kind::Int => {
                let n = (width_get(widths, width_index) + 1) as usize;
                let bytes = r.take(n)?;
                if requested {
                    let mut full = [if bytes[0] & 128 == 0 { 0 } else { 255 }; 8];
                    full[8 - n..].copy_from_slice(bytes);
                    selected = FieldValue::Inline(Value::from(i64::from_be_bytes(full)));
                }
            }
            Kind::Real => {
                let value = r.float()?;
                if requested {
                    selected = FieldValue::Inline(Value::from(value));
                }
            }
            Kind::Bool => {
                let value = match r.byte()? {
                    0 => false,
                    1 => true,
                    _ => return Err("boolean encoding".into()),
                };
                if requested {
                    selected = FieldValue::Inline(Value::Bool(value));
                }
            }
            Kind::Json if requested => {
                selected = FieldValue::Inline(json_read(&mut r, 0)?);
            }
            Kind::Json => json_skip(&mut r, 0)?,
            Kind::Geo => {
                let geometry = Geom::decode(r.blob()?).ok_or("invalid binary geometry")?;
                if requested {
                    selected = FieldValue::Inline(geo_json(geometry));
                }
            }
            Kind::Point => {
                let point = Geom::Point(r.float()?, r.float()?);
                if requested {
                    selected = FieldValue::Inline(geo_json(point));
                }
            }
            Kind::Vector(dimension) => {
                if requested {
                    selected = FieldValue::Vector {
                        ordinal,
                        dimension: *dimension,
                    };
                }
            }
        }
    }
    if h & 2 != 0 {
        if r.byte()? != 8 {
            return Err("extras must be object".into());
        }
        let n = r.count()?;
        let mut previous: Option<&str> = None;
        for _ in 0..n {
            let key = std::str::from_utf8(r.blob()?)?;
            if previous.is_some_and(|old| old >= key) {
                return Err("unordered/duplicate object key".into());
            }
            if layout.fields.iter().any(|(name, _)| name == key) {
                return Err("declared key in extras".into());
            }
            previous = Some(key);
            if declared.is_none() && key == field {
                let value = json_read(&mut r, 1)?;
                selected = if value.is_null() {
                    FieldValue::Null
                } else {
                    FieldValue::Inline(value)
                };
            } else {
                json_skip(&mut r, 1)?;
            }
        }
    }
    r.done()?;
    Ok(selected)
}

/// Validate a complete dense-v3 row and locate one declared vector without
/// materializing its inline values or touching any external vector sidecar.
/// The returned ordinal is physical to this immutable layout.
pub(crate) fn locate_vector(
    layout: &Layout,
    bytes: &[u8],
    field: &str,
    dimension: usize,
) -> Result<Option<usize>> {
    layout.validate()?;
    let target: Result<Option<usize>> =
        match layout.fields.iter().position(|(name, _)| name == field) {
            None => Ok(None),
            Some(i) => match layout.fields[i].1 {
                Kind::Vector(dim) if dim == dimension => Ok(Some(i)),
                Kind::Vector(_) => Err("historical vector dimension mismatch".into()),
                _ => Err("historical vector field kind mismatch".into()),
            },
        };

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
        .filter(|(_, kind)| matches!(kind, Kind::Int))
        .count();
    let widths = r.take((integers * 3).div_ceil(8))?;
    let mut integer_index = 0;
    let mut found = None;
    for (i, (_, kind)) in layout.fields.iter().enumerate() {
        let width_index = integer_index;
        if matches!(kind, Kind::Int) {
            integer_index += 1;
        }
        match states.map_or(2, |s| (s[i / 4] >> ((i % 4) * 2)) & 3) {
            0 | 1 => continue,
            2 => {}
            _ => return Err("invalid field state".into()),
        }
        match kind {
            Kind::Text => {
                std::str::from_utf8(r.blob()?)?;
            }
            Kind::Int => {
                let n = (width_get(widths, width_index) + 1) as usize;
                r.take(n)?;
            }
            Kind::Real => {
                r.float()?;
            }
            Kind::Bool => match r.byte()? {
                0 | 1 => {}
                _ => return Err("boolean encoding".into()),
            },
            Kind::Json => json_skip(&mut r, 0)?,
            Kind::Geo => {
                Geom::decode(r.blob()?).ok_or("invalid binary geometry")?;
            }
            Kind::Point => {
                r.float()?;
                r.float()?;
            }
            Kind::Vector(_) => {
                if matches!(&target, Ok(Some(target)) if *target == i) {
                    found = Some(i);
                }
            }
        }
    }
    if h & 2 != 0 {
        if r.byte()? != 8 {
            return Err("extras must be object".into());
        }
        let n = r.count()?;
        let mut previous: Option<&str> = None;
        for _ in 0..n {
            let key = std::str::from_utf8(r.blob()?)?;
            if previous.is_some_and(|old| old >= key) {
                return Err("unordered/duplicate object key".into());
            }
            if layout.fields.iter().any(|(name, _)| name == key) {
                return Err("declared key in extras".into());
            }
            previous = Some(key);
            json_skip(&mut r, 1)?;
        }
    }
    r.done()?;
    target?;
    Ok(found)
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

    fn decode_ok(layout: &Layout, row: &[u8], vectors: &[(usize, Vec<u8>)]) -> bool {
        decode_direct(layout, row, |field| {
            vectors
                .iter()
                .find(|(candidate, _)| *candidate == field)
                .map(|(_, bytes)| bytes.clone())
                .ok_or_else(|| "missing test vector".into())
        })
        .is_ok()
    }

    fn assert_locator_matches_decode(
        layout: &Layout,
        row: &[u8],
        vectors: &[(usize, Vec<u8>)],
        field: &str,
        dimension: usize,
    ) {
        assert_eq!(
            locate_vector(layout, row, field, dimension).is_ok(),
            decode_ok(layout, row, vectors),
            "locator/decode acceptance differs for field {field}"
        );
    }

    fn inline_start(layout: &Layout, row: &[u8]) -> usize {
        let mut r = Read { b: row, p: 0 };
        let h = r.uv().unwrap();
        if h & 1 != 0 {
            r.take(layout.fields.len().div_ceil(4)).unwrap();
        }
        let integers = layout
            .fields
            .iter()
            .filter(|(_, kind)| matches!(kind, Kind::Int))
            .count();
        r.take((integers * 3).div_ceil(8)).unwrap();
        r.p
    }

    #[test]
    fn locate_vector_skips_every_inline_kind_and_extras() {
        let layout = Layout {
            id: 41,
            fields: vec![
                ("text".into(), Kind::Text),
                ("int".into(), Kind::Int),
                ("real".into(), Kind::Real),
                ("bool".into(), Kind::Bool),
                ("json".into(), Kind::Json),
                ("geo".into(), Kind::Geo),
                ("point".into(), Kind::Point),
                ("left".into(), Kind::Vector(2)),
                ("target".into(), Kind::Vector(3)),
                ("tail".into(), Kind::Text),
            ],
        };
        let document = json!({
            "text":"hello λ", "int":i64::MIN, "real":-0.0, "bool":true,
            "json":{"nested":[null,true,1.5,{"z":u64::MAX}]},
            "geo":{"type":"LineString","coordinates":[[1.0,2.0],[3.0,4.0]]},
            "point":{"type":"Point","coordinates":[144.5,-37.5]},
            "left":[1.0,2.0], "target":[3.0,4.0,5.0], "tail":"after vectors",
            "extra":{"array":[null,{"unicode":"雪🦀"}],"number":u64::MAX}
        });
        let encoded = encode_direct(&layout, &document).unwrap();
        assert_eq!(
            locate_vector(&layout, &encoded.row, "target", 3).unwrap(),
            Some(8)
        );
        assert_eq!(
            locate_vector(&layout, &encoded.row, "left", 2).unwrap(),
            Some(7)
        );
        assert_eq!(
            locate_vector(&layout, &encoded.row, "historical_absent", 3).unwrap(),
            None
        );
        assert_locator_matches_decode(&layout, &encoded.row, &encoded.vectors, "target", 3);
    }

    #[test]
    fn locate_vector_handles_missing_null_incompatible_and_reordered_layouts() {
        let layout = Layout {
            id: 51,
            fields: vec![
                ("other".into(), Kind::Vector(2)),
                ("target".into(), Kind::Vector(3)),
                ("tail".into(), Kind::Bool),
            ],
        };
        for document in [
            json!({"other":[1,2],"tail":true}),
            json!({"other":[1,2],"target":null,"tail":true}),
        ] {
            let encoded = encode_direct(&layout, &document).unwrap();
            assert_eq!(
                locate_vector(&layout, &encoded.row, "target", 3).unwrap(),
                None
            );
            assert_locator_matches_decode(&layout, &encoded.row, &encoded.vectors, "target", 3);
        }

        let old = Layout {
            id: 52,
            fields: vec![
                ("prefix".into(), Kind::Text),
                ("target".into(), Kind::Vector(3)),
                ("other".into(), Kind::Vector(2)),
            ],
        };
        let new = Layout {
            id: 53,
            fields: vec![
                ("other".into(), Kind::Vector(2)),
                ("flag".into(), Kind::Bool),
                ("target".into(), Kind::Vector(3)),
            ],
        };
        let old_row = encode_direct(
            &old,
            &json!({"prefix":"old","target":[1,2,3],"other":[4,5]}),
        )
        .unwrap();
        let new_row =
            encode_direct(&new, &json!({"other":[4,5],"flag":true,"target":[1,2,3]})).unwrap();
        assert_eq!(
            locate_vector(&old, &old_row.row, "target", 3).unwrap(),
            Some(1)
        );
        assert_eq!(
            locate_vector(&new, &new_row.row, "target", 3).unwrap(),
            Some(2)
        );

        let scalar = Layout {
            id: 54,
            fields: vec![("target".into(), Kind::Text), ("tail".into(), Kind::Bool)],
        };
        let scalar_row = encode_direct(&scalar, &json!({"target":"old","tail":true})).unwrap();
        assert!(locate_vector(&scalar, &scalar_row.row, "target", 3).is_err());
        let mut malformed_scalar = scalar_row.row;
        malformed_scalar.push(0);
        assert!(locate_vector(&scalar, &malformed_scalar, "target", 3)
            .unwrap_err()
            .to_string()
            .contains("trailing bytes"));

        let wrong_dimension = Layout {
            id: 55,
            fields: vec![("target".into(), Kind::Vector(2))],
        };
        let wrong_row = encode_direct(&wrong_dimension, &json!({"target":[1,2]})).unwrap();
        assert!(locate_vector(&wrong_dimension, &wrong_row.row, "target", 3).is_err());

        let absent = Layout {
            id: 56,
            fields: vec![("other".into(), Kind::Bool)],
        };
        let absent_row = encode_direct(&absent, &json!({"other":true,"target":[1,2,3]})).unwrap();
        assert_eq!(
            locate_vector(&absent, &absent_row.row, "target", 3).unwrap(),
            None
        );
    }

    #[test]
    fn locate_vector_rejects_truncated_or_trailing_tail_after_target() {
        let layout = Layout {
            id: 61,
            fields: vec![
                ("target".into(), Kind::Vector(3)),
                ("tail".into(), Kind::Json),
                ("text".into(), Kind::Text),
            ],
        };
        let encoded = encode_direct(
            &layout,
            &json!({
                "target":[1,2,3], "tail":{"deep":[1,true,null,{"x":"last"}]},
                "text":"after", "extra":{"more":[1,2,3]}
            }),
        )
        .unwrap();
        assert_eq!(
            locate_vector(&layout, &encoded.row, "target", 3).unwrap(),
            Some(0)
        );
        for end in 0..encoded.row.len() {
            assert!(
                locate_vector(&layout, &encoded.row[..end], "target", 3).is_err(),
                "truncated end {end} accepted"
            );
            assert_locator_matches_decode(
                &layout,
                &encoded.row[..end],
                &encoded.vectors,
                "target",
                3,
            );
        }
        let mut trailing = encoded.row.clone();
        trailing.push(0);
        assert!(locate_vector(&layout, &trailing, "target", 3).is_err());
        assert_locator_matches_decode(&layout, &trailing, &encoded.vectors, "target", 3);
    }

    #[test]
    fn locate_vector_matches_decoder_on_invalid_inline_values_and_extras() {
        let cases = [
            (Kind::Bool, json!(true), 1usize),
            (Kind::Real, json!(1.25), 8),
            (
                Kind::Point,
                json!({"type":"Point","coordinates":[1.0,2.0]}),
                16,
            ),
        ];
        for (case, (kind, value, width)) in cases.into_iter().enumerate() {
            let layout = Layout {
                id: 70 + case as u64,
                fields: vec![("target".into(), Kind::Vector(2)), ("value".into(), kind)],
            };
            let encoded = encode_direct(&layout, &json!({"target":[1,2],"value":value})).unwrap();
            let at = inline_start(&layout, &encoded.row);
            assert_eq!(encoded.row.len() - at, width);
            let mut invalid = encoded.row.clone();
            match case {
                0 => invalid[at] = 2,
                1 | 2 => invalid[at..at + 8].copy_from_slice(&f64::NAN.to_le_bytes()),
                _ => unreachable!(),
            }
            assert!(locate_vector(&layout, &invalid, "target", 2).is_err());
            assert_locator_matches_decode(&layout, &invalid, &encoded.vectors, "target", 2);
        }

        let extras_layout = Layout {
            id: 73,
            fields: vec![("target".into(), Kind::Vector(2))],
        };
        let extras = encode_direct(
            &extras_layout,
            &json!({"target":[1,2],"extra":{"nested":[1,true]}}),
        )
        .unwrap();
        let at = inline_start(&extras_layout, &extras.row);
        assert_eq!(extras.row[at], 8);
        let mut non_object = extras.row.clone();
        non_object[at] = 7;
        assert!(locate_vector(&extras_layout, &non_object, "target", 2).is_err());
        assert_locator_matches_decode(&extras_layout, &non_object, &extras.vectors, "target", 2);
    }

    #[test]
    fn read_field_returns_every_state_and_kind_without_fetching_vectors() {
        let layout = Layout {
            id: 81,
            fields: vec![
                ("text".into(), Kind::Text),
                ("int".into(), Kind::Int),
                ("real".into(), Kind::Real),
                ("bool".into(), Kind::Bool),
                ("json".into(), Kind::Json),
                ("geo".into(), Kind::Geo),
                ("point".into(), Kind::Point),
                ("left".into(), Kind::Vector(2)),
                ("target".into(), Kind::Vector(3)),
                ("vector_missing".into(), Kind::Vector(4)),
                ("vector_null".into(), Kind::Vector(4)),
                ("missing".into(), Kind::Text),
                ("null".into(), Kind::Json),
            ],
        };
        let document = json!({
            "text":"hello 雪", "int":-9007199254740993_i64, "real":1.25, "bool":true,
            "json":{"nested":[null,true,18446744073709551615_u64]},
            "geo":{"type":"LineString","coordinates":[[1.0,2.0],[3.0,4.0]]},
            "point":{"type":"Point","coordinates":[144.5,-37.5]},
            "left":[1.0,2.0], "target":[3.0,4.0,5.0], "vector_null":null, "null":null,
            "extra_array":[1,2,3], "extra_null":null,
            "extra_object":{"n":9007199254740993_u64}
        });
        let encoded = encode_direct(&layout, &document).unwrap();
        let expected = [
            ("text", FieldValue::Inline(json!("hello 雪"))),
            (
                "int",
                FieldValue::Inline(Value::from(-9007199254740993_i64)),
            ),
            ("real", FieldValue::Inline(json!(1.25))),
            ("bool", FieldValue::Inline(json!(true))),
            (
                "json",
                FieldValue::Inline(json!({"nested":[null,true,18446744073709551615_u64]})),
            ),
            (
                "geo",
                FieldValue::Inline(
                    json!({"type":"LineString","coordinates":[[1.0,2.0],[3.0,4.0]]}),
                ),
            ),
            (
                "point",
                FieldValue::Inline(json!({"type":"Point","coordinates":[144.5,-37.5]})),
            ),
            (
                "left",
                FieldValue::Vector {
                    ordinal: 7,
                    dimension: 2,
                },
            ),
            (
                "target",
                FieldValue::Vector {
                    ordinal: 8,
                    dimension: 3,
                },
            ),
            ("vector_missing", FieldValue::Missing),
            ("vector_null", FieldValue::Null),
            ("missing", FieldValue::Missing),
            ("null", FieldValue::Null),
            ("extra_array", FieldValue::Inline(json!([1, 2, 3]))),
            ("extra_null", FieldValue::Null),
            (
                "extra_object",
                FieldValue::Inline(json!({"n":9007199254740993_u64})),
            ),
            ("absent_everywhere", FieldValue::Missing),
        ];
        for (field, value) in expected {
            assert_eq!(read_field(&layout, &encoded.row, field).unwrap(), value);
        }
    }

    #[test]
    fn read_field_uses_each_immutable_layouts_physical_vector_ordinal() {
        let old = Layout {
            id: 82,
            fields: vec![
                ("prefix".into(), Kind::Text),
                ("embedding".into(), Kind::Vector(3)),
                ("other".into(), Kind::Vector(2)),
            ],
        };
        let reordered = Layout {
            id: 83,
            fields: vec![
                ("other".into(), Kind::Vector(2)),
                ("active".into(), Kind::Bool),
                ("embedding".into(), Kind::Vector(3)),
            ],
        };
        let historical = Layout {
            id: 84,
            fields: vec![("active".into(), Kind::Bool)],
        };
        let old_row = encode_direct(
            &old,
            &json!({"prefix":"old","embedding":[1,2,3],"other":[4,5]}),
        )
        .unwrap();
        let reordered_row = encode_direct(
            &reordered,
            &json!({"other":[4,5],"active":true,"embedding":[1,2,3]}),
        )
        .unwrap();
        let historical_row =
            encode_direct(&historical, &json!({"active":true,"embedding":[1,2,3]})).unwrap();

        assert_eq!(
            read_field(&old, &old_row.row, "embedding").unwrap(),
            FieldValue::Vector {
                ordinal: 1,
                dimension: 3
            }
        );
        assert_eq!(
            read_field(&reordered, &reordered_row.row, "embedding").unwrap(),
            FieldValue::Vector {
                ordinal: 2,
                dimension: 3
            }
        );
        assert_eq!(
            read_field(&historical, &historical_row.row, "embedding").unwrap(),
            FieldValue::Inline(json!([1, 2, 3]))
        );
        assert_eq!(
            locate_vector(&historical, &historical_row.row, "embedding", 3).unwrap(),
            None
        );
    }

    #[test]
    fn read_field_preserves_all_signed_integer_bits() {
        let values = [
            i64::MIN,
            -9007199254740993,
            -129,
            -1,
            0,
            127,
            9007199254740993,
            i64::MAX,
        ];
        let layout = Layout {
            id: 85,
            fields: values
                .iter()
                .enumerate()
                .map(|(i, _)| (format!("i{i}"), Kind::Int))
                .collect(),
        };
        let mut document = Map::new();
        for (i, value) in values.iter().enumerate() {
            document.insert(format!("i{i}"), Value::from(*value));
        }
        let encoded = encode_direct(&layout, &Value::Object(document)).unwrap();
        for (i, expected) in values.into_iter().enumerate() {
            let FieldValue::Inline(actual) =
                read_field(&layout, &encoded.row, &format!("i{i}")).unwrap()
            else {
                panic!("integer field did not produce an inline value")
            };
            assert_eq!(actual.as_i64(), Some(expected));
        }
    }

    #[test]
    fn read_field_validates_the_tail_after_an_early_match() {
        let layout = Layout {
            id: 86,
            fields: vec![
                ("early".into(), Kind::Text),
                ("tail".into(), Kind::Json),
                ("point".into(), Kind::Point),
            ],
        };
        let encoded = encode_direct(
            &layout,
            &json!({
                "early":"selected",
                "tail":{"deep":[1,true,null,{"last":"value"}]},
                "point":{"type":"Point","coordinates":[1.0,2.0]},
                "extra":{"more":[1,2,3]}
            }),
        )
        .unwrap();
        assert_eq!(
            read_field(&layout, &encoded.row, "early").unwrap(),
            FieldValue::Inline(json!("selected"))
        );
        for end in 0..encoded.row.len() {
            assert!(
                read_field(&layout, &encoded.row[..end], "early").is_err(),
                "truncated end {end} accepted"
            );
        }
        let mut trailing = encoded.row.clone();
        trailing.push(0);
        assert!(read_field(&layout, &trailing, "early").is_err());

        let mut unordered = Vec::new();
        uv((layout.id << 2) | 2, &mut unordered);
        blob(b"selected", &mut unordered);
        json_write(&json!({"ok":true}), &mut unordered, 0).unwrap();
        unordered.extend_from_slice(&1.0_f64.to_le_bytes());
        unordered.extend_from_slice(&2.0_f64.to_le_bytes());
        unordered.push(8);
        uv(2, &mut unordered);
        blob(b"z", &mut unordered);
        json_write(&json!(1), &mut unordered, 1).unwrap();
        blob(b"a", &mut unordered);
        json_write(&json!(2), &mut unordered, 1).unwrap();
        assert!(read_field(&layout, &unordered, "early").is_err());

        let mut collision = Vec::new();
        uv((layout.id << 2) | 2, &mut collision);
        blob(b"selected", &mut collision);
        json_write(&json!({"ok":true}), &mut collision, 0).unwrap();
        collision.extend_from_slice(&1.0_f64.to_le_bytes());
        collision.extend_from_slice(&2.0_f64.to_le_bytes());
        collision.push(8);
        uv(1, &mut collision);
        blob(b"early", &mut collision);
        json_write(&json!(2), &mut collision, 1).unwrap();
        assert!(read_field(&layout, &collision, "early").is_err());
    }

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
