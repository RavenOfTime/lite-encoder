//! Inter prediction: motion compensation and motion vector prediction.
//!
//! Spec 8.4. Scope here follows what the test camera actually emits: P slices
//! only, one reference frame, no weighted prediction. That removes
//! bi-prediction, the implicit and explicit weighting modes, and B-slice
//! direct-mode derivation entirely — see the project notes for the
//! measurement. What remains is the interpolation filters and the median
//! motion vector predictor.
//!
//! # Why the filters look expensive
//!
//! H.264 predicts from *fractional* sample positions. Luma resolves to
//! quarter-sample accuracy: half-sample positions come from a six-tap filter
//! `(1, -5, 20, 20, -5, 1)`, and quarter positions from averaging a full and a
//! half position. Chroma resolves to eighth-sample accuracy with plain
//! bilinear weights. This is where a real decoder spends most of its time, and
//! in ffmpeg it is the single largest body of hand-written assembly.
//!
//! Filters run block-at-a-time: the source patch is gathered once, the
//! fractional position is resolved once, and each arm loops over in-bounds
//! patch samples. The spec's own per-sample wording is kept under
//! `#[cfg(test)]` and the block paths are checked against it.

/// A reference picture plane, with the bounds needed for edge clamping.
#[derive(Debug, Clone, Copy)]
pub struct Plane<'a> {
    pub data: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

impl Plane<'_> {
    /// Fetch a sample, clamping coordinates into the picture.
    ///
    /// Spec 8.4.2.2.1 requires exactly this: motion vectors may point outside
    /// the reference picture, and the edge sample is repeated when they do.
    /// Encoders rely on it, so it is a decoding rule rather than a safety net.
    #[inline]
    fn at(&self, x: i32, y: i32) -> i32 {
        let x = x.clamp(0, self.width as i32 - 1) as usize;
        let y = y.clamp(0, self.height as i32 - 1) as usize;
        self.data[y * self.stride + x] as i32
    }
}

