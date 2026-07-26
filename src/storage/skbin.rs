//! SKBIN — self-describing per-record binary payload encoding (Level 1).
//!
//! Replaces raw-JSON payload storage. Each record is encoded INDEPENDENTLY:
//! field *names* become integer IDs from a shared [`FieldTable`]; values are
//! typed (varint ints, f64, packed bool/null) but strings stay **literal in the
//! record**. Nothing is deduplicated across records — every value byte lives in
//! exactly one record, so a corrupt byte destroys at most that one record.
//!
//! The only shared state is the field-NAME table (structural metadata, the same
//! class as the offset index raw storage already depends on). Losing it costs
//! column *labels*, never *data*.
//!
//! Stored record layout: `[0x02][crc32 of body, LE u32][body]`. A CRC mismatch
//! on read fails that one record — corruption is detected, never silently served.

use std::collections::HashMap;

use serde_json::Value;

/// First byte of a SKBIN record. Raw JSON starts with `{` (0x7B) and legacy
/// zstd records with 0x01, so all three coexist in one store with zero migration.
pub const TAG_SKBIN: u8 = 0x02;

// Value tags (Level 1 — no interned-string / templated-string tags: those would
// put value data in shared state).
const T_NULL: u8 = 0;
const T_FALSE: u8 = 1;
const T_TRUE: u8 = 2;
const T_INT: u8 = 3;   // zigzag varint (i64)
const T_FLOAT: u8 = 4; // 8-byte IEEE-754 LE
const T_UINT: u8 = 5;  // plain varint (u64 > i64::MAX)
const T_STR: u8 = 6;   // varint len + utf8 bytes (ALWAYS literal)
const T_ARR: u8 = 7;   // varint count + values
const T_OBJ: u8 = 8;   // varint count + [varint field_id, value]*

/// Shared field-name table: `field_id <-> name`. IDs are append-only and never
/// reused, so a record encoded at any time decodes against any later table.
#[derive(Default, Clone)]
pub struct FieldTable {
    names: Vec<String>,
    ids: HashMap<String, u32>,
}

impl FieldTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern a field name, returning its stable id (appending if new).
    pub fn intern(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(name.to_string());
        self.ids.insert(name.to_string(), id);
        id
    }

    fn name(&self, id: u32) -> Option<&str> {
        self.names.get(id as usize).map(|s| s.as_str())
    }

    /// Reserved for the query-engine skip-scan path (`get_field`), not yet wired.
    #[allow(dead_code)]
    pub fn id_of(&self, name: &str) -> Option<u32> {
        self.ids.get(name).copied()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Serialize the table: `[varint count][varint len + utf8]*`. Tiny; written
    /// redundantly + checksummed by the storage layer.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_uv(&mut out, self.names.len() as u64);
        for n in &self.names {
            put_uv(&mut out, n.len() as u64);
            out.extend_from_slice(n.as_bytes());
        }
        out
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let mut p = 0;
        let count = get_uv(b, &mut p)? as usize;
        let mut t = FieldTable::new();
        for _ in 0..count {
            let l = get_uv(b, &mut p)? as usize;
            if p + l > b.len() {
                return None;
            }
            let name = std::str::from_utf8(&b[p..p + l]).ok()?.to_string();
            p += l;
            t.ids.insert(name.clone(), t.names.len() as u32);
            t.names.push(name);
        }
        Some(t)
    }
}

