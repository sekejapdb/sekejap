//! 2g: data-oblivious vector fingerprints (TurboQuant-style, arXiv 2504.19874).
//!
//! Beginner's map of what happens and why it needs NO training:
//!
//! 1. ROTATE the vector by a fixed random rotation. A rotation preserves all
//!    distances and dot products, but after it every coordinate of a unit
//!    vector looks like a small Gaussian: sigma = 1/sqrt(dim), regardless of
//!    what the data means. That statistical guarantee is the whole trick --
//!    it replaces the per-dataset codebook training PQ needs.
//! 2. QUANTIZE each rotated coordinate independently against a FIXED ladder
//!    of 2^BITS levels sized for that Gaussian. The recipe depends only on
//!    (dimension, seed); both live in the catalog, so any process can encode
//!    or decode any vector at any time, forever.
//! 3. Store the original LENGTH (one f32): the code approximates the vector's
//!    direction; the norm restores its scale.
//!
//! The rotation is a fast Walsh-Hadamard transform (FWHT) with seeded random
//! sign flips, ROUNDS times: O(d log d) instead of a d*d matrix multiply,
//! and zero bytes of stored matrix. FWHT needs a power-of-two width, so
//! vectors are zero-padded up to one (1536 -> 2048).
//!
//! Search math: dot(x, q) = |x| * dot(unit_code(x), rotate(q)) because
//! rotations preserve dots. One estimated dot yields L2, cosine and dot
//! scores alike. The estimate is APPROXIMATE -- callers oversample and
//! rescore against the exact f32 rows (D23), so approximation can only ever
//! cost a MISS, never a wrong distance in the final ranking.

/// Default code width. 4-bit = 16 levels, dim/2 bytes; 2-bit = 4 levels,
/// dim/4 bytes -- half the scan I/O and half the unpack work, the lever
/// that matters when the code keyspace outgrows the pool. Recorded per
/// store in the catalog; the recall gate decides the default.
pub const DEFAULT_BITS: usize = 2;
pub const MAX_LEVELS: usize = 16;
const ROUNDS: usize = 1;
/// Quantizer span in sigmas: rotated unit-vector coordinates scaled by
/// sqrt(d) are ~N(0,1); +-2.5 covers 98.8% of mass, the tails clamp.
const SPAN: f32 = 2.5;

/// Bytes of code per vector for a padded width (norm f32 NOT included).
pub fn code_len(padded: usize, bits: usize) -> usize { padded * bits / 8 }

pub fn pad_dim(dim: usize) -> usize {
    // Floor at 64: the estimate kernels consume whole 16-byte lanes, and
    // padded*bits/8 TRUNCATED TO ZERO for dim <= 2 at 2 bits -- every
    // set_vec of a tiny vector then indexed an empty code buffer and
    // panicked (found by e1's dim-2 integration tests). A 64-floor makes
    // every code an exact lane multiple at 1/2/4 bits.
    dim.next_power_of_two().max(64)
}

