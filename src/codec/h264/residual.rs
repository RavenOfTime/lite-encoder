//! CABAC residual block decoding, spec 9.3.2.3 and 9.3.3.1.3.
//!
//! A residual block is not coded as a list of coefficients. It is coded as a
//! *significance map* — which positions are non-zero, and where the last one
//! is — followed by the levels of those positions in reverse scan order. Two
//! properties of real video make that cheap: most coefficients are zero, and
//! most non-zero ones are ±1. The reverse ordering exists so the level
//! contexts can adapt on the small high-frequency coefficients before
//! reaching the large low-frequency ones, which is where the coding gain is.
//!
//! # Scope
//!
//! Frame-coded 4:2:0 only. The field-coded significance tables are a second
//! full set of context offsets and a second scan order; interlacing is
//! rejected at the parameter-set stage, so they are absent rather than
//! unreachable. See [`super`].

use super::cabac::{ArithDecoder, ContextState};

/// Zig-zag scan for a 4x4 block, spec table 8-13.
///
/// Maps scan position to raster position within the block. Coefficients are
/// coded in this order because it runs from low to high frequency, which is
/// what makes the trailing run of zeroes long enough to be worth signalling
/// with a single "last" flag.
pub const ZIGZAG_4X4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

/// Zig-zag scan for an 8x8 block, spec table 8-14.
pub const ZIGZAG_8X8: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Spec `ctxBlockCat`, table 9-42: which kind of block is being decoded.
///
/// The category is not cosmetic. It selects a different bank of contexts for
/// every syntax element in the block, because the coefficient statistics of,
/// say, a chroma DC block and an 8x8 luma block have nothing in common.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockCat {
    /// The 16 DC coefficients of an `I_16x16` macroblock, gathered from its
    /// sixteen 4x4 blocks and transformed again.
    Intra16x16Dc = 0,
    /// The 15 AC coefficients of one 4x4 block of an `I_16x16` macroblock;
    /// position 0 is missing because it went into the DC block.
    Intra16x16Ac = 1,
    /// A full 4x4 luma block.
    Luma4x4 = 2,
    /// The 4 chroma DC coefficients of one component, 4:2:0.
    ChromaDc = 3,
    /// The 15 AC coefficients of one 4x4 chroma block.
    ChromaAc = 4,
    /// A full 8x8 luma block, under `transform_size_8x8_flag`.
    Luma8x8 = 5,
}

impl BlockCat {
    /// Spec `maxNumCoeff`: how many coefficient positions the block has.
    pub fn max_coeffs(self) -> usize {
        match self {
            BlockCat::Intra16x16Dc | BlockCat::Luma4x4 => 16,
            BlockCat::Intra16x16Ac | BlockCat::ChromaAc => 15,
            BlockCat::ChromaDc => 4,
            BlockCat::Luma8x8 => 64,
        }
    }

    /// Whether the block's coefficients start at position 1 of its 4x4 block
    /// rather than position 0.
    ///
    /// True for the two AC categories, whose DC coefficient is coded
    /// elsewhere. Callers scattering coefficients back into raster order need
    /// this offset or they will write the whole block one position too early.
    pub fn is_ac(self) -> bool {
        matches!(self, BlockCat::Intra16x16Ac | BlockCat::ChromaAc)
    }

    /// Whether `coded_block_flag` is coded for this category.
    ///
    /// False for 8x8 luma in 4:2:0: the coded block pattern already says
    /// whether the block has coefficients, so the flag would be redundant.
    pub fn has_coded_block_flag(self) -> bool {
        self != BlockCat::Luma8x8
    }

    /// Base `ctxIdx` for `coded_block_flag`, spec table 9-11 plus the
    /// per-category offsets of table 9-42.
    fn coded_block_flag_base(self) -> usize {
        85 + 4 * self as usize
    }

    /// Base `ctxIdx` for `significant_coeff_flag`, frame coded.
    fn significant_base(self) -> usize {
        match self {
            BlockCat::Luma8x8 => 402,
            _ => 105 + CAT_4X4_OFFSETS[self as usize],
        }
    }

    /// Base `ctxIdx` for `last_significant_coeff_flag`, frame coded.
    fn last_significant_base(self) -> usize {
        match self {
            BlockCat::Luma8x8 => 417,
            _ => 166 + CAT_4X4_OFFSETS[self as usize],
        }
    }

    /// Base `ctxIdx` for `coeff_abs_level_minus1`.
    fn level_base(self) -> usize {
        match self {
            BlockCat::Luma8x8 => 426,
            _ => 227 + LEVEL_CAT_OFFSETS[self as usize],
        }
    }
}

