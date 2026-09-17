//! Version-1 scalar index keys. Lexicographic byte order is value order within
//! a declared scalar kind; missing and null share the first key. Text order is
//! binary UTF-8 order (no locale, case folding, or Unicode normalization).
use crate::collections::{Error, Result};
use crate::Kind;
use serde_json::Value;

const SIGN: u64 = 1 << 63;
const TEXT_LIMIT: usize = 1024;

fn tag(kind: &Kind) -> Option<u8> {
    match kind {
        Kind::Bool => Some(1),
        Kind::Int => Some(2),
        Kind::Real => Some(3),
        Kind::Text => Some(4),
        _ => None,
    }
}
fn invalid(message: &str) -> Error {
    Error::InvalidInput(message.into())
}
fn corrupt(message: &str) -> Error {
    Error::Corrupt(message.into())
}

pub(crate) fn encode(kind: &Kind, value: Option<&Value>) -> Result<Vec<u8>> {
    let tag = tag(kind).ok_or_else(|| invalid("scalar index requires Bool, Int, Real, or Text"))?;
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(vec![0]);
    };
    let mut out = vec![tag];
    match kind {
        Kind::Bool => out.push(u8::from(
            value
                .as_bool()
                .ok_or_else(|| invalid("scalar Bool value required"))?,
        )),
        Kind::Int => {
            let n = value
                .as_i64()
                .ok_or_else(|| invalid("scalar i64 value required"))?;
            out.extend_from_slice(&((n as u64) ^ SIGN).to_be_bytes());
        }
        Kind::Real => {
            let n = value
                .as_f64()
                .filter(|n| n.is_finite())
                .ok_or_else(|| invalid("finite scalar Real value required"))?;
            let bits = if n == 0.0 { 0 } else { n.to_bits() };
            let ordered = if bits & SIGN != 0 { !bits } else { bits ^ SIGN };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        Kind::Text => {
            let s = value
                .as_str()
                .ok_or_else(|| invalid("scalar Text value required"))?;
            if s.len() > TEXT_LIMIT {
                return Err(invalid("scalar Text index value exceeds 1024 UTF-8 bytes"));
            }
            for b in s.bytes() {
                out.push(b);
                if b == 0 {
                    out.push(255);
                }
            }
            out.extend_from_slice(&[0, 0]);
        }
        _ => unreachable!("kind checked above"),
    }
    Ok(out)
}

