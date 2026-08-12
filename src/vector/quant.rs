//! # Quantization — shrinking vectors from `f32` to `int8` to save RAM
//!
//! "Quantization" means storing each vector number in fewer bits, trading a
//! little precision for a lot of memory. Here each `f32` (4 bytes) becomes an
//! `int8` (1 byte) — 4× smaller — by mapping its value range onto -128..127.
//! The small int8 codes stay resident and drive the fast graph traversal; the
//! exact `f32` vectors live on disk and are only read to re-rank the final
//! candidates. This is the same split DiskANN / pgvector / Qdrant use.
//!
//! Scalar int8 quantization for **disk-first, low-RAM** vector search.
//!
//! The foundational design (same split as DiskANN / pgvector / Qdrant):
//!
//! - **int8 codes stay resident in RAM** (128 B / 128-d vector = 4× smaller than
//!   f32's 512 B) and drive HNSW graph traversal via a native SIMD int kernel.
//! - **full-precision f32 vectors live on disk** ([`VectorStore`](super::super::storage::vecstore))
//!   and are read back only to **re-rank** the handful of final candidates.
//!
//! This makes steady-state RAM ≈ `int8 codes + graph` (bounded, predictable) instead
//! of holding every f32 in RAM — without the page-cache-eviction tail-latency of a
//! pure mmap approach.
//!
//! ## Quantization
//!
//! A single **global** affine map (one `offset`, one `scale`) over the whole field,
//! calibrated to the **0.5 % / 99.5 % quantiles** (from a sample) so a few outliers
//! don't stretch the range and crush precision — the standard Qdrant/Lucene practice.
//!
//! ```text
//! code_i = clamp(round((x_i - offset) / scale), 0, 255)   as u8
//! x_i   ≈ offset + scale * code_i
//! ```
//!
//! For **L2** the squared distance in the quantized domain is
//! `scale² · Σ (a_i − b_i)²` over the u8 codes. `scale²` is a positive constant, so
//! for *ranking* during traversal we compare the raw integer `Σ (a_i − b_i)²` directly
//! (no float work on the hot path); the exact f32 re-rank fixes any quantization error.

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

use std::collections::HashMap;
use crate::vector::access::QuantAccess;

/// A calibrated global scalar quantizer for one vector field.
#[derive(Clone, Debug)]
pub struct ScalarQuantizer {
    /// Low end of the calibrated range (maps to code 0).
    pub offset: f32,
    /// Value covered by one code step: `(hi - lo) / 255`.
    pub scale: f32,
}

impl ScalarQuantizer {
    /// Calibrate from a sample of values using the 0.5 % / 99.5 % quantiles.
    ///
    /// `sample` may be any subset of the field's raw components; a few thousand
    /// is plenty. Falls back to a unit map if the sample is empty or degenerate.
    pub fn calibrate(sample: &mut [f32]) -> Self {
        if sample.is_empty() {
            return Self { offset: 0.0, scale: 1.0 };
        }
        sample.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sample.len();
        let lo = sample[((n as f64 * 0.005) as usize).min(n - 1)];
        let hi = sample[((n as f64 * 0.995) as usize).min(n - 1)];
        let span = (hi - lo).max(f32::MIN_POSITIVE);
        Self { offset: lo, scale: span / 255.0 }
    }

    /// Quantize a full-precision vector to u8 codes.
    #[inline]
    pub fn quantize(&self, v: &[f32]) -> Vec<u8> {
        let inv = 1.0 / self.scale;
        v.iter()
            .map(|&x| {
                let q = ((x - self.offset) * inv).round();
                q.clamp(0.0, 255.0) as u8
            })
            .collect()
    }
}

/// Ranking-only squared L2 between two u8 code vectors: `Σ (a_i − b_i)²` as `u32`.
///
/// Monotonic in the true (scaled) L2, so it orders candidates identically without
/// touching `scale²` — the exact f32 re-rank supplies real distances afterward.
#[inline]
pub fn l2_u8(a: &[u8], b: &[u8]) -> u32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return unsafe { l2_u8_avx2(a, b) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        return unsafe { l2_u8_neon(a, b) };
    }
    #[allow(unreachable_code)]
    l2_u8_scalar(a, b)
}