// ── varint ────────────────────────────────────────────────────────────────
fn put_uv(o: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            o.push(b);
            break;
        }
        o.push(b | 0x80);
    }
}
fn get_uv(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut s = 0u32;
    loop {
        let x = *b.get(*p)?;
        *p += 1;
        v |= ((x & 0x7f) as u64) << s;
        if x & 0x80 == 0 {
            break;
        }
        s += 7;
        if s >= 64 {
            return None;
        }
    }
    Some(v)
}
fn zig(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}
fn unzig(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

// ── encode ──────────────────────────────────────────────────────────────────

/// Encode a JSON value to a framed SKBIN record: `[0x02][crc32][body]`.
/// New field names are interned into `ft`.
pub fn encode(v: &Value, ft: &mut FieldTable) -> Vec<u8> {
    let mut body = Vec::new();
    enc_value(v, ft, &mut body);
    let crc = crc32fast::hash(&body);
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(TAG_SKBIN);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn enc_value(v: &Value, ft: &mut FieldTable, o: &mut Vec<u8>) {
    match v {
        Value::Null => o.push(T_NULL),
        Value::Bool(false) => o.push(T_FALSE),
        Value::Bool(true) => o.push(T_TRUE),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                o.push(T_INT);
                put_uv(o, zig(i));
            } else if let Some(u) = n.as_u64() {
                o.push(T_UINT);
                put_uv(o, u);
            } else {
                o.push(T_FLOAT);
                o.extend_from_slice(&n.as_f64().unwrap().to_le_bytes());
            }
        }
        Value::String(s) => {
            o.push(T_STR);
            put_uv(o, s.len() as u64);
            o.extend_from_slice(s.as_bytes());
        }
        Value::Array(a) => {
            o.push(T_ARR);
            put_uv(o, a.len() as u64);
            for e in a {
                enc_value(e, ft, o);
            }
        }
        Value::Object(m) => {
            o.push(T_OBJ);
            put_uv(o, m.len() as u64);
            for (k, val) in m {
                put_uv(o, ft.intern(k) as u64);
                enc_value(val, ft, o);
            }
        }
    }
}

// ── decode ──────────────────────────────────────────────────────────────────

/// Decode a framed SKBIN record. Returns `None` on CRC mismatch (corruption),
/// truncation, or an unknown field id — never garbage.
pub fn decode(rec: &[u8], ft: &FieldTable) -> Option<Value> {
    let body = verify(rec)?;
    let mut p = 0;
    let v = dec_value(body, &mut p, ft)?;
    Some(v)
}

/// Is this a SKBIN record? (first byte tag)
pub fn is_skbin(rec: &[u8]) -> bool {
    rec.first() == Some(&TAG_SKBIN)
}

// Strip + verify the `[0x02][crc32][body]` frame, returning the body.
fn verify(rec: &[u8]) -> Option<&[u8]> {
    if rec.len() < 5 || rec[0] != TAG_SKBIN {
        return None;
    }
    let crc = u32::from_le_bytes([rec[1], rec[2], rec[3], rec[4]]);
    let body = &rec[5..];
    if crc32fast::hash(body) != crc {
        return None; // corruption detected — do not serve
    }
    Some(body)
}

fn dec_value(b: &[u8], p: &mut usize, ft: &FieldTable) -> Option<Value> {
    let tag = *b.get(*p)?;
    *p += 1;
    Some(match tag {
        T_NULL => Value::Null,
        T_FALSE => Value::Bool(false),
        T_TRUE => Value::Bool(true),
        T_INT => Value::Number(unzig(get_uv(b, p)?).into()),
        T_UINT => Value::Number(get_uv(b, p)?.into()),
        T_FLOAT => {
            if *p + 8 > b.len() {
                return None;
            }
            let mut x = [0u8; 8];
            x.copy_from_slice(&b[*p..*p + 8]);
            *p += 8;
            serde_json::Number::from_f64(f64::from_le_bytes(x)).map(Value::Number)?
        }
        T_STR => {
            let l = get_uv(b, p)? as usize;
            if *p + l > b.len() {
                return None;
            }
            let s = std::str::from_utf8(&b[*p..*p + l]).ok()?.to_string();
            *p += l;
            Value::String(s)
        }
        T_ARR => {
            let n = get_uv(b, p)? as usize;
            let mut a = Vec::with_capacity(n);
            for _ in 0..n {
                a.push(dec_value(b, p, ft)?);
            }
            Value::Array(a)
        }
        T_OBJ => {
            let n = get_uv(b, p)? as usize;
            let mut m = serde_json::Map::new();
            for _ in 0..n {
                let fid = get_uv(b, p)? as u32;
                let name = ft.name(fid)?.to_string();
                m.insert(name, dec_value(b, p, ft)?);
            }
            Value::Object(m)
        }
        _ => return None,
    })
}

// ── skip-scan single-field access ─────────────────────────────────────────────

/// Fetch ONE top-level field by name without materializing the rest of the
/// record — the fast path for the query engine (WHERE / SELECT on a field).
/// Reserved: not yet wired into the query engine (follow-on perf increment).
#[allow(dead_code)]
pub fn get_field(rec: &[u8], name: &str, ft: &FieldTable) -> Option<Value> {
    let fid = ft.id_of(name)?;
    let body = verify(rec)?;
    let mut p = 0;
    if *body.get(p)? != T_OBJ {
        return None;
    }
    p += 1;
    let n = get_uv(body, &mut p)? as usize;
    for _ in 0..n {
        let f = get_uv(body, &mut p)? as u32;
        if f == fid {
            return dec_value(body, &mut p, ft);
        }
        skip_value(body, &mut p)?;
    }
    None
}

