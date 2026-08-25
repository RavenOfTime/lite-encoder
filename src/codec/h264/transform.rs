//! Inverse transforms and dequantisation.
//!
//! H.264 does not use a floating-point DCT. It uses integer transforms chosen
//! so that encoder and decoder agree bit-exactly on every platform, with the
//! usual DCT scaling folded into the quantisation tables instead. That is why
//! dequantisation and the transform have to be read together: neither is
//! normalised on its own.
//!
//! Everything here is pure arithmetic over small fixed-size blocks, with no
//! bitstream state, so it is testable against the spec in isolation. Sections
//! cited are from ITU-T H.264 (03/2010).

/// `normAdjust4x4`, spec table 8-15 / equation 8-317.
///
/// Indexed by `[qP % 6][position class]`. The three position classes exist
/// because the 4x4 transform's basis functions have three distinct norms:
/// corners, centres, and everything else.
const NORM_ADJUST_4X4: [[i32; 3]; 6] = [
    [10, 16, 13],
    [11, 18, 14],
    [13, 20, 16],
    [14, 23, 18],
    [16, 25, 20],
    [18, 29, 23],
];

/// Which of the three `normAdjust4x4` classes each raster position falls into.
const POS_CLASS_4X4: [usize; 16] = [
    0, 2, 0, 2, //
    2, 1, 2, 1, //
    0, 2, 0, 2, //
    2, 1, 2, 1,
];

/// Flat weight scale, used whenever no custom scaling list is in force.
///
/// A flat list is 16 at every position, which makes `LevelScale` reduce to
/// `normAdjust` scaled by 16. Cameras essentially always use flat lists, but
/// SPS/PPS may override them, so the API takes the list rather than assuming.
pub const FLAT_WEIGHT_SCALE_4X4: [u8; 16] = [16; 16];

/// Spec Table 7-3 `Default_4x4_Intra`, in scanning order.
pub const DEFAULT_SCALING_LIST_4X4_INTRA: [u8; 16] = [
    6, 13, 20, 28, 13, 20, 28, 32, 20, 28, 32, 37, 28, 32, 37, 42,
];

/// Spec Table 7-3 `Default_4x4_Inter`, in scanning order.
pub const DEFAULT_SCALING_LIST_4X4_INTER: [u8; 16] = [
    10, 14, 20, 24, 14, 20, 24, 27, 20, 24, 27, 30, 24, 27, 30, 34,
];

/// `LevelScale4x4(m, i, j)`, spec equation 8-318.
#[inline]
fn level_scale_4x4(weight_scale: &[u8; 16], m: usize, pos: usize) -> i32 {
    weight_scale[pos] as i32 * NORM_ADJUST_4X4[m][POS_CLASS_4X4[pos]]
}

/// Dequantise the coefficients of a 4x4 residual block. Spec 8.5.12.1.
///
/// `qp` is the block's quantisation parameter, already clipped to 0..=51 by
/// the caller. The two branches are not an optimisation: below qP 24 the
/// scale factor is fractional, so the spec rounds rather than shifting left.
///
/// When `skip_dc` is set, position 0 is left untouched because it carries a
/// DC value produced by [`dequant_luma_dc`] or [`dequant_chroma_dc`] instead.
pub fn dequant_4x4(block: &mut [i32; 16], qp: u8, weight_scale: &[u8; 16], skip_dc: bool) {
    let qp = qp as usize;
    let (m, shift) = (qp % 6, qp / 6);
    let start = usize::from(skip_dc);

    if qp >= 24 {
        let shift = shift - 4;
        for (pos, c) in block.iter_mut().enumerate().skip(start) {
            *c = (*c * level_scale_4x4(weight_scale, m, pos)) << shift;
        }
    } else {
        let shift = 4 - shift;
        let round = 1 << (shift - 1);
        for (pos, c) in block.iter_mut().enumerate().skip(start) {
            *c = (*c * level_scale_4x4(weight_scale, m, pos) + round) >> shift;
        }
    }
}