#[inline]
fn l2_u8_scalar(a: &[u8], b: &[u8]) -> u32 {
    let mut sum: u32 = 0;
    for i in 0..a.len() {
        let d = a[i] as i32 - b[i] as i32;
        sum += (d * d) as u32;
    }
    sum
}

/// AVX2 int8 L2: widen u8→i16 in 16-lane chunks, `madd(diff,diff)` → i32 accumulate.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn l2_u8_avx2(a: &[u8], b: &[u8]) -> u32 {
    let n = a.len();
    let mut acc = _mm256_setzero_si256();
    let mut i = 0;
    while i + 16 <= n {
        // Load 16 u8, zero-extend to 16×i16.
        let va = _mm256_cvtepu8_epi16(_mm_loadu_si128(a.as_ptr().add(i) as *const __m128i));
        let vb = _mm256_cvtepu8_epi16(_mm_loadu_si128(b.as_ptr().add(i) as *const __m128i));
        let diff = _mm256_sub_epi16(va, vb);
        // madd: pairs of i16 multiplied and summed into i32 lanes.
        let sq = _mm256_madd_epi16(diff, diff);
        acc = _mm256_add_epi32(acc, sq);
        i += 16;
    }
    // Horizontal sum of the 8 i32 lanes.
    let mut lanes = [0i32; 8];
    _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, acc);
    let mut total: u32 = lanes.iter().map(|&x| x as u32).sum();
    while i < n {
        let d = a[i] as i32 - b[i] as i32;
        total += (d * d) as u32;
        i += 1;
    }
    total
}

/// NEON int8 L2: `vabd_u8` (abs diff) → `vmull_u8` (square) → widening accumulate.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn l2_u8_neon(a: &[u8], b: &[u8]) -> u32 {
    use std::arch::aarch64::*;
    let n = a.len();
    let mut acc = vdupq_n_u32(0);
    let mut i = 0;
    while i + 8 <= n {
        let va = vld1_u8(a.as_ptr().add(i));
        let vb = vld1_u8(b.as_ptr().add(i));
        let d = vabd_u8(va, vb); // |a-b| as u8 (fits: 0..255)
        let sq = vmull_u8(d, d); // d² as u16×8
        acc = vpadalq_u16(acc, sq); // pairwise-add-accumulate into u32×4
        i += 8;
    }
    let mut total = vaddvq_u32(acc);
    while i < n {
        let dd = a[i] as i32 - b[i] as i32;
        total += (dd * dd) as u32;
        i += 1;
    }
    total
}

/// A field's int8 codes, **resident in RAM**, plus the quantizer that made them.
///
/// Codes are packed into one contiguous `Vec<u8>` (`dim` bytes per vector) so
/// traversal reads are cache-friendly; an `id → dense slot` map gives O(1) lookup.
/// Full-precision f32 for these ids lives on disk (the field's [`VectorStore`]).
///
/// RAM = `n · dim` bytes (e.g. 1M × 128 = 128 MB) + the id map — 4× smaller than
/// holding f32, and the *only* per-vector RAM cost of the disk-first index.
pub struct QuantizedField {
    pub quantizer: ScalarQuantizer,
    pub dim: usize,
    codes: Vec<u8>,
    id_to_slot: HashMap<u64, u32>,
}

impl QuantizedField {
    pub fn new(quantizer: ScalarQuantizer, dim: usize) -> Self {
        Self { quantizer, dim, codes: Vec::new(), id_to_slot: HashMap::new() }
    }

    /// Pre-size for `n` vectors (avoids reallocation during bulk quantize).
    pub fn with_capacity(quantizer: ScalarQuantizer, dim: usize, n: usize) -> Self {
        Self {
            quantizer,
            dim,
            codes: Vec::with_capacity(n * dim),
            id_to_slot: HashMap::with_capacity(n),
        }
    }