#[inline]
fn clip1(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// The six-tap filter `(1, -5, 20, 20, -5, 1)`, unrounded and unshifted.
///
/// Kept unnormalised because the centre position applies it twice and rounds
/// only once at the end, at a different shift.
#[inline]
fn tap6(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32) -> i32 {
    a - 5 * b + 20 * c + 20 * d - 5 * e + f
}

/// Horizontal half-sample intermediate `b1`, spec 8-244.
///
/// Written per sample, as the spec writes it. [`predict_luma`] does not call
/// it — it filters a whole block at a time — so this and its two companions
/// exist to state the definition plainly and to be what the optimised path is
/// tested against.
#[cfg(test)]
fn half_h(p: &Plane, x: i32, y: i32) -> i32 {
    tap6(
        p.at(x - 2, y),
        p.at(x - 1, y),
        p.at(x, y),
        p.at(x + 1, y),
        p.at(x + 2, y),
        p.at(x + 3, y),
    )
}

/// Vertical half-sample intermediate `h1`, spec 8-245. See [`half_h`].
#[cfg(test)]
fn half_v(p: &Plane, x: i32, y: i32) -> i32 {
    tap6(
        p.at(x, y - 2),
        p.at(x, y - 1),
        p.at(x, y),
        p.at(x, y + 1),
        p.at(x, y + 2),
        p.at(x, y + 3),
    )
}

/// Centre half-sample intermediate `j1`, spec 8-246.
///
/// Applies the six-tap filter vertically to horizontal intermediates. The spec
/// notes the transpose gives the same value, which the tests check.
#[cfg(test)]
fn centre(p: &Plane, x: i32, y: i32) -> i32 {
    tap6(
        half_h(p, x, y - 2),
        half_h(p, x, y - 1),
        half_h(p, x, y),
        half_h(p, x, y + 1),
        half_h(p, x, y + 2),
        half_h(p, x, y + 3),
    )
}

/// Round and clip a single-pass six-tap result to a sample. Spec 8-247.
#[inline]
fn round_half(v: i32) -> i32 {
    clip1((v + 16) >> 5) as i32
}

/// Round and clip the double-pass centre result. Spec 8-248.
#[inline]
fn round_centre(v: i32) -> i32 {
    clip1((v + 512) >> 10) as i32
}

/// The six-tap filter reaches two samples back and three forward, so a
/// `bw` by `bh` block is predicted from a `bw + 5` by `bh + 5` patch.
const MARGIN: usize = 5;

/// The largest patch any luma partition needs: a 16x16 block plus the margin.
const MAX_PATCH: usize = (16 + MARGIN) * (16 + MARGIN);

/// The largest horizontal-intermediate buffer: one row per patch row, one
/// column per output column.
const MAX_ROWS: usize = (16 + MARGIN) * 16;

/// Copies the source patch out of the reference, clamping to its edges.
///
/// Gathering once is the whole point of the block-at-a-time structure. Every
/// fractional position reads overlapping taps, and the centre position reads
/// thirty-six source samples for each sample it produces; done per output
/// sample that is thirty-six clamped loads, and done per block it is one.
fn gather(p: &Plane, x0: i32, y0: i32, pw: usize, ph: usize, out: &mut [u8]) {
    // The common case by far: the patch lies wholly inside the reference, so
    // no coordinate needs clamping and each row is one copy.
    if x0 >= 0 && y0 >= 0 && (x0 as usize + pw) <= p.width && (y0 as usize + ph) <= p.height {
        for j in 0..ph {
            let start = (y0 as usize + j) * p.stride + x0 as usize;
            out[j * pw..(j + 1) * pw].copy_from_slice(&p.data[start..start + pw]);
        }
        return;
    }
    for j in 0..ph {
        let y = (y0 + j as i32).clamp(0, p.height as i32 - 1) as usize;
        let row = y * p.stride;
        for i in 0..pw {
            let x = (x0 + i as i32).clamp(0, p.width as i32 - 1) as usize;
            out[j * pw + i] = p.data[row + x];
        }
    }
}

/// Copies a block of full samples, for the integer-vector case.
fn copy_block(p: &Plane, x0: i32, y0: i32, bw: usize, bh: usize, out: &mut [u8]) {
    if x0 >= 0 && y0 >= 0 && (x0 as usize + bw) <= p.width && (y0 as usize + bh) <= p.height {
        for j in 0..bh {
            let start = (y0 as usize + j) * p.stride + x0 as usize;
            out[j * bw..(j + 1) * bw].copy_from_slice(&p.data[start..start + bw]);
        }
        return;
    }
    for j in 0..bh {
        for i in 0..bw {
            out[j * bw + i] = p.at(x0 + i as i32, y0 + j as i32) as u8;
        }
    }
}

/// Predict one luma block from a reference plane. Spec 8.4.2.2.1.
///
/// `x0`, `y0` are the block's position in the *current* picture, and `mv` is
/// in quarter-sample units. `out` receives `bw * bh` samples with stride `bw`.
///
/// The sixteen fractional positions are the spec's lettered grid: `(0, 0)` is
/// the full sample `G`, `(2, 2)` is the centre `j`, and the rest are half
/// positions or averages of two positions.
///
/// The fractional position is fixed for the whole block, so it is resolved
/// once here rather than per sample, and each arm runs its own loop over
/// source samples already gathered into a patch. The spec's own wording is
/// per sample — `half_h`, `half_v` and `centre` say it that way, and the
/// tests check this against them — but evaluating it that way re-reads and
/// re-filters the same taps for every neighbouring output.
pub fn predict_luma(
    reference: &Plane,
    x0: i32,
    y0: i32,
    mv: (i32, i32),
    bw: usize,
    bh: usize,
    out: &mut [u8],
) {
    let (mvx, mvy) = mv;
    // Arithmetic shift, not division: motion vectors are signed and the spec
    // floors toward negative infinity.
    let base_x = x0 + (mvx >> 2);
    let base_y = y0 + (mvy >> 2);
    let xfrac = (mvx & 3) as usize;
    let yfrac = (mvy & 3) as usize;

    if xfrac == 0 && yfrac == 0 {
        copy_block(reference, base_x, base_y, bw, bh, out);
        return;
    }

    let (pw, ph) = (bw + MARGIN, bh + MARGIN);
    let mut patch = [0u8; MAX_PATCH];
    gather(
        reference,
        base_x - 2,
        base_y - 2,
        pw,
        ph,
        &mut patch[..pw * ph],
    );

    // Patch coordinates of output sample `(i, j)`: the margin puts the full
    // sample `G` two rows down and two columns along.
    let full = |i: usize, j: usize| patch[(j + 2) * pw + (i + 2)] as i32;
    // Horizontal six-tap over patch row `r`, aligned to output column `i`.
    let horizontal = |r: usize, i: usize| {
        let b = r * pw + i;
        tap6(
            patch[b] as i32,
            patch[b + 1] as i32,
            patch[b + 2] as i32,
            patch[b + 3] as i32,
            patch[b + 4] as i32,
            patch[b + 5] as i32,
        )
    };
    // Vertical six-tap over patch column `c`, aligned to output row `j`.
    let vertical = |j: usize, c: usize| {
        tap6(
            patch[j * pw + c] as i32,
            patch[(j + 1) * pw + c] as i32,
            patch[(j + 2) * pw + c] as i32,
            patch[(j + 3) * pw + c] as i32,
            patch[(j + 4) * pw + c] as i32,
            patch[(j + 5) * pw + c] as i32,
        )
    };

    // The centre position filters vertically over unrounded horizontal
    // intermediates, so those are computed once for the whole patch and
    // shared down each column instead of six times per output sample.
    let needs_centre = matches!((xfrac, yfrac), (2, 1) | (2, 2) | (2, 3) | (1, 2) | (3, 2));
    let mut rows = [0i32; MAX_ROWS];
    if needs_centre {
        for r in 0..ph {
            for i in 0..bw {
                rows[r * bw + i] = horizontal(r, i);
            }
        }
    }
    let centre_at = |rows: &[i32], i: usize, j: usize| {
        tap6(
            rows[j * bw + i],
            rows[(j + 1) * bw + i],
            rows[(j + 2) * bw + i],
            rows[(j + 3) * bw + i],
            rows[(j + 4) * bw + i],
            rows[(j + 5) * bw + i],
        )
    };

    // One loop per fractional position, so the sixteen-way decision is taken
    // once for the block rather than once per sample.
    macro_rules! fill {
        ($value:expr) => {
            for j in 0..bh {
                for i in 0..bw {
                    let f = $value;
                    out[j * bw + i] = clip1(f(i, j));
                }
            }
        };
    }

    match (xfrac, yfrac) {
        (1, 0) => fill!(|i, j| (full(i, j) + round_half(horizontal(j + 2, i)) + 1) >> 1),
        (2, 0) => fill!(|i, j| round_half(horizontal(j + 2, i))),
        (3, 0) => fill!(|i, j| (full(i + 1, j) + round_half(horizontal(j + 2, i)) + 1) >> 1),

        (0, 1) => fill!(|i, j| (full(i, j) + round_half(vertical(j, i + 2)) + 1) >> 1),
        (0, 2) => fill!(|i, j| round_half(vertical(j, i + 2))),
        (0, 3) => fill!(|i, j| (full(i, j + 1) + round_half(vertical(j, i + 2)) + 1) >> 1),

        (2, 2) => fill!(|i, j| round_centre(centre_at(&rows, i, j))),
        (1, 2) => fill!(|i, j| {
            (round_half(vertical(j, i + 2)) + round_centre(centre_at(&rows, i, j)) + 1) >> 1
        }),
        (3, 2) => fill!(|i, j| {
            (round_centre(centre_at(&rows, i, j)) + round_half(vertical(j, i + 3)) + 1) >> 1
        }),
        (2, 1) => fill!(|i, j| {
            (round_half(horizontal(j + 2, i)) + round_centre(centre_at(&rows, i, j)) + 1) >> 1
        }),
        (2, 3) => fill!(|i, j| {
            (round_centre(centre_at(&rows, i, j)) + round_half(horizontal(j + 3, i)) + 1) >> 1
        }),

        // Diagonal quarter positions: average of the two adjacent half
        // positions, never involving the centre.
        (1, 1) => {
            fill!(
                |i, j| (round_half(horizontal(j + 2, i)) + round_half(vertical(j, i + 2)) + 1) >> 1
            )
        }
        (3, 1) => {
            fill!(
                |i, j| (round_half(horizontal(j + 2, i)) + round_half(vertical(j, i + 3)) + 1) >> 1
            )
        }
        (1, 3) => {
            fill!(
                |i, j| (round_half(vertical(j, i + 2)) + round_half(horizontal(j + 3, i)) + 1) >> 1
            )
        }
        (3, 3) => {
            fill!(
                |i, j| (round_half(vertical(j, i + 3)) + round_half(horizontal(j + 3, i)) + 1) >> 1
            )
        }

        _ => unreachable!("the integer position returned early"),
    }
}

/// The bilinear filter reads one sample past the block on each axis.
const CHROMA_MARGIN: usize = 1;

/// The largest patch any chroma partition needs: an 8x8 block plus the margin.
const MAX_CHROMA_PATCH: usize = (8 + CHROMA_MARGIN) * (8 + CHROMA_MARGIN);

/// Predict one chroma block for 4:2:0. Spec 8.4.2.2.2.
///
/// `mv` is the *luma* motion vector in quarter-sample units. Chroma is half
/// resolution in both axes, so the same vector addresses eighth-sample
/// positions in the chroma plane, which is why no scaling appears here and the
/// fractional mask is 7 rather than 3.
///
/// As with [`predict_luma`], the source patch is gathered once — here `bw + 1`
/// by `bh + 1`, since bilinear only reaches one sample past the block — the
/// weights are fixed for the whole block, and each output sample is four
/// in-bounds patch reads rather than four clamped loads against the reference.
pub fn predict_chroma(
    reference: &Plane,
    x0: i32,
    y0: i32,
    mv: (i32, i32),
    bw: usize,
    bh: usize,
    out: &mut [u8],
) {
    let (mvx, mvy) = mv;
    // Arithmetic shift: motion vectors are signed and the spec floors toward
    // negative infinity, same as the luma path.
    let base_x = x0 + (mvx >> 3);
    let base_y = y0 + (mvy >> 3);
    let xfrac = mvx & 7;
    let yfrac = mvy & 7;

    if xfrac == 0 && yfrac == 0 {
        copy_block(reference, base_x, base_y, bw, bh, out);
        return;
    }

    let (pw, ph) = (bw + CHROMA_MARGIN, bh + CHROMA_MARGIN);
    let mut patch = [0u8; MAX_CHROMA_PATCH];
    gather(reference, base_x, base_y, pw, ph, &mut patch[..pw * ph]);

    let w_a = (8 - xfrac) * (8 - yfrac);
    let w_b = xfrac * (8 - yfrac);
    let w_c = (8 - xfrac) * yfrac;
    let w_d = xfrac * yfrac;

    for j in 0..bh {
        for i in 0..bw {
            let a = patch[j * pw + i] as i32;
            let b = patch[j * pw + i + 1] as i32;
            let c = patch[(j + 1) * pw + i] as i32;
            let d = patch[(j + 1) * pw + i + 1] as i32;

            // Bilinear over eighths, spec 8-266. Weights sum to 64 and the
            // inputs are samples, so the rounded result is already in 0..=255.
            out[j * bw + i] = ((w_a * a + w_b * b + w_c * c + w_d * d + 32) >> 6) as u8;
        }
    }
}

// -- Motion vector prediction ---------------------------------------------

/// One neighbouring partition's motion state, for the predictor.
///
/// `ref_idx` of `-1` means the neighbour is unavailable or intra coded; the
/// spec treats both identically here, substituting a zero vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Neighbour {
    pub mv: (i32, i32),
    pub ref_idx: i8,
}