/// Decode one scalar prefix and return bytes consumed, leaving an entity-ID
/// suffix to the caller. Reject noncanonical encodings instead of admitting
/// multiple byte keys for the same value.
pub(crate) fn decode(kind: &Kind, bytes: &[u8]) -> Result<(Value, usize)> {
    let expected = tag(kind).ok_or_else(|| corrupt("unsupported scalar index kind"))?;
    let actual = *bytes
        .first()
        .ok_or_else(|| corrupt("missing scalar key tag"))?;
    if actual == 0 {
        return Ok((Value::Null, 1));
    }
    if actual != expected {
        return Err(corrupt("scalar key tag does not match declared kind"));
    }
    match kind {
        Kind::Bool => match bytes.get(1) {
            Some(0) => Ok((Value::Bool(false), 2)),
            Some(1) => Ok((Value::Bool(true), 2)),
            _ => Err(corrupt("invalid scalar Bool key")),
        },
        Kind::Int | Kind::Real => {
            let encoded = bytes
                .get(1..9)
                .ok_or_else(|| corrupt("truncated scalar numeric key"))?;
            let ordered = u64::from_be_bytes(encoded.try_into().unwrap());
            if matches!(kind, Kind::Int) {
                return Ok((Value::from((ordered ^ SIGN) as i64), 9));
            }
            let bits = if ordered & SIGN != 0 {
                ordered ^ SIGN
            } else {
                !ordered
            };
            let n = f64::from_bits(bits);
            if !n.is_finite() || bits == SIGN {
                return Err(corrupt("noncanonical or nonfinite scalar Real key"));
            }
            Ok((Value::from(n), 9))
        }
        Kind::Text => {
            let mut out = Vec::new();
            let mut at = 1;
            loop {
                let b = *bytes
                    .get(at)
                    .ok_or_else(|| corrupt("unterminated scalar Text key"))?;
                at += 1;
                if b == 0 {
                    match bytes.get(at) {
                        Some(0) => {
                            let s = String::from_utf8(out)
                                .map_err(|_| corrupt("invalid UTF-8 scalar key"))?;
                            return Ok((Value::String(s), at + 1));
                        }
                        Some(255) => at += 1,
                        _ => return Err(corrupt("invalid scalar Text escape")),
                    }
                }
                if out.len() == TEXT_LIMIT {
                    return Err(corrupt("scalar Text key exceeds 1024 UTF-8 bytes"));
                }
                out.push(b);
            }
        }
        _ => unreachable!("kind checked above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn check_order_and_roundtrip(kind: Kind, values: &[Value]) {
        let keys: Vec<_> = values
            .iter()
            .map(|v| encode(&kind, Some(v)).unwrap())
            .collect();
        for pair in keys.windows(2) {
            assert!(pair[0] < pair[1], "{pair:?}");
        }
        for (value, key) in values.iter().zip(keys) {
            let mut with_suffix = key.clone();
            with_suffix.extend_from_slice(&[0x81, 0x01]);
            assert_eq!(
                decode(&kind, &with_suffix).unwrap(),
                (value.clone(), key.len())
            );
        }
    }

    #[test]
    fn integers_and_booleans_keep_order_at_boundaries() {
        check_order_and_roundtrip(
            Kind::Int,
            &[
                Value::Null,
                json!(i64::MIN),
                json!(-1),
                json!(0),
                json!(1),
                json!(i64::MAX),
            ],
        );
        check_order_and_roundtrip(Kind::Bool, &[Value::Null, json!(false), json!(true)]);
        assert_eq!(
            encode(&Kind::Int, Some(&json!(0))).unwrap(),
            vec![2, 128, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn reals_keep_order_and_normalize_zero() {
        check_order_and_roundtrip(
            Kind::Real,
            &[
                Value::Null,
                json!(-f64::MAX),
                json!(-1.0),
                json!(-f64::from_bits(1)),
                json!(0.0),
                json!(f64::from_bits(1)),
                json!(1.0),
                json!(f64::MAX),
            ],
        );
        assert_eq!(
            encode(&Kind::Real, Some(&json!(-0.0))).unwrap(),
            encode(&Kind::Real, Some(&json!(0.0))).unwrap()
        );
        assert_eq!(
            encode(&Kind::Real, Some(&json!(42))).unwrap(),
            encode(&Kind::Real, Some(&json!(42.0))).unwrap()
        );
        for bits in [
            SIGN,
            f64::INFINITY.to_bits(),
            f64::NEG_INFINITY.to_bits(),
            f64::NAN.to_bits(),
        ] {
            let ordered = if bits & SIGN != 0 { !bits } else { bits ^ SIGN };
            let mut key = vec![3];
            key.extend_from_slice(&ordered.to_be_bytes());
            assert!(matches!(decode(&Kind::Real, &key), Err(Error::Corrupt(_))));
        }
    }

    #[test]
    fn utf8_nuls_prefixes_and_size_boundary() {
        check_order_and_roundtrip(
            Kind::Text,
            &[
                Value::Null,
                json!(""),
                json!("\0"),
                json!("\0\0"),
                json!("\0a"),
                json!("a"),
                json!("a\0"),
                json!("aa"),
                json!("é"),
                json!("😀"),
            ],
        );
        assert_eq!(
            encode(&Kind::Text, Some(&json!("a\0"))).unwrap(),
            vec![4, b'a', 0, 255, 0, 0]
        );
        for s in ["\0".repeat(1024), "é".repeat(512)] {
            let v = Value::String(s);
            let key = encode(&Kind::Text, Some(&v)).unwrap();
            assert_eq!(decode(&Kind::Text, &key).unwrap(), (v, key.len()));
        }
        assert!(encode(&Kind::Text, Some(&json!("é".repeat(513)))).is_err());
        let mut oversized = vec![4];
        oversized.extend(std::iter::repeat_n(b'a', 1025));
        oversized.extend_from_slice(&[0, 0]);
        assert!(decode(&Kind::Text, &oversized).is_err());
    }

    #[test]
    fn missing_null_and_strict_type_checks() {
        for kind in [Kind::Int, Kind::Real, Kind::Bool, Kind::Text] {
            assert_eq!(encode(&kind, None).unwrap(), vec![0]);
            assert_eq!(encode(&kind, Some(&Value::Null)).unwrap(), vec![0]);
        }
        for kind in [Kind::Json, Kind::Geo, Kind::Point, Kind::Vector(3)] {
            assert!(matches!(encode(&kind, None), Err(Error::InvalidInput(_))));
            assert!(decode(&kind, &[0]).is_err());
        }
        for (kind, value) in [
            (Kind::Int, json!(1.5)),
            (Kind::Int, json!(u64::MAX)),
            (Kind::Real, json!("1")),
            (Kind::Bool, json!(1)),
            (Kind::Text, json!(false)),
        ] {
            assert!(matches!(
                encode(&kind, Some(&value)),
                Err(Error::InvalidInput(_))
            ));
        }
    }

    #[test]
    fn malformed_keys_do_not_decode_as_valid_values() {
        for (kind, bytes) in [
            (Kind::Bool, vec![]),
            (Kind::Bool, vec![1]),
            (Kind::Bool, vec![1, 2]),
            (Kind::Int, vec![3, 0, 0, 0, 0, 0, 0, 0, 0]),
            (Kind::Int, vec![2, 0]),
            (Kind::Text, vec![4, 0]),
            (Kind::Text, vec![4, 0, 1]),
            (Kind::Text, vec![4, 255, 0, 0]),
            (Kind::Text, vec![4, b'a']),
        ] {
            assert!(matches!(decode(&kind, &bytes), Err(Error::Corrupt(_))));
        }
    }
}
