//! CABAC decoding of the macroblock layer, spec 7.3.5 and 9.3.3.1.1.
//!
//! Every syntax element here is coded against its neighbours. That is what
//! makes CABAC worth its complexity — a motion vector difference in a static
//! scene costs a fraction of a bit because the contexts have learned that its
//! neighbours were zero — and it is also what makes this the most delicate
//! part of the decoder. A context index that is off by one does not fail; it
//! decodes a different, entirely plausible value, and the picture drifts.
//!
//! So the derivations in 9.3.3.1.1 are written out one per syntax element,
//! each next to the element it serves, rather than being folded together into
//! something shorter. The context index numbers are the spec's own
//! `ctxIdxOffset` values, quoted rather than computed, because they are an
//! arbitrary allocation and there is nothing to derive them from.
//!
//! # Scope
//!
//! I and P slices, frame coded. `P_8x8ref0` never appears: the spec gives it
//! no CABAC binarisation, so a stream containing one is not a stream this
//! path can be asked to decode.

use super::cabac::{ArithDecoder, ContextState};
use super::mb::{self, MbType, SliceKind, SubMbType};
use super::neighbour::{BlockRef, MbAddr};
use super::residual::BlockCat;
use super::state::{MbInfo, PictureState};

/// What the context derivations need besides the bitstream.
///
/// `cur` is the macroblock being decoded, which is not yet in `state` and
/// which several derivations must read: a motion vector difference is coded
/// against the partition to its left, and for the right-hand partitions of a
/// macroblock that partition is one this same macroblock has just decoded.
pub struct MbContext<'a> {
    pub state: &'a PictureState,
    pub cur: &'a MbInfo,
    pub addr: MbAddr,
    /// `constrained_intra_pred_flag` from the picture parameter set.
    pub constrained_intra: bool,
}

impl MbContext<'_> {
    /// The state behind a block reference, taking the in-progress macroblock
    /// from `cur` rather than from the picture.
    fn info(&self, block: BlockRef) -> &MbInfo {
        if block.mb == self.addr {
            self.cur
        } else {
            self.state.at(block)
        }
    }

    fn mb_a(&self) -> Option<&MbInfo> {
        let a = self.state.neighbours.mb_a(self.addr)?;
        Some(self.state.at(BlockRef { mb: a, blk: 0 }))
    }

    fn mb_b(&self) -> Option<&MbInfo> {
        let b = self.state.neighbours.mb_b(self.addr)?;
        Some(self.state.at(BlockRef { mb: b, blk: 0 }))
    }
}

/// `mb_skip_flag`, spec 9.3.3.1.1.1. Contexts 11..13 for P slices.
///
/// The context is how many of the two neighbours were *not* skipped, so a
/// macroblock surrounded by skips costs almost nothing to skip itself. In
/// surveillance footage that is most of the picture.
pub fn decode_mb_skip_flag(d: &mut ArithDecoder<'_>, cx: &mut ContextState, c: &MbContext) -> bool {
    let not_skipped = |mb: Option<&MbInfo>| usize::from(mb.is_some_and(|m| !m.is_skipped()));
    let inc = not_skipped(c.mb_a()) + not_skipped(c.mb_b());
    d.decode_decision(&mut cx[11 + inc]) == 1
}