/// The 4x4 inverse integer transform, in place. Spec 8.5.12.2.
///
/// Input is dequantised coefficients in raster order; output is residual
/// samples, already rounded by the final `(x + 32) >> 6`.
///
/// The `>> 1` terms are what make this an integer approximation of the DCT
/// rather than the DCT itself. They are arithmetic shifts, so they round
/// toward negative infinity on negative values, and the spec depends on that.
pub fn inverse_4x4(block: &mut [i32; 16]) {
    // Rows.
    for i in 0..4 {
        let r = i * 4;
        let (d0, d1, d2, d3) = (block[r], block[r + 1], block[r + 2], block[r + 3]);

        let e0 = d0 + d2;
        let e1 = d0 - d2;
        let e2 = (d1 >> 1) - d3;
        let e3 = d1 + (d3 >> 1);

        block[r] = e0 + e3;
        block[r + 1] = e1 + e2;
        block[r + 2] = e1 - e2;
        block[r + 3] = e0 - e3;
    }

    // Columns, then the common rounding shift.
    for j in 0..4 {
        let (f0, f1, f2, f3) = (block[j], block[j + 4], block[j + 8], block[j + 12]);

        let g0 = f0 + f2;
        let g1 = f0 - f2;
        let g2 = (f1 >> 1) - f3;
        let g3 = f1 + (f3 >> 1);

        block[j] = (g0 + g3 + 32) >> 6;
        block[j + 4] = (g1 + g2 + 32) >> 6;
        block[j + 8] = (g1 - g2 + 32) >> 6;
        block[j + 12] = (g0 - g3 + 32) >> 6;
    }
}

/// The 4x4 Hadamard transform applied to Intra_16x16 luma DC coefficients,
/// followed by their dequantisation. Spec 8.5.10.
///
/// Intra_16x16 macroblocks are flat enough that coding each sub-block's DC
/// independently wastes bits, so the sixteen DC values get their own second
/// transform stage. This runs before the per-block [`inverse_4x4`], and its
/// outputs are written into position 0 of each sub-block.
pub fn dequant_luma_dc(dc: &mut [i32; 16], qp: u8, weight_scale: &[u8; 16]) {
    hadamard_4x4(dc);

    let qp = qp as usize;
    let scale = level_scale_4x4(weight_scale, qp % 6, 0);
    let shift = qp / 6;

    if qp >= 36 {
        let shift = shift - 6;
        for v in dc.iter_mut() {
            *v = (*v * scale) << shift;
        }
    } else {
        let shift = 6 - shift;
        let round = 1 << (shift - 1);
        for v in dc.iter_mut() {
            *v = (*v * scale + round) >> shift;
        }
    }
}

/// Separable 4x4 Hadamard, in place. Its own function because the DC stage
/// uses it unscaled, unlike [`inverse_4x4`]'s butterflies.
fn hadamard_4x4(block: &mut [i32; 16]) {
    for i in 0..4 {
        let r = i * 4;
        let (d0, d1, d2, d3) = (block[r], block[r + 1], block[r + 2], block[r + 3]);

        let s0 = d0 + d3;
        let s1 = d1 + d2;
        let s2 = d1 - d2;
        let s3 = d0 - d3;

        block[r] = s0 + s1;
        block[r + 1] = s3 + s2;
        block[r + 2] = s0 - s1;
        block[r + 3] = s3 - s2;
    }

    for j in 0..4 {
        let (d0, d1, d2, d3) = (block[j], block[j + 4], block[j + 8], block[j + 12]);

        let s0 = d0 + d3;
        let s1 = d1 + d2;
        let s2 = d1 - d2;
        let s3 = d0 - d3;

        block[j] = s0 + s1;
        block[j + 4] = s3 + s2;
        block[j + 8] = s0 - s1;
        block[j + 12] = s3 - s2;
    }
}

