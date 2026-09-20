//! Frozen symmetric-int8 codec for the opt-in persisted quantized-scan family.
//!
//! Each vector has its own f64 scale and signed int8 lanes. No training state
//! or corpus-dependent calibration is involved. Exact f32 sidecars remain the
//! authoritative values and must be used to rerank a bounded candidate set.

pub(crate) const MAX_DIMENSION: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Metric {
    Cosine,
    SquaredL2,
    NegativeDot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Invalid(&'static str),
    Cancelled,
}

fn dimension(dimension: usize) -> Result<(), Error> {
    if !(1..=MAX_DIMENSION).contains(&dimension) {
        return Err(Error::Invalid("quantized vector dimension"));
    }
    Ok(())
}

/// Bytes: scale:f64le, followed by dimension signed two's-complement lanes.
/// Scale is max(abs(original f32 lane))/127 evaluated in f64. Quantization
/// rounds halfway cases away from zero. A zero vector has positive-zero scale
/// and all-zero lanes. Nonzero vectors have at least one lane of magnitude127.
pub(crate) fn encode(lanes: &[f32]) -> Result<Vec<u8>, Error> {
    dimension(lanes.len())?;
    let mut maximum = 0.0f64;
    for &lane in lanes {
        if !lane.is_finite() {
            return Err(Error::Invalid("non-finite quantized source vector"));
        }
        maximum = maximum.max(f64::from(lane).abs());
    }
    let scale = maximum / 127.0;
    let mut bytes = Vec::with_capacity(8 + lanes.len());
    bytes.extend_from_slice(&scale.to_le_bytes());
    for &lane in lanes {
        let code = if scale == 0.0 {
            0
        } else {
            (f64::from(lane) / scale).round().clamp(-127.0, 127.0) as i8
        };
        bytes.push(code as u8);
    }
    Ok(bytes)
}

pub(crate) struct Decoded<'a> {
    pub(crate) scale: f64,
    pub(crate) codes: &'a [u8],
}

pub(crate) fn decode(bytes: &[u8], declared_dimension: usize) -> Result<Decoded<'_>, Error> {
    dimension(declared_dimension)?;
    if bytes.len() != 8 + declared_dimension {
        return Err(Error::Invalid("quantized vector length"));
    }
    let scale = f64::from_le_bytes(bytes[..8].try_into().unwrap());
    if !scale.is_finite() || scale < 0.0 || scale > f64::from(f32::MAX) / 127.0 {
        return Err(Error::Invalid("quantized vector scale"));
    }
    let codes = &bytes[8..];
    if scale == 0.0 {
        if scale.to_bits() != 0 || codes.iter().any(|&lane| lane != 0) {
            return Err(Error::Invalid("noncanonical zero quantized vector"));
        }
    } else {
        if codes.iter().any(|&lane| lane == 128)
            || !codes.iter().any(|&lane| lane == 127 || lane == 129)
        {
            return Err(Error::Invalid("noncanonical quantized lanes"));
        }
        if scale < f64::from(f32::from_bits(1)) / 127.0 {
            return Err(Error::Invalid("quantized scale below f32 source range"));
        }
    }
    Ok(Decoded { scale, codes })
}

/// Convert query lanes to f64 once per search so the int8 inner loop does
/// not repeat the widening on every compact entry.
pub(crate) fn widen_query(query: &[f32]) -> Result<(Vec<f64>, f64), Error> {
    let mut wide = Vec::with_capacity(query.len());
    let mut query_norm = 0.0f64;
    for &lane in query {
        if !lane.is_finite() {
            return Err(Error::Invalid("non-finite quantized query"));
        }
        let wide_lane = f64::from(lane);
        query_norm += wide_lane * wide_lane;
        wide.push(wide_lane);
    }
    Ok((wide, query_norm))
}