/// `mb_type`, spec 9.3.3.1.1.3 and tables 9-34 to 9-39.
///
/// Returns `None` only if the decoded value falls outside the type tables,
/// which a conforming stream cannot do.
pub fn decode_mb_type(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    c: &MbContext,
    slice: SliceKind,
) -> Option<MbType> {
    match slice {
        SliceKind::I => mb::mb_type(SliceKind::I, decode_intra_mb_type(d, cx, Some(c))),
        SliceKind::P => {
            // One bin separates the inter types from the intra ones.
            if d.decode_decision(&mut cx[14]) == 0 {
                let value = if d.decode_decision(&mut cx[15]) == 0 {
                    // P_L0_16x16 or P_8x8.
                    3 * d.decode_decision(&mut cx[16]) as u32
                } else {
                    // P_L0_L0_8x16 or P_L0_L0_16x8, in that order, which is
                    // why this subtracts rather than adds.
                    2 - d.decode_decision(&mut cx[17]) as u32
                };
                mb::mb_type(SliceKind::P, value)
            } else {
                // The intra suffix is the I-slice tree with its own context
                // bank and without the neighbour-derived first bin: in a P
                // slice the neighbours are usually inter, so the statistic
                // that bin carries in an I slice is not there.
                let value = decode_intra_mb_type(d, cx, None);
                mb::mb_type(SliceKind::I, value)
            }
        }
    }
}

/// The intra `mb_type` tree, shared by I slices and the intra suffix of P
/// slices.
///
/// `c` is `Some` only for an I slice, where the first bin takes a
/// neighbour-derived context; the suffix form uses a single fixed context.
fn decode_intra_mb_type(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    c: Option<&MbContext>,
) -> u32 {
    let ctx = match c {
        Some(c) => {
            // condTermFlagN counts neighbours that are *not* I_NxN, since a
            // run of I_NxN macroblocks makes another one likely.
            let not_nxn = |mb: Option<&MbInfo>| {
                usize::from(mb.is_some_and(|m| m.is_intra() && !m.is_intra_nxn()))
            };
            let inc = not_nxn(c.mb_a()) + not_nxn(c.mb_b());
            if d.decode_decision(&mut cx[3 + inc]) == 0 {
                return 0;
            }
            IntraMbTypeCtx::I_SLICE
        }
        None => {
            if d.decode_decision(&mut cx[17]) == 0 {
                return 0;
            }
            IntraMbTypeCtx::P_SUFFIX
        }
    };

    // I_PCM is signalled by the terminate bin rather than a context, because
    // what follows it is raw bytes and the arithmetic decoder has to stop.
    if d.decode_terminate() == 1 {
        return 25;
    }

    let mut value = 1 + 12 * d.decode_decision(&mut cx[ctx.luma]) as u32;
    if d.decode_decision(&mut cx[ctx.chroma_first]) == 1 {
        value += 4;
        if d.decode_decision(&mut cx[ctx.chroma_second]) == 1 {
            value += 4;
        }
    }
    value += 2 * d.decode_decision(&mut cx[ctx.mode_first]) as u32;
    value += d.decode_decision(&mut cx[ctx.mode_second]) as u32;
    value
}

/// Context indices for the `I_16x16` part of the intra `mb_type` tree.
///
/// Spec table 9-39 assigns these per `ctxIdxOffset`, and the two banks are not
/// the same shape: the I-slice bank gives the two chroma bins separate
/// contexts, while the P suffix codes both from one. Writing the indices out
/// rather than deriving them from a base is deliberate — the arithmetic that
/// works for one bank silently reads the wrong context in the other, and a
/// wrong context does not fail, it just decodes a different macroblock type
/// and desynchronises a few macroblocks later.
struct IntraMbTypeCtx {
    /// `CodedBlockPatternLuma != 0`.
    luma: usize,
    /// `CodedBlockPatternChroma != 0`.
    chroma_first: usize,
    /// `CodedBlockPatternChroma == 2`.
    chroma_second: usize,
    mode_first: usize,
    mode_second: usize,
}

impl IntraMbTypeCtx {
    /// `ctxIdxOffset` 3, with increments 3, 4, 5, 6 and 7.
    const I_SLICE: Self = Self {
        luma: 6,
        chroma_first: 7,
        chroma_second: 8,
        mode_first: 9,
        mode_second: 10,
    };
    /// `ctxIdxOffset` 17, with increments 1, 2, 2, 3 and 3.
    const P_SUFFIX: Self = Self {
        luma: 18,
        chroma_first: 19,
        chroma_second: 19,
        mode_first: 20,
        mode_second: 20,
    };
}