/// Split a u64 seed into one sign per (round, coordinate) via a tiny PRNG.
fn signs(seed: u64, round: usize, padded: usize) -> Vec<f32> {
    let mut s = seed ^ (round as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (0..padded).map(|_| {
        s ^= s << 13; s ^= s >> 7; s ^= s << 17;
        if s & 1 == 1 { 1.0 } else { -1.0 }
    }).collect()
}

/// In-place FWHT. Classic butterfly; unnormalised (we fold the 1/sqrt(n)
/// into a single scale at the end of the rotation).
fn fwht(v: &mut [f32]) {
    let n = v.len();
    let mut h = 1;
    while h < n {
        for block in v.chunks_exact_mut(2 * h) {
            let (a, b) = block.split_at_mut(h);
            // contiguous halves: this loop auto-vectorizes; the strided
            // v[j]/v[j+h] form did not (2k measured 49.6us/encode before).
            for j in 0..h {
                let (x, y) = (a[j], b[j]);
                a[j] = x + y;
                b[j] = x - y;
            }
        }
        h *= 2;
    }
}

/// The prepared recipe: sign vectors and scales are FIXED per (seed, width),
/// so they are computed once here, never per vector. Before this hoist the
/// encode path allocated fresh sign vectors and re-ran the PRNG for EVERY
/// vector -- measured 3.3x on the 250K ingest ladder; the hoist plus
/// ROUNDS 3 -> 1 (recall unchanged at 1.000 on both the gaussian and the
/// hostile sparse dataset; the identity-rotation mutation fails the sparse
/// case at 0.555) brought bulk 15.9s -> 8.6s.
pub struct Encoder {
    padded: usize,
    bits: usize,
    levels: usize,
    signs: Vec<Vec<f32>>,
    /// (1/sqrt(n))^ROUNDS folded into one multiply at the end.
    scale: f32,
    table: [f32; MAX_LEVELS],
}

impl Encoder {
    pub fn new(dim: usize, seed: u64, bits: usize) -> Encoder {
        let padded = pad_dim(dim);
        Encoder {
            padded,
            bits,
            levels: 1 << bits,
            signs: (0..ROUNDS).map(|r| signs(seed, r, padded)).collect(),
            scale: (1.0 / (padded as f32).sqrt()).powi(ROUNDS as i32),
            table: level_table(padded, bits),
        }
    }

    pub fn bits(&self) -> usize { self.bits }

    pub fn padded(&self) -> usize { self.padded }

    /// Rotate `x` (already padded) in place.
    pub fn rotate(&self, x: &mut [f32]) {
        for sg in &self.signs {
            for (xi, s) in x.iter_mut().zip(sg) { *xi *= s; }
            fwht(x);
        }
        for xi in x.iter_mut() { *xi *= self.scale; }
    }

    /// Encode: returns (norm, packed code) for a raw vector (dim <= padded).
    pub fn encode(&self, v: &[f32]) -> (f32, Vec<u8>) {
        let norm = v.iter().map(|a| a * a).sum::<f32>().sqrt();
        let mut x = vec![0f32; self.padded];
        if norm > 0.0 {
            for (xi, vi) in x.iter_mut().zip(v) { *xi = vi / norm; }
        }
        self.rotate(&mut x);
        let sd = (self.padded as f32).sqrt();
        let per = 8 / self.bits;
        let mut code = vec![0u8; self.padded * self.bits / 8];
        for (j, &xi) in x.iter().enumerate() {
            let z = (xi * sd).clamp(-SPAN, SPAN);
            let q = (((z + SPAN) / (2.0 * SPAN)) * (self.levels as f32 - 1.0)).round() as usize;
            let q = q.min(self.levels - 1) as u8;
            code[j / per] |= q << ((j % per) * self.bits);
        }
        (norm, code)
    }

    /// Pad + rotate a query once per search.
    pub fn rotate_query(&self, q: &[f32]) -> Vec<f32> {
        let mut x = vec![0f32; self.padded];
        x[..q.len()].copy_from_slice(q);
        self.rotate(&mut x);
        x
    }

}

/// The affine estimate (2g.2): because the quantizer ladder is UNIFORM,
/// level value v(l) = l * step - SPAN (in coordinate units), so
///   dot(x, q) ~ norm * ( step * SUM(l_j * q_j)  -  SPAN * SUM(q_j) ) / sd
/// The second sum is one constant per query; the first needs no table at
/// all -- unpack each nibble to an integer, convert, multiply-add. Every
/// step of that loop is straight-line arithmetic the compiler can
/// vectorize, unlike a table lookup which never vectorizes. Four
/// accumulators break the dependency chain as before.
pub struct AffineQuery {
    pub qrot: Vec<f32>,
    /// step / sqrt(padded), premultiplied.
    pub a: f32,
    /// -SPAN/sqrt(padded) * SUM(qrot), premultiplied.
    pub b: f32,
}

impl Encoder {
    pub fn affine_query(&self, q: &[f32]) -> AffineQuery {
        let qrot = self.rotate_query(q);
        let sd = (self.padded as f32).sqrt();
        let step = 2.0 * SPAN / (self.levels as f32 - 1.0);
        let sum_q: f32 = qrot.iter().sum();
        AffineQuery { qrot, a: step / sd, b: -SPAN / sd * sum_q }
    }
}

/// dot estimate via the affine form, 2-bit codes (4 coords per byte).
/// 256 x 4 lane-value table: row b = the four 2-bit lane values of byte b
/// as f32. 4KB, L1-resident; turns per-lane shift+convert (which never
/// vectorized -- 1592ns/code) into one contiguous 4-float load per byte.
static LANE4: [[f32; 4]; 256] = {
    let mut t = [[0f32; 4]; 256];
    let mut b = 0usize;
    while b < 256 {
        t[b] = [(b & 3) as f32, ((b >> 2) & 3) as f32,
                ((b >> 4) & 3) as f32, ((b >> 6) & 3) as f32];
        b += 1;
    }
    t
};

pub fn dot_est_affine2(norm: f32, code: &[u8], aq: &AffineQuery) -> f32 {
    let q = &aq.qrot;
    let (mut a0, mut a1, mut a2, mut a3) = (0f32, 0f32, 0f32, 0f32);
    // 16 code bytes = 64 lanes per iteration; both sides chunked exact so
    // the compiler sees fixed trip counts and no bounds checks (2k: the
    // per-byte form with open indexing measured 1993ns/code).
    let cb = code.chunks_exact(16);
    let rest = cb.remainder();
    let qb = q.chunks_exact(64);
    for (ch, qk) in cb.zip(qb) {
        for i in 0..16 {
            let l = &LANE4[ch[i] as usize];
            let base = i * 4;
            a0 += l[0] * qk[base];
            a1 += l[1] * qk[base + 1];
            a2 += l[2] * qk[base + 2];
            a3 += l[3] * qk[base + 3];
        }
    }
    let mut j = (code.len() - rest.len()) * 4;
    for &b in rest {
        a0 += (b & 3) as f32 * q[j];
        a1 += ((b >> 2) & 3) as f32 * q[j + 1];
        a2 += ((b >> 4) & 3) as f32 * q[j + 2];
        a3 += ((b >> 6) & 3) as f32 * q[j + 3];
        j += 4;
    }
    let s = (a0 + a1) + (a2 + a3);
    norm * (aq.a * s + aq.b)
}

/// dot estimate via the affine form, 4-bit codes (nibble-packed).
pub fn dot_est_affine(norm: f32, code: &[u8], aq: &AffineQuery) -> f32 {
    let q = &aq.qrot;
    let (mut a0, mut a1, mut a2, mut a3) = (0f32, 0f32, 0f32, 0f32);
    let chunks = code.chunks_exact(4);
    let rest = chunks.remainder();
    let mut j = 0usize;
    for ch in chunks {
        a0 += (ch[0] & 0x0F) as f32 * q[j]     + (ch[0] >> 4) as f32 * q[j + 1];
        a1 += (ch[1] & 0x0F) as f32 * q[j + 2] + (ch[1] >> 4) as f32 * q[j + 3];
        a2 += (ch[2] & 0x0F) as f32 * q[j + 4] + (ch[2] >> 4) as f32 * q[j + 5];
        a3 += (ch[3] & 0x0F) as f32 * q[j + 6] + (ch[3] >> 4) as f32 * q[j + 7];
        j += 8;
    }
    for &b in rest {
        a0 += (b & 0x0F) as f32 * q[j] + (b >> 4) as f32 * q[j + 1];
        j += 2;
    }
    let s = (a0 + a1) + (a2 + a3);
    norm * (aq.a * s + aq.b)
}


/// The 16 reconstruction values, in coordinate units (already divided by
/// sqrt(padded)): dequant(level) * sd = the ladder midpoint.
pub fn level_table(padded: usize, bits: usize) -> [f32; MAX_LEVELS] {
    let sd = (padded as f32).sqrt();
    let levels = 1 << bits;
    let mut t = [0f32; MAX_LEVELS];
    for l in 0..levels {
        let z = (l as f32) / (levels as f32 - 1.0) * (2.0 * SPAN) - SPAN;
        t[l] = z / sd;
    }
    t
}