/// The 2x2 Hadamard for chroma DC coefficients, plus dequantisation.
/// Spec 8.5.11.
///
/// 4:2:0 chroma is 8x8 per macroblock, so four 4x4 blocks, so four DC values.
pub fn dequant_chroma_dc(dc: &mut [i32; 4], qp: u8, weight_scale: &[u8; 16]) {
    let (c0, c1, c2, c3) = (dc[0], dc[1], dc[2], dc[3]);
    dc[0] = c0 + c1 + c2 + c3;
    dc[1] = c0 - c1 + c2 - c3;
    dc[2] = c0 + c1 - c2 - c3;
    dc[3] = c0 - c1 - c2 + c3;

    let qp = qp as usize;
    let scale = level_scale_4x4(weight_scale, qp % 6, 0);
    for v in dc.iter_mut() {
        // Single form, unlike the luma DC case: the left shift is applied
        // before the `>> 5`, so there is no fractional-scale branch.
        *v = ((*v * scale) << (qp / 6)) >> 5;
    }
}

/// `normAdjust8x8`, spec table 8-16 / equation 8-320.
///
/// Six position classes rather than the 4x4 transform's three, for the same
/// reason: the 8x8 basis functions have six distinct norms.
const NORM_ADJUST_8X8: [[i32; 6]; 6] = [
    [20, 18, 32, 19, 25, 24],
    [22, 19, 35, 21, 28, 26],
    [26, 23, 42, 24, 33, 31],
    [28, 25, 45, 26, 35, 33],
    [32, 28, 51, 30, 40, 38],
    [36, 32, 58, 34, 46, 43],
];

/// Flat 8x8 weight scale, the counterpart to [`FLAT_WEIGHT_SCALE_4X4`].
pub const FLAT_WEIGHT_SCALE_8X8: [u8; 64] = [16; 64];

/// Spec Table 7-4 `Default_8x8_Intra`, in scanning order.
pub const DEFAULT_SCALING_LIST_8X8_INTRA: [u8; 64] = [
    6, 10, 13, 16, 18, 23, 25, 27, 10, 11, 16, 18, 23, 25, 27, 29, 13, 16, 18, 23, 25, 27, 29,
    31, 16, 18, 23, 25, 27, 29, 31, 33, 18, 23, 25, 27, 29, 31, 33, 36, 23, 25, 27, 29, 31, 33,
    36, 38, 25, 27, 29, 31, 33, 36, 38, 40, 27, 29, 31, 33, 36, 38, 40, 42,
];

/// Spec Table 7-4 `Default_8x8_Inter`, in scanning order.
pub const DEFAULT_SCALING_LIST_8X8_INTER: [u8; 64] = [
    9, 13, 15, 17, 19, 21, 22, 24, 13, 13, 17, 19, 21, 22, 24, 25, 15, 17, 19, 21, 22, 24, 25,
    27, 17, 19, 21, 22, 24, 25, 27, 28, 19, 21, 22, 24, 25, 27, 28, 30, 21, 22, 24, 25, 27, 28,
    30, 32, 22, 24, 25, 27, 28, 30, 32, 33, 24, 25, 27, 28, 30, 32, 33, 35,
];

/// The 8x8 position-class rule, spec equation 8-319.
///
/// A `const fn` over the raster index rather than a table, because the rule is
/// short and the derivation is the documentation.
#[inline]
fn pos_class_8x8(pos: usize) -> usize {
    let (i, j) = (pos / 8, pos % 8);
    match (i % 4, j % 4, i % 2, j % 2) {
        (0, 0, _, _) => 0,
        (_, _, 1, 1) => 1,
        (2, 2, _, _) => 2,
        (0, _, _, 1) | (_, 0, 1, _) => 3,
        (0, 2, _, _) | (2, 0, _, _) => 4,
        _ => 5,
    }
}