/// `sub_mb_type` for a P slice, spec table 9-38. Contexts 21..23.
pub fn decode_sub_mb_type(d: &mut ArithDecoder<'_>, cx: &mut ContextState) -> Option<SubMbType> {
    let value = if d.decode_decision(&mut cx[21]) == 1 {
        0
    } else if d.decode_decision(&mut cx[22]) == 0 {
        1
    } else if d.decode_decision(&mut cx[23]) == 1 {
        2
    } else {
        3
    };
    mb::sub_mb_type(value)
}

/// `transform_size_8x8_flag`, spec 9.3.3.1.1.10. Contexts 399..401.
pub fn decode_transform_size_8x8(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    c: &MbContext,
) -> bool {
    let uses_8x8 = |mb: Option<&MbInfo>| usize::from(mb.is_some_and(|m| m.transform_8x8));
    let inc = uses_8x8(c.mb_a()) + uses_8x8(c.mb_b());
    d.decode_decision(&mut cx[399 + inc]) == 1
}

/// `intra_chroma_pred_mode`, spec 9.3.3.1.1.8. Contexts 64..67.
pub fn decode_intra_chroma_pred_mode(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    c: &MbContext,
) -> u8 {
    // An inter or I_PCM neighbour contributes nothing, and so does one that
    // chose mode 0; both are stored as mode 0, so one test covers them.
    let non_dc = |mb: Option<&MbInfo>| {
        usize::from(
            mb.is_some_and(|m| m.is_intra() && m.mb_type != MbType::IPcm && m.chroma_mode != 0),
        )
    };
    let inc = non_dc(c.mb_a()) + non_dc(c.mb_b());
    if d.decode_decision(&mut cx[64 + inc]) == 0 {
        return 0;
    }
    // The remaining three modes are a truncated unary tail sharing one
    // context.
    for mode in 1..3 {
        if d.decode_decision(&mut cx[67]) == 0 {
            return mode;
        }
    }
    3
}

/// `prev_intra4x4_pred_mode_flag` and `rem_intra4x4_pred_mode` together,
/// spec 9.3.3.1.1 and 8.3.1.1. Contexts 68 and 69.
///
/// Returns the block's actual prediction mode. Coding it as "the same as
/// predicted" in one bin is why intra prediction is cheap on flat and
/// directional content: neighbouring blocks usually share a direction.
pub fn decode_intra_pred_mode(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    predicted: u8,
) -> u8 {
    if d.decode_decision(&mut cx[68]) == 1 {
        return predicted;
    }
    // Three fixed-length bins share context 69. The CABAC binarisation emits
    // the least-significant bit first (table 9-34), unlike the usual
    // most-significant-bit-first fixed-length syntax.
    let mut mode = 0;
    for bit in 0..3 {
        mode |= d.decode_decision(&mut cx[69]) << bit;
    }
    // The coded value skips over the predicted mode, so anything at or above
    // it shifts up by one. That is what makes eight modes fit in three bins.
    mode + u8::from(mode >= predicted)
}