/// `ctxIdxBlockCatOffset` for the significance flags of the 4x4 categories.
/// The gaps are the categories' coefficient counts, less the one position
/// that never needs a flag.
const CAT_4X4_OFFSETS: [usize; 5] = [0, 15, 29, 44, 47];

/// `ctxIdxBlockCatOffset` for `coeff_abs_level_minus1`.
const LEVEL_CAT_OFFSETS: [usize; 5] = [0, 10, 20, 30, 39];

/// Spec table 9-43: `ctxIdxInc` for `significant_coeff_flag` in a frame-coded
/// 8x8 block.
///
/// The 4x4 categories use the scan position directly, but an 8x8 block has 63
/// of them and only 15 contexts, so positions are grouped by how likely a
/// coefficient there is to be non-zero. The grouping is not monotonic in scan
/// order, which is why it is a table and not arithmetic.
const SIG_8X8: [u8; 63] = [
    0, 1, 2, 3, 4, 5, 5, 4, 4, 3, 3, 4, 4, 4, 5, 5, 4, 4, 4, 4, 3, 3, 6, 7, 7, 7, 8, 9, 10, 9, 8,
    7, 7, 6, 11, 12, 13, 11, 6, 7, 8, 9, 14, 10, 9, 8, 6, 11, 12, 13, 11, 6, 9, 14, 10, 9, 11, 12,
    13, 11, 14, 10, 12,
];

/// Spec table 9-43: `ctxIdxInc` for `last_significant_coeff_flag` in a
/// frame-coded 8x8 block. Coarser than [`SIG_8X8`]: nine contexts, and this
/// one *is* monotonic, since how likely a position is to be the last non-zero
/// coefficient depends only on how far into the scan it is.
const LAST_8X8: [u8; 63] = [
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    3, 3, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8,
];

/// Decodes `coded_block_flag`, spec 9.3.3.1.1.9.
///
/// `ctx_inc` is `condTermFlagA + 2 * condTermFlagB`, derived from whether the
/// left and above blocks had coefficients. That derivation needs macroblock
/// state this module does not have, so the caller computes it; see
/// [`super::neighbour`].
pub fn decode_coded_block_flag(
    d: &mut ArithDecoder<'_>,
    contexts: &mut ContextState,
    cat: BlockCat,
    ctx_inc: usize,
) -> bool {
    debug_assert!(
        cat.has_coded_block_flag(),
        "{cat:?} has no coded_block_flag"
    );
    debug_assert!(ctx_inc < 4);
    d.decode_decision(&mut contexts[cat.coded_block_flag_base() + ctx_inc]) == 1
}

/// Decodes one residual block's coefficients into `out`, in scan order.
///
/// `out` must be `cat.max_coeffs()` long and is fully overwritten, so the
/// caller need not zero it. The values are levels, not dequantised
/// coefficients, and they are in *scan* order: [`ZIGZAG_4X4`] and
/// [`ZIGZAG_8X8`] map them back to raster positions.
///
/// Assumes `coded_block_flag` has already said the block has coefficients.
/// Calling it for an empty block would decode another block's bins.
pub fn decode_coefficients(
    d: &mut ArithDecoder<'_>,
    contexts: &mut ContextState,
    cat: BlockCat,
    out: &mut [i32],
) {
    let max = cat.max_coeffs();
    debug_assert_eq!(out.len(), max);
    out.fill(0);

    // Pass one: the significance map. `significant` is dense rather than a
    // list of positions because pass two walks it backwards.
    let mut significant = [false; 64];
    let mut num_coeff = max;
    let mut i = 0;
    while i < num_coeff - 1 {
        let sig = d.decode_decision(&mut contexts[cat.significant_base() + sig_inc(cat, i)]) == 1;
        significant[i] = sig;
        if sig
            && d.decode_decision(&mut contexts[cat.last_significant_base() + last_inc(cat, i)]) == 1
        {
            // This was the last non-zero coefficient; everything after it is
            // zero and is never coded.
            num_coeff = i + 1;
            break;
        }
        i += 1;
        if d.overran() {
            return;
        }
    }
    // Falling out of the loop without a "last" flag means the final position
    // is non-zero by inference: had it been zero, the previous coefficient
    // would have been signalled as last.
    if i == num_coeff - 1 {
        significant[i] = true;
    }

    // Pass two: levels, in reverse scan order.
    //
    // The two counters are what makes this adaptive. Contexts are chosen by
    // how many ±1 levels and how many larger levels have been seen *so far in
    // this block*, so a block that opens with a run of ones stays cheap and
    // one that opens with a large level immediately switches to the contexts
    // that expect more.
    let mut num_eq1 = 0u32;
    let mut num_gt1 = 0u32;
    for pos in (0..num_coeff).rev() {
        if !significant[pos] {
            continue;
        }

        let base = cat.level_base();
        let first_inc = if num_gt1 != 0 {
            0
        } else {
            4.min(1 + num_eq1) as usize
        };
        // Spec 9.3.3.1.3: chroma DC has one context fewer in this bank.
        let rest_inc = 5 + (4 - usize::from(cat == BlockCat::ChromaDc)).min(num_gt1 as usize);

        let prefix = d.decode_truncated_unary(14, contexts, |bin| {
            base + if bin == 0 { first_inc } else { rest_inc }
        });
        let magnitude = if prefix == 14 {
            // The prefix saturated, so an Exp-Golomb suffix carries the rest.
            14 + d.decode_exp_golomb_bypass(0)
        } else {
            prefix
        } + 1;

        if magnitude == 1 {
            num_eq1 += 1;
        } else {
            num_gt1 += 1;
        }

        out[pos] = if d.decode_bypass() == 1 {
            -(magnitude as i32)
        } else {
            magnitude as i32
        };

        if d.overran() {
            return;
        }
    }
}

