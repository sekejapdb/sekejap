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

/// The lane loop, written once and specialised PER METRIC.
///
/// The loop this replaces kept three running sums -- `dot`, `stored_norm`
/// and `squared_l2` -- and added into all three on every lane, whatever the
/// metric asked for. Two of the three are dead on any given call: Cosine
/// reads `dot` and `stored_norm`, `NegativeDot` reads `dot`, `SquaredL2`
/// reads `squared_l2`. Each arm below keeps only the sums its own metric
/// reads.
///
/// THE VALUE IS BIT-FOR-BIT THE OLD ONE. Every retained sum accumulates the
/// same terms in the same lane order into the same single f64; what is gone
/// is arithmetic whose result was discarded. Nothing about the distance an
/// entry gets, the shortlist it enters or the answer a caller sees moves.
///
/// `POLL` is a const so the unfiltered page-order scan -- the one that never
/// cancels -- compiles with no cancellation test in the loop at all, while
/// the filtered probe keeps the one-per-256-lane cadence it had.
///
/// WHAT THIS IS NOT (Law 4). A 32-lane entry costs 24.4 ns to score on its
/// own and 24.0 ns with only the two sums Cosine reads, because ONE f64
/// accumulator makes the loop a chain of `dim` dependent additions and the
/// chain's LENGTH, not the op count, is what the processor waits on. Four
/// partial sums per accumulator break that chain and cost 16.2 ns measured
/// on their own -- and 134 ms against 128 ms per fifty 50,000-entry queries
/// measured INSIDE the scan, where the surrounding walk already hides the
/// chain and the extra accumulators only add register pressure. That
/// version also changes the approximate distance's last place, since f64
/// addition is not associative. It was measured and dropped; this one keeps
/// the arithmetic and takes the op count.
const POLL_LANES: usize = 256;
const LANE_STEP: usize = 8;

/// One metric's distance. See the note above for why there is an arm per
/// metric and why each arm keeps a single accumulator per sum.
#[inline(always)]
fn distance_of<const POLL: bool>(
    scale: f64,
    codes: &[u8],
    query: &[f64],
    query_norm: f64,
    metric: Metric,
    zero_query_is_an_error: bool,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<Option<f64>, Error> {
    let dim = codes.len();
    /// One lane's stored value: an exactly-representable int8 code times the
    /// entry's own f64 scale, which is one correctly-rounded multiply.
    macro_rules! stored {
        ($at:expr) => {
            f64::from(unsafe { *codes.get_unchecked($at) } as i8) * scale
        };
    }
    macro_rules! lane {
        ($at:expr) => {
            unsafe { *query.get_unchecked($at) }
        };
    }
    // The walk: eight-lane chunks then the remainder, with the poll on the
    // same 256-lane boundaries the single three-sum loop used.
    macro_rules! walk {
        ($one:expr) => {{
            let mut at = 0usize;
            while at + LANE_STEP <= dim {
                if POLL && at % POLL_LANES == 0 && cancelled() {
                    return Err(Error::Cancelled);
                }
                for j in 0..LANE_STEP {
                    $one(at + j);
                }
                at += LANE_STEP;
            }
            if POLL && at % POLL_LANES == 0 && at < dim && cancelled() {
                return Err(Error::Cancelled);
            }
            while at < dim {
                $one(at);
                at += 1;
            }
        }};
    }
    let distance = match metric {
        Metric::SquaredL2 => {
            let mut squared_l2 = 0.0f64;
            walk!(|at| {
                let difference = stored!(at) - lane!(at);
                squared_l2 += difference * difference;
            });
            squared_l2
        }
        Metric::NegativeDot => {
            let mut dot = 0.0f64;
            walk!(|at| {
                dot += stored!(at) * lane!(at);
            });
            -dot
        }
        Metric::Cosine => {
            let mut dot = 0.0f64;
            let mut stored_norm = 0.0f64;
            walk!(|at| {
                let stored = stored!(at);
                dot += stored * lane!(at);
                stored_norm += stored * stored;
            });
            if zero_query_is_an_error && query_norm == 0.0 {
                return Err(Error::Invalid("zero cosine query"));
            }
            if stored_norm == 0.0 {
                return Ok(None);
            }
            1.0 - dot / (stored_norm.sqrt() * query_norm.sqrt())
        }
    };
    Ok(Some(if distance == 0.0 { 0.0 } else { distance }))
}

/// Symmetric-int8 distance. `query` is already widened; `query_norm` is
/// `sum(q*q)` in lane order. Eight-lane chunks, no bounds checks in the hot
/// loop, one accumulator per sum the metric reads; see [`distance_of`].
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
    distance_of::<true>(scale, codes, query, query_norm, metric, true, &mut cancelled)
}

/// Same arithmetic as [`score_i8`], with no cancellation poll: `POLL` is
/// `false`, so the test is not in the compiled loop at all. The caller has
/// already widened the query and refused a zero cosine query; the one length
/// this asserts is the one the unchecked lane reads depend on.
#[inline(always)]
pub(crate) fn score_i8_hot(
    scale: f64,
    codes: &[u8],
    query: &[f64],
    query_norm: f64,
    metric: Metric,
) -> Option<f64> {
    // Both lane loops read `query` unchecked at offsets bounded by
    // `codes.len()`. A plain assert, not a debug one: the proof has to hold
    // for every caller, not only the builds with debug assertions on, and one
    // length compare per vector does not show up next to `dim` multiplies.
    assert_eq!(query.len(), codes.len(), "quantized score lane count");
    match distance_of::<false>(scale, codes, query, query_norm, metric, false, &mut || false) {
        Ok(distance) => distance,
        // `POLL` is false, so the one error this call can produce --
        // `Cancelled` -- has no way to be raised.
        Err(_) => unreachable!("the non-polling score cannot cancel"),
    }
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