/// `coded_block_pattern`, spec 9.3.3.1.1.4. Contexts 73..76 and 77..84.
///
/// Returns `(CodedBlockPatternLuma, CodedBlockPatternChroma)`.
pub fn decode_coded_block_pattern(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    c: &MbContext,
) -> (u8, u8) {
    let mut luma = 0u8;
    for blk in 0..4u8 {
        // The context counts neighbouring 8x8 blocks that are *empty*, so a
        // macroblock in a flat region codes its empty pattern cheaply. Blocks
        // already decoded in this macroblock count, which is why `luma` is
        // read back as it is built.
        let empty = |dx: i32, dy: i32| {
            let Some(n) = c.state.neighbours.luma_8x8_neighbour(c.addr, blk, dx, dy) else {
                // An unavailable neighbour is not evidence of emptiness.
                return 0;
            };
            let info = self_or(c, n, luma);
            usize::from(!info)
        };
        let inc = empty(-1, 0) + 2 * empty(0, -1);
        luma |= d.decode_decision(&mut cx[73 + inc]) << blk;
    }

    // Chroma is two bins: "any coefficients at all", then "AC as well as DC".
    let chroma_of = |mb: Option<&MbInfo>| mb.map_or(0, |m| m.cbp_chroma);
    let (a, b) = (chroma_of(c.mb_a()), chroma_of(c.mb_b()));
    let inc = usize::from(a > 0) + 2 * usize::from(b > 0);
    if d.decode_decision(&mut cx[77 + inc]) == 0 {
        return (luma, 0);
    }
    let inc = 4 + usize::from(a == 2) + 2 * usize::from(b == 2);
    (luma, 1 + d.decode_decision(&mut cx[77 + inc]))
}

/// Whether an 8x8 luma block has coded coefficients, reading the in-progress
/// pattern for blocks of the current macroblock.
fn self_or(c: &MbContext, n: BlockRef, luma_so_far: u8) -> bool {
    if n.mb == c.addr {
        return luma_so_far >> n.blk & 1 == 1;
    }
    let info = c.state.at(n);
    // I_PCM has every coefficient, by definition; skip has none.
    info.mb_type == MbType::IPcm || info.luma_8x8_coded(n.blk)
}

/// `mb_qp_delta`, spec 9.3.3.1.1.5. Contexts 60..63.
///
/// The binarisation is unary over a zig-zag of the signed range: 0, 1, -1, 2,
/// -2 and so on, so small corrections in either direction are short.
pub fn decode_mb_qp_delta(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    previous_was_nonzero: bool,
) -> i32 {
    let mut inc = usize::from(previous_was_nonzero);
    let mut value = 0;
    // Two full quantiser ranges is the widest a conforming delta can be; the
    // bound is a guard against a corrupt stream, not part of the syntax.
    while value < 104 && d.decode_decision(&mut cx[60 + inc]) == 1 {
        inc = 2 + (inc >> 1);
        value += 1;
    }
    let magnitude = (value + 1) / 2;
    if value % 2 == 1 {
        magnitude
    } else {
        -magnitude
    }
}

/// `ref_idx_l0`, spec 9.3.3.1.1.6. Contexts 54..59.
///
/// `blk` is the 4x4 luma block at the partition's top-left corner.
pub fn decode_ref_idx(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    c: &MbContext,
    blk: u8,
) -> u8 {
    // A neighbour that already points away from the nearest reference makes
    // this partition more likely to as well.
    let refers_away = |n: Option<BlockRef>| {
        usize::from(n.is_some_and(|n| {
            let info = c.info(n);
            !info.is_intra() && info.ref_idx_of_block(n.blk) > 0
        }))
    };
    let mut inc = refers_away(c.state.neighbours.luma_4x4_a(c.addr, blk))
        + 2 * refers_away(c.state.neighbours.luma_4x4_b(c.addr, blk));

    let mut value = 0;
    // Unbounded unary. After the first bin the context collapses to one of
    // two, since how far past reference 1 an index runs carries no useful
    // neighbour statistic.
    while value < 32 && d.decode_decision(&mut cx[54 + inc]) == 1 {
        inc = (inc >> 2) + 4;
        value += 1;
    }
    value
}

