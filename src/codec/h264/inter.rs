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
//! The implementation here is the straightforward per-sample form, which
//! recomputes shared intermediates. That is deliberate for now: it matches the
//! spec structure closely enough to audit line by line. Blocking it up so each
//! six-tap intermediate is computed once, and then adding SIMD, is a change
//! confined to this file and worth making only once real streams decode
//! correctly.

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
#[inline]
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

/// Vertical half-sample intermediate `h1`, spec 8-245.
#[inline]
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
#[inline]
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

/// Predict one luma block from a reference plane. Spec 8.4.2.2.1.
///
/// `x0`, `y0` are the block's position in the *current* picture, and `mv` is
/// in quarter-sample units. `out` receives `bw * bh` samples with stride `bw`.
///
/// The sixteen fractional positions are the spec's lettered grid: `(0,0)` is
/// the full sample `G`, `(2,2)` is the centre `j`, and the rest are half
/// positions or averages of two positions.
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

    for j in 0..bh {
        for i in 0..bw {
            let x = base_x + i as i32;
            let y = base_y + j as i32;

            let v = match (xfrac, yfrac) {
                // Full sample.
                (0, 0) => reference.at(x, y),

                // Pure horizontal.
                (2, 0) => round_half(half_h(reference, x, y)),
                (1, 0) => (reference.at(x, y) + round_half(half_h(reference, x, y)) + 1) >> 1,
                (3, 0) => (reference.at(x + 1, y) + round_half(half_h(reference, x, y)) + 1) >> 1,

                // Pure vertical.
                (0, 2) => round_half(half_v(reference, x, y)),
                (0, 1) => (reference.at(x, y) + round_half(half_v(reference, x, y)) + 1) >> 1,
                (0, 3) => (reference.at(x, y + 1) + round_half(half_v(reference, x, y)) + 1) >> 1,

                // Centre, and the quarter positions adjacent to it.
                (2, 2) => round_centre(centre(reference, x, y)),
                (1, 2) => {
                    let h = round_half(half_v(reference, x, y));
                    let j_ = round_centre(centre(reference, x, y));
                    (h + j_ + 1) >> 1
                }
                (3, 2) => {
                    let m = round_half(half_v(reference, x + 1, y));
                    let j_ = round_centre(centre(reference, x, y));
                    (j_ + m + 1) >> 1
                }
                (2, 1) => {
                    let b = round_half(half_h(reference, x, y));
                    let j_ = round_centre(centre(reference, x, y));
                    (b + j_ + 1) >> 1
                }
                (2, 3) => {
                    let s = round_half(half_h(reference, x, y + 1));
                    let j_ = round_centre(centre(reference, x, y));
                    (j_ + s + 1) >> 1
                }

                // Diagonal quarter positions: average of the two adjacent
                // half positions, never involving the centre.
                (1, 1) => {
                    let b = round_half(half_h(reference, x, y));
                    let h = round_half(half_v(reference, x, y));
                    (b + h + 1) >> 1
                }
                (3, 1) => {
                    let b = round_half(half_h(reference, x, y));
                    let m = round_half(half_v(reference, x + 1, y));
                    (b + m + 1) >> 1
                }
                (1, 3) => {
                    let h = round_half(half_v(reference, x, y));
                    let s = round_half(half_h(reference, x, y + 1));
                    (h + s + 1) >> 1
                }
                (3, 3) => {
                    let m = round_half(half_v(reference, x + 1, y));
                    let s = round_half(half_h(reference, x, y + 1));
                    (m + s + 1) >> 1
                }

                _ => unreachable!("fractional position is masked to 0..=3"),
            };
            out[j * bw + i] = clip1(v);
        }
    }
}

/// Predict one chroma block for 4:2:0. Spec 8.4.2.2.2.
///
/// `mv` is the *luma* motion vector in quarter-sample units. Chroma is half
/// resolution in both axes, so the same vector addresses eighth-sample
/// positions in the chroma plane, which is why no scaling appears here and the
/// fractional mask is 7 rather than 3.
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
    let base_x = x0 + (mvx >> 3);
    let base_y = y0 + (mvy >> 3);
    let xfrac = mvx & 7;
    let yfrac = mvy & 7;

    for j in 0..bh {
        for i in 0..bw {
            let x = base_x + i as i32;
            let y = base_y + j as i32;

            let a = reference.at(x, y);
            let b = reference.at(x + 1, y);
            let c = reference.at(x, y + 1);
            let d = reference.at(x + 1, y + 1);

            // Bilinear over eighths, spec 8-266.
            let v = ((8 - xfrac) * (8 - yfrac) * a
                + xfrac * (8 - yfrac) * b
                + (8 - xfrac) * yfrac * c
                + xfrac * yfrac * d
                + 32)
                >> 6;
            out[j * bw + i] = clip1(v);
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
pub fn predict_skip_mv(a: Neighbour, b: Neighbour, c: Neighbour) -> (i32, i32) {
    // A missing neighbour, or an immediately-adjacent one that is itself
    // stationary against reference 0, forces a zero vector rather than a
    // median. This is what makes static scenes collapse to almost no bits.
    if a.ref_idx < 0
        || b.ref_idx < 0
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
            predict_skip_mv(n((0, 0), 0), moving, moving),
            (0, 0),
            "stationary left neighbour"
        );
        // So does a stationary one above.
        assert_eq!(
            predict_skip_mv(moving, n((0, 0), 0), moving),
            (0, 0),
            "stationary neighbour above"
        );
        // As does an unavailable neighbour.
        assert_eq!(
            predict_skip_mv(Neighbour::UNAVAILABLE, moving, moving),
            (0, 0),
            "unavailable left neighbour"
        );
    }

    #[test]
    fn skip_uses_the_predictor_when_neighbours_are_moving() {
        let p = predict_skip_mv(n((8, 4), 0), n((8, 4), 0), n((8, 4), 0));
        assert_eq!(p, (8, 4));
    }
}