impl Neighbour {
    /// The substitute the spec uses for an unavailable or intra neighbour.
    pub const UNAVAILABLE: Neighbour = Neighbour {
        mv: (0, 0),
        ref_idx: -1,
    };
}

/// Which partition of a macroblock is being predicted.
///
/// The 16x8 and 8x16 shapes override the median rule with a directional one,
/// because a partition split across an edge usually continues the motion of
/// the neighbour on that side. Spec 8.4.1.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Partition {
    /// Any shape that uses the plain median rule.
    Other,
    /// Upper half of a 16x8 split.
    Top16x8,
    /// Lower half of a 16x8 split.
    Bottom16x8,
    /// Left half of an 8x16 split.
    Left8x16,
    /// Right half of an 8x16 split.
    Right8x16,
}

#[inline]
fn median(a: i32, b: i32, c: i32) -> i32 {
    a + b + c - a.min(b).min(c) - a.max(b).max(c)
}

/// Derive the motion vector predictor. Spec 8.4.1.3.
///
/// `a` is the left neighbour, `b` the above neighbour, and `c` the
/// above-right neighbour — or the above-left neighbour when above-right is
/// unavailable, a substitution the caller performs since it depends on
/// macroblock addressing.
pub fn predict_mv(
    a: Neighbour,
    b: Neighbour,
    c: Neighbour,
    ref_idx: i8,
    partition: Partition,
) -> (i32, i32) {
    // Directional overrides come first: when the neighbour on the split's own
    // side references the same picture, use it outright.
    match partition {
        Partition::Top16x8 if b.ref_idx == ref_idx => return b.mv,
        Partition::Bottom16x8 if a.ref_idx == ref_idx => return a.mv,
        Partition::Left8x16 if a.ref_idx == ref_idx => return a.mv,
        Partition::Right8x16 if c.ref_idx == ref_idx => return c.mv,
        _ => {}
    }

    // When both upper neighbours are missing but the left one is present, the
    // left neighbour stands in for all three, which makes the median trivially
    // equal to it. Spec 8.4.1.3.1.
    let (b, c) = if b.ref_idx < 0 && c.ref_idx < 0 && a.ref_idx >= 0 {
        (a, a)
    } else {
        (b, c)
    };

    // If exactly one neighbour references the same picture, it wins outright;
    // the median is only a fallback for the ambiguous cases.
    let matches = [a, b, c].map(|n| n.ref_idx == ref_idx);
    if matches.iter().filter(|&&m| m).count() == 1 {
        return if matches[0] {
            a.mv
        } else if matches[1] {
            b.mv
        } else {
            c.mv
        };
    }

    (
        median(a.mv.0, b.mv.0, c.mv.0),
        median(a.mv.1, b.mv.1, c.mv.1),
    )
}