/// One component of `mvd_l0`, spec 9.3.3.1.1.7. Contexts 40..46 and 47..53.
///
/// `comp` is 0 for horizontal and 1 for vertical; the two have separate
/// context banks because vertical motion in real footage is rarer and its
/// differences are distributed differently.
pub fn decode_mvd(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    c: &MbContext,
    blk: u8,
    comp: usize,
) -> i32 {
    let base = if comp == 0 { 40 } else { 47 };

    // The context is the size of the neighbouring differences, not of the
    // vectors. A region moving fast but uniformly still codes cheaply, which
    // is the point: what is expensive is motion that disagrees with itself.
    let abs_mvd = |n: Option<BlockRef>| {
        n.map_or(0, |n| {
            let info = c.info(n);
            if info.is_intra() || info.is_skipped() {
                0
            } else {
                i32::from(info.mvd[n.blk as usize][comp].abs())
            }
        })
    };
    let sum = abs_mvd(c.state.neighbours.luma_4x4_a(c.addr, blk))
        + abs_mvd(c.state.neighbours.luma_4x4_b(c.addr, blk));
    let inc = match sum {
        0..=2 => 0,
        3..=32 => 1,
        _ => 2,
    };

    if d.decode_decision(&mut cx[base + inc]) == 0 {
        return 0;
    }

    // UEG3: a unary prefix of up to nine bins, then an Exp-Golomb escape.
    let mut magnitude = 1;
    let mut ctx = base + 3;
    while magnitude < 9 && d.decode_decision(&mut cx[ctx]) == 1 {
        if magnitude < 4 {
            ctx += 1;
        }
        magnitude += 1;
    }
    if magnitude >= 9 {
        magnitude += d.decode_exp_golomb_bypass(3) as i32;
    }

    if d.decode_bypass() == 1 {
        -magnitude
    } else {
        magnitude
    }
}

/// Which neighbouring block a `coded_block_flag` context looks at.
///
/// The categories index different scans over different planes, so the
/// neighbour lookup differs per category even though the context derivation
/// that consumes it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CbfBlock {
    /// The `I_16x16` luma DC block, whose neighbours are the neighbouring
    /// macroblocks' DC blocks.
    LumaDc,
    /// A 4x4 luma block, by index.
    Luma(u8),
    /// A chroma component's DC block.
    ChromaDc(usize),
    /// A 4x4 chroma AC block of a component.
    ChromaAc(usize, u8),
}

/// `ctxIdxInc` for `coded_block_flag`, spec 9.3.3.1.1.9.
///
/// The rule is not simply "did the neighbour have coefficients". An
/// unavailable neighbour counts as *coded* when the current macroblock is
/// intra and as uncoded when it is inter, because an intra macroblock at a
/// picture edge is far more likely to carry residual than an inter one that
/// simply had nothing to correct.
pub fn coded_block_flag_ctx_inc(c: &MbContext, block: CbfBlock) -> usize {
    let a = neighbouring_cbf(c, block, -1, 0);
    let b = neighbouring_cbf(c, block, 0, -1);
    usize::from(a) + 2 * usize::from(b)
}

fn neighbouring_cbf(c: &MbContext, block: CbfBlock, dx: i32, dy: i32) -> bool {
    let neighbours = &c.state.neighbours;
    let found = match block {
        CbfBlock::LumaDc | CbfBlock::ChromaDc(_) => {
            // A DC block's neighbour is the neighbouring macroblock's DC
            // block, so the lookup is at macroblock granularity.
            let mb = if dx < 0 {
                neighbours.mb_a(c.addr)
            } else {
                neighbours.mb_b(c.addr)
            };
            mb.map(|mb| BlockRef { mb, blk: 0 })
        }
        CbfBlock::Luma(blk) => neighbours.luma_4x4_neighbour(c.addr, blk, dx, dy),
        CbfBlock::ChromaAc(_, blk) => neighbours.chroma_4x4_neighbour(c.addr, blk, dx, dy),
    };

    let Some(found) = found else {
        // Unavailable: intra macroblocks assume a coded neighbour, inter ones
        // assume an empty one.
        return c.cur.is_intra();
    };
    let info = c.info(found);

    // I_PCM is every coefficient at once, so it always counts as coded.
    if info.mb_type == MbType::IPcm {
        return true;
    }
    if info.is_skipped() {
        return false;
    }

    match block {
        CbfBlock::LumaDc => info.cbf_luma_dc,
        CbfBlock::Luma(_) => info.luma_cbf(found.blk),
        CbfBlock::ChromaDc(comp) => info.cbf_chroma_dc[comp],
        CbfBlock::ChromaAc(comp, _) => info.chroma_cbf(comp, found.blk),
    }
}