fn skip_value(b: &[u8], p: &mut usize) -> Option<()> {
    let tag = *b.get(*p)?;
    *p += 1;
    match tag {
        T_NULL | T_FALSE | T_TRUE => {}
        T_INT | T_UINT => {
            get_uv(b, p)?;
        }
        T_FLOAT => {
            if *p + 8 > b.len() {
                return None;
            }
            *p += 8;
        }
        T_STR => {
            let l = get_uv(b, p)? as usize;
            if *p + l > b.len() {
                return None;
            }
            *p += l;
        }
        T_ARR => {
            let n = get_uv(b, p)? as usize;
            for _ in 0..n {
                skip_value(b, p)?;
            }
        }
        T_OBJ => {
            let n = get_uv(b, p)? as usize;
            for _ in 0..n {
                get_uv(b, p)?;
                skip_value(b, p)?;
            }
        }
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip(v: Value) {
        let mut ft = FieldTable::new();
        let rec = encode(&v, &mut ft);
        assert_eq!(rec[0], TAG_SKBIN);
        let back = decode(&rec, &ft).expect("decode");
        assert_eq!(back, v, "roundtrip mismatch");
    }

    #[test]
    fn roundtrip_scalars_and_nesting() {
        roundtrip(json!(null));
        roundtrip(json!(true));
        roundtrip(json!(false));
        roundtrip(json!(0));
        roundtrip(json!(-42));
        roundtrip(json!(i64::MIN));
        roundtrip(json!(u64::MAX));
        roundtrip(json!(3.14159));
        roundtrip(json!(-0.0));
        roundtrip(json!("hello world"));
        roundtrip(json!(""));
        roundtrip(json!([1, "two", 3.0, true, null]));
        roundtrip(json!({"_collection":"order","_key":"ord-1","qty":3,"tags":["a","b"],"nested":{"x":1,"y":"z"}}));
    }

    #[test]
    fn realistic_record_roundtrips() {
        let v = json!({
            "_collection":"order","_key":"ord-0001a2","customer_id":"cust-000123",
            "email":"user5@example.com","quantity":3,"unit_price_cents":4599,
            "currency":"USD","status":"delivered","created_at":"2026-05-14T09:30:00Z",
            "notes":"handle with care","tags":["priority","fragile"],"late":false
        });
        roundtrip(v);
    }

    #[test]
    fn field_table_persists_and_ids_stay_stable() {
        let mut ft = FieldTable::new();
        let r1 = encode(&json!({"a":1,"b":2}), &mut ft);
        let r2 = encode(&json!({"b":9,"c":"x"}), &mut ft); // c is new
        let bytes = ft.to_bytes();
        let ft2 = FieldTable::from_bytes(&bytes).unwrap();
        // records decode against the reloaded table identically
        assert_eq!(decode(&r1, &ft2).unwrap(), json!({"a":1,"b":2}));
        assert_eq!(decode(&r2, &ft2).unwrap(), json!({"b":9,"c":"x"}));
    }

    #[test]
    fn skip_scan_field_access() {
        let mut ft = FieldTable::new();
        let rec = encode(&json!({"a":1,"status":"paid","big":[1,2,3],"z":9.5}), &mut ft);
        assert_eq!(get_field(&rec, "status", &ft), Some(json!("paid")));
        assert_eq!(get_field(&rec, "z", &ft), Some(json!(9.5)));
        assert_eq!(get_field(&rec, "a", &ft), Some(json!(1)));
        assert_eq!(get_field(&rec, "missing", &ft), None);
    }

    #[test]
    fn corruption_is_detected_not_served() {
        let mut ft = FieldTable::new();
        let mut rec = encode(&json!({"amount":100,"note":"ok"}), &mut ft);
        // flip a byte in the body → CRC must catch it
        let last = rec.len() - 1;
        rec[last] ^= 0xff;
        assert!(decode(&rec, &ft).is_none(), "corrupt record must not decode");
    }

    #[test]
    fn truncation_is_rejected() {
        let mut ft = FieldTable::new();
        let rec = encode(&json!({"a":"hello there"}), &mut ft);
        assert!(decode(&rec[..rec.len() - 3], &ft).is_none());
    }
}