/// Dequantise an 8x8 residual block. Spec 8.5.13.1.
///
/// The branch threshold is qP 36 rather than the 4x4 transform's 24, because
/// the 8x8 transform carries six more bits of headroom.
pub fn dequant_8x8(block: &mut [i32; 64], qp: u8, weight_scale: &[u8; 64]) {
    let qp = qp as usize;
    let (m, shift) = (qp % 6, qp / 6);

    if qp >= 36 {
        let shift = shift - 6;
        for (pos, c) in block.iter_mut().enumerate() {
            let scale = weight_scale[pos] as i32 * NORM_ADJUST_8X8[m][pos_class_8x8(pos)];
            *c = (*c * scale) << shift;
        }
    } else {
        let shift = 6 - shift;
        let round = 1 << (shift - 1);
        for (pos, c) in block.iter_mut().enumerate() {
            let scale = weight_scale[pos] as i32 * NORM_ADJUST_8X8[m][pos_class_8x8(pos)];
            *c = (*c * scale + round) >> shift;
        }
    }
}

/// The 8x8 inverse integer transform, in place. Spec 8.5.13.2.
///
/// High profile only, which is what the test camera emits on both streams, so
/// this is not optional for our target. Structurally the same as
/// [`inverse_4x4`]: a separable butterfly network over rows then columns, with
/// a shared `(x + 32) >> 6` at the end. The `>> 2` terms here play the role
/// the `>> 1` terms play in the 4x4 case.
pub fn inverse_8x8(block: &mut [i32; 64]) {
    for i in 0..8 {
        let r = i * 8;
        butterfly_8(block, r, 1);
    }
    for j in 0..8 {
        butterfly_8(block, j, 8);
        // The rounding shift belongs to the second pass only.
        for k in 0..8 {
            let idx = j + k * 8;
            block[idx] = (block[idx] + 32) >> 6;
        }
    }
}

/// One 8-point inverse butterfly over `block[base + k * step]`.
#[inline]
fn butterfly_8(block: &mut [i32; 64], base: usize, step: usize) {
    let d = |k: usize| block[base + k * step];
    let (d0, d1, d2, d3, d4, d5, d6, d7) = (d(0), d(1), d(2), d(3), d(4), d(5), d(6), d(7));

    let a0 = d0 + d4;
    let a2 = d0 - d4;
    let a4 = (d2 >> 1) - d6;
    let a6 = (d6 >> 1) + d2;

    let b0 = a0 + a6;
    let b2 = a2 + a4;
    let b4 = a2 - a4;
    let b6 = a0 - a6;

    let a1 = -d3 + d5 - d7 - (d7 >> 1);
    let a3 = d1 + d7 - d3 - (d3 >> 1);
    let a5 = -d1 + d7 + d5 + (d5 >> 1);
    let a7 = d3 + d5 + d1 + (d1 >> 1);

    let b1 = (a7 >> 2) + a1;
    let b3 = a3 + (a5 >> 2);
    let b5 = (a3 >> 2) - a5;
    let b7 = a7 - (a1 >> 2);

    block[base] = b0 + b7;
    block[base + step] = b2 + b5;
    block[base + 2 * step] = b4 + b3;
    block[base + 3 * step] = b6 + b1;
    block[base + 4 * step] = b6 - b1;
    block[base + 5 * step] = b4 - b3;
    block[base + 6 * step] = b2 - b5;
    block[base + 7 * step] = b0 - b7;
}

/// Add a decoded residual block to prediction samples, with clipping.
///
/// `dst` is a window into a picture plane, so `stride` is the plane's stride
/// rather than 4.
pub fn add_residual_4x4(dst: &mut [u8], offset: usize, stride: usize, block: &[i32; 16]) {
    for i in 0..4 {
        let row = offset + i * stride;
        for j in 0..4 {
            let p = dst[row + j] as i32 + block[i * 4 + j];
            dst[row + j] = p.clamp(0, 255) as u8;
        }
    }
}