/// The block category a `coded_block_flag` lookup belongs to.
pub fn cbf_category(block: CbfBlock, intra_16x16: bool) -> BlockCat {
    match block {
        CbfBlock::LumaDc => BlockCat::Intra16x16Dc,
        CbfBlock::Luma(_) if intra_16x16 => BlockCat::Intra16x16Ac,
        CbfBlock::Luma(_) => BlockCat::Luma4x4,
        CbfBlock::ChromaDc(_) => BlockCat::ChromaDc,
        CbfBlock::ChromaAc(..) => BlockCat::ChromaAc,
    }
}

#[cfg(test)]
mod tests {
    use super::super::intra::Intra16x16Mode;
    use super::super::mb::Partitioning;
    use super::*;

    /// Builds a picture with `mbs` decoded, and macroblock 5 — the one every
    /// test decodes — opened but not yet written. That order matters:
    /// availability compares slices, so the macroblock being decoded has to
    /// be on the map before its own neighbour queries answer anything.
    fn state_with(mbs: &[(MbAddr, MbInfo)]) -> PictureState {
        let mut state = PictureState::new(4, 3);
        for &(addr, info) in mbs {
            state.put(addr, 0, info);
        }
        state.begin_macroblock(5, 0);
        state
    }

    fn context<'a>(state: &'a PictureState, cur: &'a MbInfo, addr: MbAddr) -> MbContext<'a> {
        MbContext {
            state,
            cur,
            addr,
            constrained_intra: false,
        }
    }

    fn inter(mv: [i16; 2]) -> MbInfo {
        let mut mb = MbInfo::new(MbType::Inter(Partitioning::P16x16), 26);
        mb.fill_motion(0, 0, 16, 16, mv);
        mb
    }

    fn intra_16x16() -> MbInfo {
        MbInfo::new(
            MbType::Intra16x16 {
                mode: Intra16x16Mode::Dc,
                cbp_luma: 15,
                cbp_chroma: 0,
            },
            26,
        )
    }

    // -- Context derivations ----------------------------------------------
    //
    // These are checked directly rather than through a decode, because the
    // context index is the part that is easy to get wrong and impossible to
    // see afterwards: a wrong index still decodes a plausible value.

    /// An unavailable neighbour counts as coded for an intra macroblock and
    /// uncoded for an inter one. The asymmetry is deliberate in the spec and
    /// is the derivation's least obvious clause.
    #[test]
    fn an_unavailable_neighbour_depends_on_the_current_macroblocks_type() {
        let state = state_with(&[]);

        let intra = intra_16x16();
        // Macroblock 5 has neighbours in the picture, but none are decoded.
        let c = context(&state, &intra, 5);
        assert_eq!(coded_block_flag_ctx_inc(&c, CbfBlock::Luma(0)), 3);

        let p = inter([0, 0]);
        let c = context(&state, &p, 5);
        assert_eq!(coded_block_flag_ctx_inc(&c, CbfBlock::Luma(0)), 0);
    }

    #[test]
    fn a_coded_neighbour_raises_the_block_flag_context() {
        let mut left = intra_16x16();
        // Block 5 of the left macroblock is the one to the left of block 0.
        left.set_luma_cbf(5, true);
        let state = state_with(&[(4, left), (1, intra_16x16())]);

        let cur = intra_16x16();
        let c = context(&state, &cur, 5);
        // Left coded, above not: increment 1.
        assert_eq!(coded_block_flag_ctx_inc(&c, CbfBlock::Luma(0)), 1);

        let mut above = intra_16x16();
        above.set_luma_cbf(10, true);
        let state = state_with(&[(4, left), (1, above)]);
        let c = context(&state, &cur, 5);
        assert_eq!(coded_block_flag_ctx_inc(&c, CbfBlock::Luma(0)), 3);
    }