    /// Quantize `v` and store its codes under `id` (append, or overwrite in place).
    pub fn insert(&mut self, id: u64, v: &[f32]) {
        debug_assert_eq!(v.len(), self.dim);
        let code = self.quantizer.quantize(v);
        match self.id_to_slot.get(&id) {
            Some(&slot) => {
                let off = slot as usize * self.dim;
                self.codes[off..off + self.dim].copy_from_slice(&code);
            }
            None => {
                let slot = (self.codes.len() / self.dim) as u32;
                self.codes.extend_from_slice(&code);
                self.id_to_slot.insert(id, slot);
            }
        }
    }

    /// Quantize a raw query vector with this field's calibration.
    #[inline]
    pub fn quantize_query(&self, q: &[f32]) -> Vec<u8> {
        self.quantizer.quantize(q)
    }

    /// Bytes held resident in RAM (codes + id map) — for RAM accounting.
    pub fn mem_bytes(&self) -> usize {
        self.codes.capacity() + self.id_to_slot.capacity() * (8 + 4)
    }
}

impl QuantAccess for QuantizedField {
    #[inline]
    fn code(&self, id: u64) -> Option<&[u8]> {
        let &slot = self.id_to_slot.get(&id)?;
        let off = slot as usize * self.dim;
        Some(&self.codes[off..off + self.dim])
    }

    #[inline]
    fn len(&self) -> usize {
        self.id_to_slot.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_u8_matches_scalar() {
        let a: Vec<u8> = (0..130).map(|i| (i * 7 % 256) as u8).collect();
        let b: Vec<u8> = (0..130).map(|i| (i * 13 + 3) as u8).collect();
        assert_eq!(l2_u8(&a, &b), l2_u8_scalar(&a, &b));
    }

    #[test]
    fn quantize_roundtrip_is_close() {
        let q = ScalarQuantizer { offset: 0.0, scale: 1.0 };
        let v = vec![10.0, 200.0, 0.0, 255.0];
        let codes = q.quantize(&v);
        assert_eq!(codes, vec![10, 200, 0, 255]);
    }

    #[test]
    fn calibrate_clips_outliers() {
        // 1000 values in [0,100] plus one huge outlier — 99.5% quantile ignores it.
        let mut s: Vec<f32> = (0..1000).map(|i| i as f32 * 0.1).collect();
        s.push(1_000_000.0);
        let q = ScalarQuantizer::calibrate(&mut s);
        // range should be ~[0.5%..99.5%] of 0..99.9, not stretched to 1e6.
        assert!(q.scale < 1.0, "outlier stretched the scale: {}", q.scale);
    }

    #[test]
    fn ranking_is_monotonic_with_true_l2() {
        // Two codes at increasing distance rank the same way as their squared diff.
        let base = vec![100u8; 128];
        let near = vec![102u8; 128];
        let far = vec![150u8; 128];
        assert!(l2_u8(&base, &near) < l2_u8(&base, &far));
    }

    #[test]
    fn quantized_field_stores_and_reads_codes() {
        let q = ScalarQuantizer { offset: 0.0, scale: 1.0 };
        let mut f = QuantizedField::new(q, 4);
        f.insert(10, &[1.0, 2.0, 3.0, 4.0]);
        f.insert(20, &[5.0, 6.0, 7.0, 8.0]);
        assert_eq!(f.code(10), Some(&[1u8, 2, 3, 4][..]));
        assert_eq!(f.code(20), Some(&[5u8, 6, 7, 8][..]));
        assert_eq!(f.code(99), None);
        assert_eq!(f.len(), 2);
        // overwrite in place
        f.insert(10, &[9.0, 9.0, 9.0, 9.0]);
        assert_eq!(f.code(10), Some(&[9u8, 9, 9, 9][..]));
        assert_eq!(f.len(), 2);
    }
}