/// Symmetric-int8 distance. `query` is already widened; `query_norm` is
/// `sum(q*q)` in lane order. Eight-lane chunks, no bounds checks in the hot
/// loop. Arithmetic is still per-lane f64 so Cosine/L2/dot match the previous
/// sequential accumulation.
pub(crate) fn score_i8(
    scale: f64,
    codes: &[u8],
    query: &[f64],
    query_norm: f64,
    metric: Metric,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Option<f64>, Error> {
    if query.len() != codes.len() {
        return Err(Error::Invalid("quantized query dimension"));
    }
    let dim = codes.len();
    let mut dot = 0.0f64;
    let mut stored_norm = 0.0f64;
    let mut squared_l2 = 0.0f64;
    let mut at = 0usize;
    while at + 8 <= dim {
        if at % 256 == 0 && cancelled() {
            return Err(Error::Cancelled);
        }
        unsafe {
            for j in 0..8 {
                let stored = f64::from(*codes.get_unchecked(at + j) as i8) * scale;
                let query_lane = *query.get_unchecked(at + j);
                dot += stored * query_lane;
                stored_norm += stored * stored;
                let difference = stored - query_lane;
                squared_l2 += difference * difference;
            }
        }
        at += 8;
    }
    if at % 256 == 0 && at < dim && cancelled() {
        return Err(Error::Cancelled);
    }
    while at < dim {
        unsafe {
            let stored = f64::from(*codes.get_unchecked(at) as i8) * scale;
            let query_lane = *query.get_unchecked(at);
            dot += stored * query_lane;
            stored_norm += stored * stored;
            let difference = stored - query_lane;
            squared_l2 += difference * difference;
        }
        at += 1;
    }
    let distance = match metric {
        Metric::SquaredL2 => squared_l2,
        Metric::NegativeDot => -dot,
        Metric::Cosine if query_norm == 0.0 => {
            return Err(Error::Invalid("zero cosine query"));
        }
        Metric::Cosine if stored_norm == 0.0 => return Ok(None),
        Metric::Cosine => 1.0 - dot / (stored_norm.sqrt() * query_norm.sqrt()),
    };
    Ok(Some(if distance == 0.0 { 0.0 } else { distance }))
}

/// Same arithmetic as [`score_i8`], with no cancellation poll. The caller has
/// already widened the query; the one length this asserts is the one the
/// unchecked lane reads depend on.
#[inline(always)]
pub(crate) fn score_i8_hot(
    scale: f64,
    codes: &[u8],
    query: &[f64],
    query_norm: f64,
    metric: Metric,
) -> Option<f64> {
    let dim = codes.len();
    // Both lane loops read `query` unchecked at offsets bounded by
    // `codes.len()`. A plain assert, not a debug one: the proof has to hold
    // for every caller, not only the builds with debug assertions on, and one
    // length compare per vector does not show up next to `dim` multiplies.
    assert_eq!(query.len(), dim, "quantized score lane count");
    let mut dot = 0.0f64;
    let mut stored_norm = 0.0f64;
    let mut squared_l2 = 0.0f64;
    let mut at = 0usize;
    while at + 8 <= dim {
        unsafe {
            for j in 0..8 {
                let stored = f64::from(*codes.get_unchecked(at + j) as i8) * scale;
                let query_lane = *query.get_unchecked(at + j);
                dot += stored * query_lane;
                stored_norm += stored * stored;
                let difference = stored - query_lane;
                squared_l2 += difference * difference;
            }
        }
        at += 8;
    }
    while at < dim {
        unsafe {
            let stored = f64::from(*codes.get_unchecked(at) as i8) * scale;
            let query_lane = *query.get_unchecked(at);
            dot += stored * query_lane;
            stored_norm += stored * stored;
            let difference = stored - query_lane;
            squared_l2 += difference * difference;
        }
        at += 1;
    }
    let distance = match metric {
        Metric::SquaredL2 => squared_l2,
        Metric::NegativeDot => -dot,
        Metric::Cosine if stored_norm == 0.0 => return None,
        Metric::Cosine => 1.0 - dot / (stored_norm.sqrt() * query_norm.sqrt()),
    };
    Some(if distance == 0.0 { 0.0 } else { distance })
}

impl Decoded<'_> {
    /// Scores the decoded approximation, not the authoritative original.
    /// Cosine excludes stored zero vectors and refuses a zero query. A caller
    /// must label this approximate and exact-rerank its selected candidates.
    pub(crate) fn score(
        &self,
        query: &[f32],
        metric: Metric,
        cancelled: impl FnMut() -> bool,
    ) -> Result<Option<f64>, Error> {
        let (wide, query_norm) = widen_query(query)?;
        score_i8(self.scale, self.codes, &wide, query_norm, metric, cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_scale_lanes_and_halfway_rule() {
        let bytes = encode(&[127.0, -127.0, 0.5, -0.5, 1.5, -1.5]).unwrap();
        assert_eq!(bytes, [0, 0, 0, 0, 0, 0, 240, 63, 127, 129, 1, 255, 2, 254]);
        assert!(decode(&bytes, 6).is_ok());
        assert_eq!(encode(&[0.0, -0.0]).unwrap(), vec![0; 10]);
    }

    #[test]
    fn subnormal_and_maximum_f32_keep_finite_nonzero_scales() {
        for magnitude in [f32::from_bits(1), f32::MIN_POSITIVE, 1.0, f32::MAX] {
            let bytes = encode(&[magnitude, -magnitude, 0.0]).unwrap();
            let decoded = decode(&bytes, 3).unwrap();
            assert!(decoded.scale.is_finite() && decoded.scale > 0.0);
            assert_eq!(decoded.codes, [127, 129, 0]);
            let score = decoded
                .score(&[magnitude, -magnitude, 0.0], Metric::Cosine, || false)
                .unwrap()
                .unwrap();
            assert!(score.abs() < 1e-14);
        }
    }

    #[test]
    fn metric_goldens_and_zero_rules() {
        let bytes = encode(&[3.0, 0.0]).unwrap();
        let decoded = decode(&bytes, 2).unwrap();
        assert_eq!(
            decoded.score(&[0.0, 4.0], Metric::SquaredL2, || false),
            Ok(Some(25.0))
        );
        assert_eq!(
            decoded.score(&[0.0, 4.0], Metric::Cosine, || false),
            Ok(Some(1.0))
        );
        assert_eq!(
            decoded.score(&[2.0, 0.0], Metric::NegativeDot, || false),
            Ok(Some(-6.0))
        );
        assert!(decoded
            .score(&[0.0, 0.0], Metric::Cosine, || false)
            .is_err());
        let zero = encode(&[0.0, 0.0]).unwrap();
        let zero = decode(&zero, 2).unwrap();
        assert_eq!(zero.score(&[1.0, 0.0], Metric::Cosine, || false), Ok(None));
        assert_eq!(
            zero.score(&[1.0, 0.0], Metric::SquaredL2, || false),
            Ok(Some(1.0))
        );
    }

    #[test]
    fn quantization_error_is_bounded_per_lane() {
        let lanes: Vec<_> = (0..1536)
            .map(|i| ((i * 97 % 4099) as f32 - 2049.0) / 31.0)
            .collect();
        let bytes = encode(&lanes).unwrap();
        let decoded = decode(&bytes, lanes.len()).unwrap();
        for (&original, &code) in lanes.iter().zip(decoded.codes) {
            let reconstructed = f64::from(code as i8) * decoded.scale;
            assert!((f64::from(original) - reconstructed).abs() <= decoded.scale * 0.500000000001);
        }
        assert_eq!(bytes.len(), 1544);
    }

    #[test]
    fn malformed_values_and_invalid_inputs_are_rejected() {
        assert!(encode(&[]).is_err());
        assert!(encode(&vec![0.0; MAX_DIMENSION + 1]).is_err());
        assert!(encode(&[f32::NAN]).is_err());
        assert!(encode(&[f32::INFINITY]).is_err());
        let good = encode(&[1.0, 0.0]).unwrap();
        for scale in [-1.0, -0.0, f64::NAN, f64::INFINITY, f64::MAX] {
            let mut corrupt = good.clone();
            corrupt[..8].copy_from_slice(&scale.to_le_bytes());
            assert!(decode(&corrupt, 2).is_err());
        }
        for codes in [[128, 0], [1, 1], [0, 0]] {
            let mut corrupt = good.clone();
            corrupt[8..].copy_from_slice(&codes);
            assert!(decode(&corrupt, 2).is_err());
        }
        assert!(decode(&good, 1).is_err());
        let decoded = decode(&good, 2).unwrap();
        assert!(decoded
            .score(&[f32::NAN, 0.0], Metric::SquaredL2, || false)
            .is_err());
        assert!(decoded.score(&[1.0], Metric::SquaredL2, || false).is_err());
    }

    #[test]
    fn maximal_vector_scoring_checks_cancellation_in_chunks() {
        let lanes = vec![1.0; MAX_DIMENSION];
        let bytes = encode(&lanes).unwrap();
        let decoded = decode(&bytes, MAX_DIMENSION).unwrap();
        let mut calls = 0;
        assert_eq!(
            decoded.score(&lanes, Metric::SquaredL2, || {
                calls += 1;
                calls == 3
            }),
            Err(Error::Cancelled)
        );
        assert_eq!(calls, 3);
    }
}