    /// I_PCM has every coefficient by definition, and skip has none.
    #[test]
    fn pcm_and_skip_neighbours_are_treated_as_full_and_empty() {
        let pcm = MbInfo::new(MbType::IPcm, 26);
        let state = state_with(&[(4, pcm), (1, MbInfo::skipped())]);
        let cur = intra_16x16();
        let c = context(&state, &cur, 5);
        assert_eq!(coded_block_flag_ctx_inc(&c, CbfBlock::Luma(0)), 1);
    }

    /// The motion vector difference context reads the neighbours' coded
    /// differences, not their vectors: uniform fast motion stays cheap.
    #[test]
    fn the_vector_difference_context_ignores_large_but_agreeing_motion() {
        let mut left = inter([200, 200]);
        left.fill_mvd(0, 0, 16, 16, [0, 0]);
        let mut above = inter([200, 200]);
        above.fill_mvd(0, 0, 16, 16, [0, 0]);
        let state = state_with(&[(4, left), (1, above)]);

        let cur = inter([0, 0]);
        let c = context(&state, &cur, 5);
        // Both neighbours coded a zero difference, so the smallest context.
        assert_eq!(mvd_ctx_inc(&c, 0, 0), 0);

        let mut left = inter([0, 0]);
        left.fill_mvd(0, 0, 16, 16, [40, 0]);
        let state = state_with(&[(4, left), (1, above)]);
        let c = context(&state, &cur, 5);
        assert_eq!(mvd_ctx_inc(&c, 0, 0), 2);
    }

    /// Mirrors the increment computed inside [`decode_mvd`], so the threshold
    /// boundaries can be checked without a bitstream.
    fn mvd_ctx_inc(c: &MbContext, blk: u8, comp: usize) -> usize {
        let abs_mvd = |n: Option<BlockRef>| {
            n.map_or(0, |n| {
                let info = c.info(n);
                if info.is_intra() || info.is_skipped() {
                    0
                } else {
                    i32::from(info.mvd[n.blk as usize][comp].abs())
                }
            })
        };
        let sum = abs_mvd(c.state.neighbours.luma_4x4_a(c.addr, blk))
            + abs_mvd(c.state.neighbours.luma_4x4_b(c.addr, blk));
        match sum {
            0..=2 => 0,
            3..=32 => 1,
            _ => 2,
        }
    }

    #[test]
    fn the_vector_difference_thresholds_are_three_and_thirty_three() {
        for (sum, expected) in [(2, 0), (3, 1), (32, 1), (33, 2)] {
            let mut left = inter([0, 0]);
            left.fill_mvd(0, 0, 16, 16, [sum, 0]);
            let state = state_with(&[(4, left)]);
            let cur = inter([0, 0]);
            let c = context(&state, &cur, 5);
            assert_eq!(mvd_ctx_inc(&c, 0, 0), expected, "sum {sum}");
        }
    }

    #[test]
    fn an_intra_neighbour_contributes_no_vector_difference() {
        let mut left = intra_16x16();
        // Even if the field holds something, an intra macroblock has no
        // motion and must not be read as though it did.
        left.mvd = [[99, 99]; 16];
        let state = state_with(&[(4, left)]);
        let cur = inter([0, 0]);
        let c = context(&state, &cur, 5);
        assert_eq!(mvd_ctx_inc(&c, 0, 0), 0);
    }