/// `ctxIdxInc` for `significant_coeff_flag` at scan position `i`.
fn sig_inc(cat: BlockCat, i: usize) -> usize {
    match cat {
        BlockCat::Luma8x8 => SIG_8X8[i] as usize,
        // 4:2:0 chroma DC has four coefficients and three contexts, so the
        // last two share. Spec writes this as Min(i / NumC8x8, 2), and
        // NumC8x8 is 1 for 4:2:0.
        BlockCat::ChromaDc => i.min(2),
        _ => i,
    }
}

/// `ctxIdxInc` for `last_significant_coeff_flag` at scan position `i`.
fn last_inc(cat: BlockCat, i: usize) -> usize {
    match cat {
        BlockCat::Luma8x8 => LAST_8X8[i] as usize,
        BlockCat::ChromaDc => i.min(2),
        _ => i,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATS: [BlockCat; 6] = [
        BlockCat::Intra16x16Dc,
        BlockCat::Intra16x16Ac,
        BlockCat::Luma4x4,
        BlockCat::ChromaDc,
        BlockCat::ChromaAc,
        BlockCat::Luma8x8,
    ];

    /// A scan order is a permutation. Anything else silently drops or
    /// duplicates coefficients, which is the kind of bug that produces a
    /// picture that looks almost right.
    #[test]
    fn the_scan_orders_are_permutations() {
        let mut seen = [false; 16];
        for &pos in &ZIGZAG_4X4 {
            assert!(!seen[pos], "position {pos} appears twice");
            seen[pos] = true;
        }
        assert!(seen.iter().all(|&s| s));

        let mut seen = [false; 64];
        for &pos in &ZIGZAG_8X8 {
            assert!(!seen[pos], "position {pos} appears twice");
            seen[pos] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    /// The scan must run low to high frequency, or the trailing-zero
    /// assumption the significance map is built on does not hold. Frequency
    /// here is the sum of the row and column indices.
    #[test]
    fn the_scan_orders_run_from_low_to_high_frequency() {
        for (scan, side) in [(&ZIGZAG_4X4[..], 4), (&ZIGZAG_8X8[..], 8)] {
            assert_eq!(scan[0], 0, "the scan must open at DC");
            assert_eq!(*scan.last().unwrap(), side * side - 1);
            // Adjacent scan positions never jump more than one diagonal.
            for pair in scan.windows(2) {
                let diagonal = |p: usize| p / side + p % side;
                let step = diagonal(pair[1]) as i32 - diagonal(pair[0]) as i32;
                assert!(step.abs() <= 1, "{:?} is not a zig-zag step", pair);
            }
        }
    }

    #[test]
    fn every_category_has_a_coefficient_count_matching_its_shape() {
        assert_eq!(BlockCat::Intra16x16Dc.max_coeffs(), 16);
        assert_eq!(BlockCat::Luma4x4.max_coeffs(), 16);
        assert_eq!(BlockCat::Luma8x8.max_coeffs(), 64);
        assert_eq!(BlockCat::ChromaDc.max_coeffs(), 4);
        // The AC categories are one short, because their DC went elsewhere.
        assert_eq!(BlockCat::Intra16x16Ac.max_coeffs(), 15);
        assert_eq!(BlockCat::ChromaAc.max_coeffs(), 15);
        for cat in CATS {
            assert_eq!(cat.is_ac(), cat.max_coeffs() == 15);
        }
    }

    #[test]
    fn only_the_eight_by_eight_category_omits_the_coded_block_flag() {
        for cat in CATS {
            assert_eq!(cat.has_coded_block_flag(), cat != BlockCat::Luma8x8);
        }
    }

    /// Every context index any category can produce must land inside the 460
    /// contexts the engine allocates. An index off the end of a bank reads
    /// another syntax element's adapted state, which decodes into plausible
    /// garbage rather than failing.
    #[test]
    fn every_context_index_stays_within_the_context_bank() {
        use super::super::cabac::NUM_CONTEXTS;

        for cat in CATS {
            let max = cat.max_coeffs();
            if cat.has_coded_block_flag() {
                for inc in 0..4 {
                    assert!(cat.coded_block_flag_base() + inc < NUM_CONTEXTS);
                }
            }
            for i in 0..max - 1 {
                assert!(
                    cat.significant_base() + sig_inc(cat, i) < NUM_CONTEXTS,
                    "{cat:?} significance at {i}"
                );
                assert!(
                    cat.last_significant_base() + last_inc(cat, i) < NUM_CONTEXTS,
                    "{cat:?} last-significance at {i}"
                );
            }
            // The level bank spans ten contexts: one of five for the first
            // bin, one of five for the rest.
            for inc in 0..10 {
                assert!(cat.level_base() + inc < NUM_CONTEXTS, "{cat:?} level {inc}");
            }
        }
    }

    /// The banks must not overlap either, or two syntax elements adapt the
    /// same state and neither converges.
    #[test]
    fn the_context_banks_of_different_categories_are_disjoint() {
        let mut used: Vec<(usize, String)> = Vec::new();
        for cat in CATS {
            for i in 0..cat.max_coeffs() - 1 {
                used.push((
                    cat.significant_base() + sig_inc(cat, i),
                    format!("{cat:?} significance"),
                ));
                used.push((
                    cat.last_significant_base() + last_inc(cat, i),
                    format!("{cat:?} last-significance"),
                ));
            }
        }
        used.sort();
        used.dedup();
        // Any index claimed by two different elements is a collision.
        for pair in used.windows(2) {
            assert_ne!(
                pair[0].0, pair[1].0,
                "{} and {} share a context",
                pair[0].1, pair[1].1
            );
        }
    }

    /// Table 9-43 is transcribed, so check the properties the spec gives it
    /// rather than re-listing the values.
    #[test]
    fn the_eight_by_eight_significance_tables_have_the_right_shape() {
        assert_eq!(*SIG_8X8.iter().max().unwrap(), 14);
        assert_eq!(SIG_8X8[0], 0);
        // Fifteen contexts, all of them used.
        let used: std::collections::BTreeSet<_> = SIG_8X8.iter().collect();
        assert_eq!(used.len(), 15);

        // The last-significance table is monotonic and uses nine contexts.
        assert!(LAST_8X8.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(*LAST_8X8.iter().max().unwrap(), 8);
        let used: std::collections::BTreeSet<_> = LAST_8X8.iter().collect();
        assert_eq!(used.len(), 9);
    }

    /// Chroma DC has four coefficients but only three contexts, and the 4x4
    /// categories index by scan position directly.
    #[test]
    fn context_increments_saturate_only_for_chroma_dc() {
        assert_eq!(
            (0..3)
                .map(|i| sig_inc(BlockCat::ChromaDc, i))
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        for i in 0..15 {
            assert_eq!(sig_inc(BlockCat::Luma4x4, i), i);
            assert_eq!(last_inc(BlockCat::Intra16x16Ac, i), i);
        }
    }

    /// Garbage input must terminate and stay inside the block, not spin or
    /// index past `out`. A damaged stream is a normal event on an RTSP feed,
    /// so this is a real path rather than a hypothetical one.
    #[test]
    fn garbage_input_terminates_within_the_block() {
        for cat in CATS {
            for fill in [0x00u8, 0x55, 0xff] {
                let data = [fill; 4];
                let mut d = ArithDecoder::new(&data).expect("init");
                let mut contexts =
                    ContextState::new(super::super::cabac::ContextVariant::Intra, 26);
                let mut out = vec![0; cat.max_coeffs()];
                decode_coefficients(&mut d, &mut contexts, cat, &mut out);
                // Levels are bounded by the binarisation, not by the input: a
                // wild value here would mean the Exp-Golomb escape ran away.
                for &level in &out {
                    assert!(level.abs() < 1 << 20, "{cat:?} decoded level {level}");
                }
            }
        }
    }
}