/// The 8x8 counterpart of [`add_residual_4x4`].
pub fn add_residual_8x8(dst: &mut [u8], offset: usize, stride: usize, block: &[i32; 64]) {
    for i in 0..8 {
        let row = offset + i * stride;
        for j in 0..8 {
            let p = dst[row + j] as i32 + block[i * 8 + j];
            dst[row + j] = p.clamp(0, 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DC-only block must reconstruct to a flat residual, because the DC
    /// basis function is constant. This is the one property of the transform
    /// that can be checked without transcribing spec test vectors.
    #[test]
    fn dc_only_block_is_flat() {
        for dc in [-512, -64, -1, 0, 1, 64, 512] {
            let mut block = [0i32; 16];
            block[0] = dc;
            inverse_4x4(&mut block);

            let expected = (dc + 32) >> 6;
            assert!(
                block.iter().all(|&v| v == expected),
                "dc {dc} gave {block:?}, expected all {expected}"
            );
        }
    }

    /// The transform is linear on even coefficients, so superposition must
    /// hold there. This catches butterfly wiring mistakes a DC-only test
    /// cannot see, because it exercises every basis function.
    ///
    /// Odd coefficients are excluded deliberately: the `>> 1` terms make the
    /// integer transform only piecewise linear on them, which is by design.
    #[test]
    fn even_coefficients_superpose() {
        let a: [i32; 16] = [
            80, -16, 32, 4, -8, 24, 0, -12, 16, 8, -4, 20, 0, -28, 12, 36,
        ];
        let b: [i32; 16] = [
            -24, 40, -8, 16, 12, -20, 28, 4, -32, 0, 24, -8, 20, 16, -12, 8,
        ];

        let mut ta = a;
        inverse_4x4(&mut ta);
        let mut tb = b;
        inverse_4x4(&mut tb);

        let mut sum = [0i32; 16];
        for i in 0..16 {
            sum[i] = a[i] + b[i];
        }
        inverse_4x4(&mut sum);

        for i in 0..16 {
            // The separate transforms round once each, the combined one only
            // once in total, so they may differ by a single unit.
            let combined = ta[i] + tb[i];
            assert!(
                (sum[i] - combined).abs() <= 1,
                "position {i}: sum {} vs separate {combined}",
                sum[i]
            );
        }
    }

    /// The Hadamard transform is its own inverse up to a factor of 4 per
    /// dimension, so applying it twice must scale by 16.
    #[test]
    fn hadamard_4x4_is_involutive_up_to_scale() {
        let original: [i32; 16] = [5, -3, 8, 1, 0, 7, -2, 4, 9, -6, 3, -1, 2, 5, -8, 6];
        let mut block = original;
        hadamard_4x4(&mut block);
        hadamard_4x4(&mut block);

        for i in 0..16 {
            assert_eq!(block[i], original[i] * 16, "position {i}");
        }
    }

    /// Dequantisation must be monotonic in qP: raising the quantiser can only
    /// widen the reconstructed step size. A sign or shift error in the
    /// qP < 24 branch shows up here as a discontinuity at the boundary.
    #[test]
    fn dequant_step_grows_with_qp() {
        let mut previous = 0;
        for qp in 0..=51u8 {
            let mut block = [0i32; 16];
            block[0] = 1;
            dequant_4x4(&mut block, qp, &FLAT_WEIGHT_SCALE_4X4, false);
            assert!(
                block[0] >= previous,
                "qp {qp} produced step {} after {previous}",
                block[0]
            );
            previous = block[0];
        }
    }

    /// qP increasing by 6 doubles the quantisation step, by construction: that
    /// is why `normAdjust` has exactly six rows.
    #[test]
    fn dequant_doubles_every_six_qp() {
        for qp in 24..=45u8 {
            let mut low = [0i32; 16];
            low[5] = 3;
            dequant_4x4(&mut low, qp, &FLAT_WEIGHT_SCALE_4X4, false);

            let mut high = [0i32; 16];
            high[5] = 3;
            dequant_4x4(&mut high, qp + 6, &FLAT_WEIGHT_SCALE_4X4, false);

            assert_eq!(high[5], low[5] * 2, "qp {qp} to {}", qp + 6);
        }
    }

    #[test]
    fn dequant_skip_dc_leaves_position_zero_alone() {
        let mut block = [7i32; 16];
        dequant_4x4(&mut block, 30, &FLAT_WEIGHT_SCALE_4X4, true);
        assert_eq!(block[0], 7);
        assert_ne!(block[1], 7);
    }

    #[test]
    fn dc_only_8x8_block_is_flat() {
        for dc in [-512, -64, -1, 0, 1, 64, 512] {
            let mut block = [0i32; 64];
            block[0] = dc;
            inverse_8x8(&mut block);

            let expected = (dc + 32) >> 6;
            assert!(
                block.iter().all(|&v| v == expected),
                "dc {dc} did not give a flat {expected}"
            );
        }
    }

    /// Same superposition argument as the 4x4 case. Coefficients are multiples
    /// of 8 because the 8x8 butterflies use `>> 2` as well as `>> 1`, so
    /// linearity only holds where neither shift truncates.
    #[test]
    fn even_coefficients_superpose_8x8() {
        let mut a = [0i32; 64];
        let mut b = [0i32; 64];
        for i in 0..64 {
            a[i] = ((i as i32 % 7) - 3) * 8;
            b[i] = ((i as i32 % 5) - 2) * 16;
        }

        let mut ta = a;
        inverse_8x8(&mut ta);
        let mut tb = b;
        inverse_8x8(&mut tb);

        let mut sum = [0i32; 64];
        for i in 0..64 {
            sum[i] = a[i] + b[i];
        }
        inverse_8x8(&mut sum);

        for i in 0..64 {
            let combined = ta[i] + tb[i];
            assert!(
                (sum[i] - combined).abs() <= 1,
                "position {i}: sum {} vs separate {combined}",
                sum[i]
            );
        }
    }

    /// Every 8x8 raster position must land in exactly one of the six classes,
    /// and all six must be reachable. A misordered match arm in
    /// `pos_class_8x8` silently shadows a class, which this catches.
    #[test]
    fn pos_class_8x8_covers_all_six_classes() {
        let mut seen = [0u32; 6];
        for pos in 0..64 {
            seen[pos_class_8x8(pos)] += 1;
        }
        assert!(
            seen.iter().all(|&n| n > 0),
            "unreachable position class: {seen:?}"
        );
        assert_eq!(seen.iter().sum::<u32>(), 64);
        // Class 0 is the (i%4==0, j%4==0) lattice: 2x2 of them per 8x8.
        assert_eq!(seen[0], 4);
    }

    #[test]
    fn dequant_8x8_doubles_every_six_qp() {
        for qp in 36..=45u8 {
            let mut low = [0i32; 64];
            low[9] = 3;
            dequant_8x8(&mut low, qp, &FLAT_WEIGHT_SCALE_8X8);

            let mut high = [0i32; 64];
            high[9] = 3;
            dequant_8x8(&mut high, qp + 6, &FLAT_WEIGHT_SCALE_8X8);

            assert_eq!(high[9], low[9] * 2, "qp {qp} to {}", qp + 6);
        }
    }

    #[test]
    fn dequant_8x8_step_grows_with_qp() {
        let mut previous = 0;
        for qp in 0..=51u8 {
            let mut block = [0i32; 64];
            block[0] = 1;
            dequant_8x8(&mut block, qp, &FLAT_WEIGHT_SCALE_8X8);
            assert!(
                block[0] >= previous,
                "qp {qp} gave {} after {previous}",
                block[0]
            );
            previous = block[0];
        }
    }

    #[test]
    fn add_residual_clips_both_ends() {
        let mut plane = vec![250u8; 32];
        let mut block = [20i32; 16];
        block[0] = -300;
        add_residual_4x4(&mut plane, 0, 8, &block);

        assert_eq!(plane[0], 0, "large negative residual must clamp to 0");
        assert_eq!(plane[1], 255, "250 + 20 must clamp to 255");
        // Outside the 4x4 window, untouched.
        assert_eq!(plane[4], 250);
    }
}