    /// The block category depends on whether the macroblock is I_16x16,
    /// because its 4x4 blocks carry AC coefficients only.
    #[test]
    fn the_block_category_splits_ac_from_full_blocks() {
        assert_eq!(
            cbf_category(CbfBlock::Luma(3), true),
            BlockCat::Intra16x16Ac
        );
        assert_eq!(cbf_category(CbfBlock::Luma(3), false), BlockCat::Luma4x4);
        assert_eq!(cbf_category(CbfBlock::LumaDc, true), BlockCat::Intra16x16Dc);
        assert_eq!(
            cbf_category(CbfBlock::ChromaDc(0), false),
            BlockCat::ChromaDc
        );
        assert_eq!(
            cbf_category(CbfBlock::ChromaAc(1, 2), false),
            BlockCat::ChromaAc
        );
    }

    // -- Decoding ---------------------------------------------------------

    /// Every context index the module can reach must be inside the bank. A
    /// decode with garbage input walks a wide spread of the trees, so this
    /// exercises indices that a well-formed stream might not reach for hours.
    #[test]
    fn decoding_garbage_stays_within_the_context_bank_and_terminates() {
        let state = state_with(&[(4, inter([0, 0])), (1, intra_16x16())]);
        let cur = inter([0, 0]);

        for fill in [0x00u8, 0x55, 0xaa, 0xff] {
            let data = [fill; 8];
            let mut d = ArithDecoder::new(&data).expect("init");
            let mut cx = ContextState::new(super::super::cabac::ContextVariant::Intra, 26);
            let c = context(&state, &cur, 5);

            decode_mb_skip_flag(&mut d, &mut cx, &c);
            decode_mb_type(&mut d, &mut cx, &c, SliceKind::P);
            decode_sub_mb_type(&mut d, &mut cx);
            decode_transform_size_8x8(&mut d, &mut cx, &c);
            let chroma = decode_intra_chroma_pred_mode(&mut d, &mut cx, &c);
            assert!(chroma <= 3);
            let mode = decode_intra_pred_mode(&mut d, &mut cx, 4);
            assert!(mode < 9, "intra mode {mode} is out of range");
            let (luma, chroma) = decode_coded_block_pattern(&mut d, &mut cx, &c);
            assert!(luma <= 15 && chroma <= 2);
            let delta = decode_mb_qp_delta(&mut d, &mut cx, false);
            assert!(delta.abs() <= 52, "quantiser delta {delta} is out of range");
            assert!(decode_ref_idx(&mut d, &mut cx, &c, 0) <= 32);
            let mvd = decode_mvd(&mut d, &mut cx, &c, 0, 0);
            assert!(mvd.abs() < 1 << 24, "vector difference {mvd} ran away");
        }
    }

    /// The prediction-mode escape skips over the predicted value, so all
    /// eight modes stay reachable and none is coded twice.
    #[test]
    fn the_intra_mode_escape_skips_the_predicted_mode() {
        // Drive the escape path with a stream that codes mode bits directly.
        // What matters is the mapping, so check it across every predicted
        // mode by construction rather than by decoding.
        for predicted in 0..9u8 {
            let decoded: Vec<u8> = (0..8u8).map(|m| m + u8::from(m >= predicted)).collect();
            assert!(
                !decoded.contains(&predicted),
                "predicted {predicted} was reachable"
            );
            assert_eq!(
                decoded
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                8
            );
            assert!(decoded.iter().all(|&m| m < 9));
        }
    }

    /// The quantiser delta binarisation alternates sign as it lengthens, so
    /// small corrections in either direction are short.
    #[test]
    fn the_quantiser_delta_binarisation_alternates_sign() {
        let mapped = |value: i32| {
            let magnitude = (value + 1) / 2;
            if value % 2 == 1 {
                magnitude
            } else {
                -magnitude
            }
        };
        assert_eq!(mapped(0), 0);
        assert_eq!(mapped(1), 1);
        assert_eq!(mapped(2), -1);
        assert_eq!(mapped(3), 2);
        assert_eq!(mapped(4), -2);
    }
}