/// Derive the motion vector for a P_Skip macroblock. Spec 8.4.1.1.
///
/// Skip is the common case in surveillance footage — most of the frame does
/// not move — so this path carries far more macroblocks than the rest of the
/// module combined.
///
/// `a_present` and `b_present` say whether the left and above *macroblocks*
/// exist and belong to this slice. That is a different question from whether
/// `a` and `b` carry motion: an intra neighbour is a perfectly present
/// macroblock that simply has no vector, and it yields
/// [`Neighbour::UNAVAILABLE`] here just as a missing one does. The spec
/// distinguishes them — only a *missing* neighbour forces the zero vector,
/// while an intra one falls through to the median — so the caller has to say
/// which it is.
pub fn predict_skip_mv(
    a: Neighbour,
    b: Neighbour,
    c: Neighbour,
    a_present: bool,
    b_present: bool,
) -> (i32, i32) {
    // A missing neighbour, or an immediately-adjacent one that is itself
    // stationary against reference 0, forces a zero vector rather than a
    // median. This is what makes static scenes collapse to almost no bits.
    if !a_present
        || !b_present
        || (a.ref_idx == 0 && a.mv == (0, 0))
        || (b.ref_idx == 0 && b.mv == (0, 0))
    {
        return (0, 0);
    }
    predict_mv(a, b, c, 0, Partition::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_plane(v: u8, w: usize, h: usize) -> Vec<u8> {
        vec![v; w * h]
    }

    fn plane<'a>(data: &'a [u8], w: usize, h: usize) -> Plane<'a> {
        Plane {
            data,
            width: w,
            height: h,
            stride: w,
        }
    }

    /// The spec's own per-sample wording, transcribed straight from 8.4.2.2.1.
    ///
    /// Kept as a test fixture rather than as the implementation: it is the
    /// definition [`predict_luma`] has to match, and it is obviously correct
    /// in a way that a block-at-a-time filter with a gathered patch and shared
    /// intermediates is not.
    fn per_sample(p: &Plane, x: i32, y: i32, xfrac: usize, yfrac: usize) -> u8 {
        let v = match (xfrac, yfrac) {
            (0, 0) => p.at(x, y),

            (2, 0) => round_half(half_h(p, x, y)),
            (1, 0) => (p.at(x, y) + round_half(half_h(p, x, y)) + 1) >> 1,
            (3, 0) => (p.at(x + 1, y) + round_half(half_h(p, x, y)) + 1) >> 1,

            (0, 2) => round_half(half_v(p, x, y)),
            (0, 1) => (p.at(x, y) + round_half(half_v(p, x, y)) + 1) >> 1,
            (0, 3) => (p.at(x, y + 1) + round_half(half_v(p, x, y)) + 1) >> 1,

            (2, 2) => round_centre(centre(p, x, y)),
            (1, 2) => (round_half(half_v(p, x, y)) + round_centre(centre(p, x, y)) + 1) >> 1,
            (3, 2) => (round_centre(centre(p, x, y)) + round_half(half_v(p, x + 1, y)) + 1) >> 1,
            (2, 1) => (round_half(half_h(p, x, y)) + round_centre(centre(p, x, y)) + 1) >> 1,
            (2, 3) => (round_centre(centre(p, x, y)) + round_half(half_h(p, x, y + 1)) + 1) >> 1,

            (1, 1) => (round_half(half_h(p, x, y)) + round_half(half_v(p, x, y)) + 1) >> 1,
            (3, 1) => (round_half(half_h(p, x, y)) + round_half(half_v(p, x + 1, y)) + 1) >> 1,
            (1, 3) => (round_half(half_v(p, x, y)) + round_half(half_h(p, x, y + 1)) + 1) >> 1,
            (3, 3) => (round_half(half_v(p, x + 1, y)) + round_half(half_h(p, x, y + 1)) + 1) >> 1,

            _ => unreachable!(),
        };
        clip1(v)
    }

    /// The block filter must agree with that definition sample for sample, at
    /// every fractional position and every partition shape.
    ///
    /// Noise, not a gradient: a smooth reference makes wrong tap weights and
    /// transposed positions produce nearly the right answer, which is exactly
    /// the class of bug this guards. Several of the positions deliberately
    /// hang off the edge of the reference, where the gathered patch has to
    /// reproduce the spec's edge clamping.
    #[test]
    fn block_interpolation_matches_the_per_sample_definition() {
        let (w, h) = (48usize, 40usize);
        let mut data = vec![0u8; w * h];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for v in &mut data {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *v = (seed >> 33) as u8;
        }
        let p = plane(&data, w, h);

        for (bw, bh) in [(16, 16), (16, 8), (8, 16), (8, 8), (8, 4), (4, 8), (4, 4)] {
            for (x0, y0) in [
                (8, 8),
                (0, 0),
                (w as i32 - bw as i32, h as i32 - bh as i32),
                (-3, 6),
                (6, -3),
                (w as i32 - 2, h as i32 - 2),
            ] {
                // Spans every fractional position, with the integer part both
                // negative and positive so the arithmetic shift is exercised.
                for mvy in -6..=6 {
                    for mvx in -6..=6 {
                        let mut out = vec![0u8; bw * bh];
                        predict_luma(&p, x0, y0, (mvx, mvy), bw, bh, &mut out);
                        for j in 0..bh {
                            for i in 0..bw {
                                let x = x0 + (mvx >> 2) + i as i32;
                                let y = y0 + (mvy >> 2) + j as i32;
                                let want =
                                    per_sample(&p, x, y, (mvx & 3) as usize, (mvy & 3) as usize);
                                assert_eq!(
                                    out[j * bw + i],
                                    want,
                                    "{bw}x{bh} at ({x0}, {y0}) mv ({mvx}, {mvy}) sample ({i}, {j})"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Spec 8.4.2.2.2 per sample: four clamped loads and the bilinear weights.
    ///
    /// Same role as [`per_sample`] for luma — the definition the block path
    /// has to match, including at the picture edge where the gathered patch
    /// must reproduce the clamps.
    fn per_sample_chroma(p: &Plane, x: i32, y: i32, xfrac: i32, yfrac: i32) -> u8 {
        let a = p.at(x, y);
        let b = p.at(x + 1, y);
        let c = p.at(x, y + 1);
        let d = p.at(x + 1, y + 1);
        let v = ((8 - xfrac) * (8 - yfrac) * a
            + xfrac * (8 - yfrac) * b
            + (8 - xfrac) * yfrac * c
            + xfrac * yfrac * d
            + 32)
            >> 6;
        clip1(v)
    }

    /// Chroma counterpart of [`block_interpolation_matches_the_per_sample_definition`].
    ///
    /// Eighth-sample vectors instead of quarter, and the partitions chroma
    /// actually sees (the inter path predicts 4x4 blocks; 8x8 covers a whole
    /// chroma macroblock). Noise and edge-hanging positions for the same
    /// reasons as the luma test.
    #[test]
    fn chroma_block_interpolation_matches_the_per_sample_definition() {
        let (w, h) = (40usize, 32usize);
        let mut data = vec![0u8; w * h];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for v in &mut data {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *v = (seed >> 33) as u8;
        }
        let p = plane(&data, w, h);

        for (bw, bh) in [(8, 8), (8, 4), (4, 8), (4, 4)] {
            for (x0, y0) in [
                (4, 4),
                (0, 0),
                (w as i32 - bw as i32, h as i32 - bh as i32),
                (-2, 3),
                (3, -2),
                (w as i32 - 1, h as i32 - 1),
            ] {
                // Covers every eighth-sample fraction, with the integer part
                // both negative and positive so the arithmetic shift is
                // exercised the same way it is for luma.
                for mvy in -14..=14 {
                    for mvx in -14..=14 {
                        let mut out = vec![0u8; bw * bh];
                        predict_chroma(&p, x0, y0, (mvx, mvy), bw, bh, &mut out);
                        for j in 0..bh {
                            for i in 0..bw {
                                let x = x0 + (mvx >> 3) + i as i32;
                                let y = y0 + (mvy >> 3) + j as i32;
                                let want = per_sample_chroma(&p, x, y, mvx & 7, mvy & 7);
                                assert_eq!(
                                    out[j * bw + i],
                                    want,
                                    "{bw}x{bh} at ({x0}, {y0}) mv ({mvx}, {mvy}) sample ({i}, {j})"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Every fractional position is a weighted average whose weights sum to a
    /// power of two, so a constant reference must predict that constant
    /// exactly. Broadest available check on the interpolation: it exercises
    /// all sixteen positions and any tap or shift error breaks it.
    #[test]
    fn constant_reference_predicts_the_constant() {
        for v in [0u8, 1, 37, 128, 254, 255] {
            let data = flat_plane(v, 32, 32);
            let p = plane(&data, 32, 32);
            for mvy in 0..4 {
                for mvx in 0..4 {
                    let mut out = [0u8; 64];
                    predict_luma(&p, 8, 8, (mvx, mvy), 8, 8, &mut out);
                    assert!(
                        out.iter().all(|&s| s == v),
                        "luma frac ({mvx},{mvy}) on constant {v} gave {:?}",
                        &out[..8]
                    );
                }
            }
            for mvy in 0..8 {
                for mvx in 0..8 {
                    let mut out = [0u8; 16];
                    predict_chroma(&p, 4, 4, (mvx, mvy), 4, 4, &mut out);
                    assert!(
                        out.iter().all(|&s| s == v),
                        "chroma frac ({mvx},{mvy}) on constant {v} was not constant"
                    );
                }
            }
        }
    }

    #[test]
    fn full_sample_position_copies_the_reference() {
        let mut data = vec![0u8; 32 * 32];
        for y in 0..32 {
            for x in 0..32 {
                data[y * 32 + x] = (x * 3 + y * 7) as u8;
            }
        }
        let p = plane(&data, 32, 32);

        let mut out = [0u8; 64];
        // +2 samples right, +1 down, in quarter-sample units.
        predict_luma(&p, 8, 8, (8, 4), 8, 8, &mut out);
        for j in 0..8 {
            for i in 0..8 {
                assert_eq!(
                    out[j * 8 + i],
                    data[(9 + j) * 32 + (10 + i)],
                    "at ({i},{j})"
                );
            }
        }
    }

    /// The six-tap filter is exact on linear input: its taps sum to 32 and it
    /// is symmetric, so a half-sample position on a ramp must land precisely
    /// on the midpoint. This pins the tap values, which the constant test
    /// cannot see.
    #[test]
    fn half_sample_on_a_ramp_hits_the_midpoint() {
        let w = 40;
        let mut data = vec![0u8; w * w];
        for y in 0..w {
            for x in 0..w {
                // Slope 4 keeps midpoints on integers, so the expected value
                // is unambiguous rather than a rounding argument.
                data[y * w + x] = (4 * x + 20) as u8;
            }
        }
        let p = plane(&data, w, w);

        let mut out = [0u8; 16];
        predict_luma(&p, 10, 10, (2, 0), 4, 4, &mut out);
        for j in 0..4 {
            for i in 0..4 {
                let left = 4 * (10 + i) + 20;
                let expected = left + 2; // midpoint of a slope-4 ramp
                assert_eq!(out[j * 4 + i] as usize, expected, "at ({i},{j})");
            }
        }
    }

    /// The spec notes the centre position may be computed by filtering
    /// horizontally then vertically, or the transpose, with identical results.
    /// Checking that here validates the intermediate is unrounded, which is
    /// the thing that makes the two orders agree.
    #[test]
    fn centre_is_order_independent() {
        let w = 40;
        let mut data = vec![0u8; w * w];
        let mut seed = 7u32;
        for s in data.iter_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *s = (seed >> 24) as u8;
        }
        let p = plane(&data, w, w);

        for y in 6..12 {
            for x in 6..12 {
                let vertical_of_horizontal = centre(&p, x, y);
                let horizontal_of_vertical = tap6(
                    half_v(&p, x - 2, y),
                    half_v(&p, x - 1, y),
                    half_v(&p, x, y),
                    half_v(&p, x + 1, y),
                    half_v(&p, x + 2, y),
                    half_v(&p, x + 3, y),
                );
                assert_eq!(
                    vertical_of_horizontal, horizontal_of_vertical,
                    "centre disagreed at ({x},{y})"
                );
            }
        }
    }

    /// Motion vectors legitimately point outside the reference picture, and
    /// the spec repeats the edge sample when they do. Must not panic, and must
    /// produce the edge value rather than garbage.
    #[test]
    fn out_of_bounds_motion_clamps_to_the_edge() {
        let mut data = vec![0u8; 16 * 16];
        for y in 0..16 {
            for x in 0..16 {
                data[y * 16 + x] = (x + y) as u8;
            }
        }
        let p = plane(&data, 16, 16);

        // Far off the top-left corner: every sample must be the corner value.
        let mut out = [0u8; 16];
        predict_luma(&p, 0, 0, (-400, -400), 4, 4, &mut out);
        assert!(out.iter().all(|&s| s == data[0]), "got {out:?}");

        // Far off the bottom-right corner.
        predict_luma(&p, 15, 15, (400, 400), 4, 4, &mut out);
        let corner = data[15 * 16 + 15];
        assert!(out.iter().all(|&s| s == corner), "got {out:?}");

        // Straddling an edge must not panic either, and the samples that fall
        // outside must repeat the corner rather than wrapping around.
        let mut chroma = [0u8; 64];
        predict_chroma(&p, 0, 0, (-3, -3), 8, 8, &mut chroma);
        assert_eq!(chroma[0], data[0], "corner sample should be repeated");
    }

    /// Negative motion vectors must floor, not truncate toward zero: a -1
    /// quarter-sample vector is a fractional position, not a whole sample.
    #[test]
    fn negative_vectors_floor_rather_than_truncate() {
        let w = 32;
        let mut data = vec![0u8; w * w];
        for y in 0..w {
            for x in 0..w {
                data[y * w + x] = (x * 4) as u8;
            }
        }
        let p = plane(&data, w, w);

        // -4 quarter-samples is exactly one whole sample left.
        let mut whole = [0u8; 16];
        predict_luma(&p, 10, 10, (-4, 0), 4, 4, &mut whole);
        for (i, &got) in whole.iter().take(4).enumerate() {
            assert_eq!(got as usize, (9 + i) * 4);
        }

        // -1 quarter-sample must be a fractional position between samples 9
        // and 10, so it cannot equal either whole-sample prediction.
        let mut frac = [0u8; 16];
        predict_luma(&p, 10, 10, (-1, 0), 4, 4, &mut frac);
        let mut at_ten = [0u8; 16];
        predict_luma(&p, 10, 10, (0, 0), 4, 4, &mut at_ten);
        assert_ne!(frac, whole);
        assert_ne!(frac, at_ten);
    }

    #[test]
    fn chroma_bilinear_interpolates_linearly() {
        let w = 16;
        let mut data = vec![0u8; w * w];
        for y in 0..w {
            for x in 0..w {
                data[y * w + x] = (8 * x) as u8;
            }
        }
        let p = plane(&data, w, w);

        // Four eighths across is the exact midpoint of a slope-8 ramp.
        let mut out = [0u8; 16];
        predict_chroma(&p, 4, 4, (4, 0), 4, 4, &mut out);
        for (i, &got) in out.iter().take(4).enumerate() {
            assert_eq!(got as usize, 8 * (4 + i) + 4, "at {i}");
        }
    }

    // -- Motion vector prediction ----------------------------------------

    fn n(mv: (i32, i32), ref_idx: i8) -> Neighbour {
        Neighbour { mv, ref_idx }
    }

    #[test]
    fn median_is_used_when_several_neighbours_match() {
        let p = predict_mv(
            n((10, -4), 0),
            n((2, 8), 0),
            n((6, 0), 0),
            0,
            Partition::Other,
        );
        assert_eq!(p, (6, 0));
    }

    /// A single matching reference index wins outright, even when its vector
    /// is the outlier the median would discard.
    #[test]
    fn a_lone_matching_reference_wins_over_the_median() {
        let p = predict_mv(
            n((100, 100), 0),
            n((2, 2), 1),
            n((4, 4), 1),
            0,
            Partition::Other,
        );
        assert_eq!(p, (100, 100));
    }

    #[test]
    fn directional_rules_apply_to_split_partitions() {
        let a = n((10, 10), 0);
        let b = n((20, 20), 0);
        let c = n((30, 30), 0);

        assert_eq!(predict_mv(a, b, c, 0, Partition::Top16x8), (20, 20));
        assert_eq!(predict_mv(a, b, c, 0, Partition::Bottom16x8), (10, 10));
        assert_eq!(predict_mv(a, b, c, 0, Partition::Left8x16), (10, 10));
        assert_eq!(predict_mv(a, b, c, 0, Partition::Right8x16), (30, 30));

        // With no reference match the directional rule does not apply and the
        // median takes over.
        let p = predict_mv(
            n((10, 10), 1),
            n((20, 20), 1),
            n((30, 30), 1),
            0,
            Partition::Top16x8,
        );
        assert_eq!(p, (20, 20), "median of the three");
    }

    #[test]
    fn missing_upper_neighbours_fall_back_to_the_left() {
        let a = n((7, -3), 0);
        let p = predict_mv(
            a,
            Neighbour::UNAVAILABLE,
            Neighbour::UNAVAILABLE,
            0,
            Partition::Other,
        );
        assert_eq!(p, (7, -3));
    }

    #[test]
    fn all_neighbours_missing_predicts_zero() {
        let p = predict_mv(
            Neighbour::UNAVAILABLE,
            Neighbour::UNAVAILABLE,
            Neighbour::UNAVAILABLE,
            0,
            Partition::Other,
        );
        assert_eq!(p, (0, 0));
    }

    #[test]
    fn skip_collapses_to_zero_next_to_a_stationary_neighbour() {
        let moving = n((40, 40), 0);

        // A stationary left neighbour forces zero.
        assert_eq!(
            predict_skip_mv(n((0, 0), 0), moving, moving, true, true),
            (0, 0),
            "stationary left neighbour"
        );
        // So does a stationary one above.
        assert_eq!(
            predict_skip_mv(moving, n((0, 0), 0), moving, true, true),
            (0, 0),
            "stationary neighbour above"
        );
        // As does an absent neighbour.
        assert_eq!(
            predict_skip_mv(Neighbour::UNAVAILABLE, moving, moving, false, true),
            (0, 0),
            "absent left neighbour"
        );
    }

    /// An intra neighbour is present but carries no vector. The spec forces
    /// zero only for a *missing* neighbour, so this must fall through to the
    /// median — where the intra neighbour contributes a zero vector like any
    /// other unusable one, but does not veto the prediction.
    #[test]
    fn an_intra_neighbour_does_not_force_a_zero_skip_vector() {
        let moving = n((40, 40), 0);
        assert_eq!(
            predict_skip_mv(Neighbour::UNAVAILABLE, moving, moving, true, true),
            (40, 40),
        );
    }

    #[test]
    fn skip_uses_the_predictor_when_neighbours_are_moving() {
        let p = predict_skip_mv(n((8, 4), 0), n((8, 4), 0), n((8, 4), 0), true, true);
        assert_eq!(p, (8, 4));
    }
}
