//! Picture-level CABAC decode loop.
//!
//! This is the seam between parsed slice headers and reconstruction: the
//! header's `PictureConfig` creates the three per-picture stores together,
//! then every macroblock is made available only after its samples exist.

use super::cabac::{ArithDecoder, ContextState, ContextVariant};
use super::inter;
use super::loopfilter::{self, FilterParams};
use super::mb::{self, MbType, Partitioning};
use super::neighbour::MbAddr;
use super::picture::{Cropping, Dpb, Picture, RefMarking};
use super::recon::{self, ReconParams, Residual};
use super::residual::{self, BlockCat};
use super::slice::{CabacSlice, Deblocking, PictureConfig, SliceInfo, SliceKind};
use super::state::{MbInfo, PictureState, DC_PRED_MODE};
use super::syntax::{self, MbContext};
use crate::media::Frame;
use crate::Error;
use std::time::Duration;

/// What a finished picture hands back, so the next one can reuse it.
pub struct Finished {
    /// The picture cropped to its display rectangle.
    pub frame: Frame,
    pub dpb: Dpb,
    /// The macroblock state store, to be reset and used again.
    pub state: PictureState,
    /// Picture buffers the DPB no longer needs (MMCO and/or sliding window).
    pub recycled: Vec<Picture>,
}

/// The mutable state of one picture.  Keeping these fields together prevents
/// accidental dimension mismatches between sample storage and neighbour state.
pub struct PictureDecoder {
    config: PictureConfig,
    pub picture: Picture,
    pub state: PictureState,
    pub dpb: Dpb,
    params: ReconParams,
    /// Deblocking control per slice, in slice-id order. Collected during
    /// decode because the filter runs after the last slice, by which point
    /// the headers that carry these are gone.
    deblocking: Vec<Deblocking>,
}

impl PictureDecoder {
    pub fn new(config: PictureConfig) -> Self {
        Self {
            picture: Picture::new(config.width_mbs, config.height_mbs),
            state: PictureState::new(config.width_mbs, config.height_mbs),
            dpb: Dpb::new(config.max_refs, config.max_frame_num),
            config,
            params: ReconParams {
                scaling: Default::default(),
                chroma_qp_offset: [0, 0],
                constrained_intra: false,
            },
            deblocking: Vec::new(),
        }
    }

    /// Decodes into buffers an earlier picture finished with.
    ///
    /// `picture` and `state` must already be reset and sized for `config`;
    /// they are handed in rather than allocated because at 1080p allocating
    /// them per picture costs more than the entire deblocking filter.
    pub fn with_resources(
        config: PictureConfig,
        dpb: Dpb,
        picture: Picture,
        state: PictureState,
    ) -> Self {
        debug_assert!(picture.is_sized(config.width_mbs, config.height_mbs));
        let mut decoder = Self::new(config);
        decoder.dpb = dpb;
        decoder.picture = picture;
        decoder.state = state;
        decoder
    }

    /// Decodes one CABAC slice into this picture.  The caller supplies a
    /// stable id for the slice; it is used by availability derivation.
    pub fn decode_slice(&mut self, slice: &CabacSlice, slice_id: u32) -> Result<(), Error> {
        if slice.info.picture != self.config {
            return Err(Error::Decode(
                "slice picture configuration changed mid-picture".into(),
            ));
        }
        if slice.info.idr {
            self.dpb.clear();
        }
        self.picture.frame_num = slice.info.frame_num;
        self.params.scaling = slice.scaling.clone();
        self.params.chroma_qp_offset = slice.info.chroma_qp_offset;
        self.params.constrained_intra = slice.info.constrained_intra;
        // Indexed by slice id, so a gap would silently shift every later
        // slice's filter settings onto the wrong macroblocks.
        if self.deblocking.len() <= slice_id as usize {
            self.deblocking
                .resize(slice_id as usize + 1, Deblocking::default());
        }
        self.deblocking[slice_id as usize] = slice.info.deblocking;
        let refs = self.dpb.list0(
            slice.info.frame_num,
            &slice.list_mods,
            slice.info.num_ref_idx_l0,
        )?;
        let mut d = ArithDecoder::new_at_bit(&slice.data, slice.bit_offset as usize)
            .ok_or_else(|| Error::Decode("truncated CABAC slice".into()))?;
        let mut cx = ContextState::new(
            match slice.info.kind {
                SliceKind::I => ContextVariant::Intra,
                SliceKind::P => ContextVariant::Inter {
                    cabac_init_idc: slice.info.cabac_init_idc,
                },
            },
            i32::from(slice.info.slice_qp),
        );
        let mut qp = slice.info.slice_qp;
        let total = self.config.width_mbs * self.config.height_mbs;
        for addr in slice.info.first_mb..total {
            self.state.begin_macroblock(addr, slice_id);
            let (info, residual) =
                self.decode_macroblock(&mut d, &mut cx, addr, qp, &slice.info)?;
            qp = info.qp;
            recon::reconstruct(
                &mut self.picture,
                &self.state,
                &refs,
                addr,
                &info,
                &residual,
                &self.params,
            )
            .map_err(|error| Error::Decode(format!("macroblock {addr} {info:?}: {error}")))?;
            self.state.put(addr, slice_id, info);
            if d.decode_terminate() == 1 {
                break;
            }
        }
        if d.overran() {
            return Err(Error::Decode("truncated CABAC macroblock data".into()));
        }
        Ok(())
    }

    /// Deblocks this picture and hands out the displayable frame. Reference
    /// pictures become reference zero for a following P picture; disposable
    /// pictures are returned immediately for buffer reuse. Call only after
    /// every slice was decoded.
    ///
    /// The filter runs here rather than per macroblock because it reads
    /// samples from the macroblocks below and to the right of the one it is
    /// filtering, which do not exist until the picture is complete.
    ///
    /// Cropping to the display rectangle copies the samples out, so the coded
    /// picture itself can then move into the DPB rather than being cloned into
    /// it. The order matters only for cost, not for correctness: both the
    /// frame and the reference are the deblocked picture.
    pub fn finish(
        mut self,
        crop: Cropping,
        pts: Duration,
        marking: &RefMarking,
    ) -> Result<Finished, Error> {
        // Concealment before the filter: unclaimed macroblocks must not keep
        // whatever samples a recycled buffer held, and the filter already
        // skips them so greying first is order-independent for correctness
        // but keeps a damaged picture's holes from looking like old video.
        let concealed_macroblocks = self.picture.grey_uncovered(&self.state.neighbours);
        loopfilter::filter_picture(
            &mut self.picture,
            &self.state,
            &FilterParams {
                slices: &self.deblocking,
                chroma_qp_offset: self.params.chroma_qp_offset,
            },
        );
        let frame = self.picture.to_frame(crop, pts, concealed_macroblocks);
        let recycled = match marking {
            RefMarking::None => vec![self.picture],
            marking => self.dpb.mark_reference(self.picture, marking)?,
        };
        Ok(Finished {
            frame,
            dpb: self.dpb,
            state: self.state,
            recycled,
        })
    }

    fn decode_macroblock(
        &self,
        d: &mut ArithDecoder<'_>,
        cx: &mut ContextState,
        addr: MbAddr,
        qp: u8,
        slice: &SliceInfo,
    ) -> Result<(MbInfo, Residual), Error> {
        let SliceInfo {
            kind,
            transform_8x8_enabled: allow_8x8,
            num_ref_idx_l0,
            ..
        } = *slice;
        let skip = kind == SliceKind::P
            && syntax::decode_mb_skip_flag(
                d,
                cx,
                &MbContext {
                    state: &self.state,
                    cur: &MbInfo::skipped(),
                    addr,
                    constrained_intra: self.params.constrained_intra,
                },
            );
        if skip {
            let mut info = MbInfo::skipped();
            info.qp = qp;
            let mv = skip_mv(&self.state, addr);
            info.fill_motion(0, 0, 16, 16, [mv.0 as i16, mv.1 as i16]);
            return Ok((info, Residual::default()));
        }
        let placeholder = MbInfo::skipped();
        let mb_kind = match kind {
            SliceKind::I => mb::SliceKind::I,
            SliceKind::P => mb::SliceKind::P,
        };
        let ty = syntax::decode_mb_type(
            d,
            cx,
            &MbContext {
                state: &self.state,
                cur: &placeholder,
                addr,
                constrained_intra: self.params.constrained_intra,
            },
            mb_kind,
        )
        .ok_or_else(|| Error::Decode("invalid CABAC macroblock type".into()))?;
        if ty == MbType::IPcm {
            return Err(Error::Decode(
                "I_PCM macroblocks are not supported by CABAC picture decode".into(),
            ));
        }
        let mut info = MbInfo::new(ty, qp);
        if let MbType::Intra16x16 {
            cbp_luma,
            cbp_chroma,
            ..
        } = ty
        {
            info.cbp_luma = cbp_luma;
            info.cbp_chroma = cbp_chroma;
        }
        if info.is_intra() {
            // For I_NxN, transform_size_8x8_flag precedes mb_pred in the
            // bitstream. It selects whether mb_pred contains four 8x8 modes
            // or sixteen 4x4 modes, so it must be read before decode_intra.
            if allow_8x8 && ty == MbType::IntraNxN {
                let c = MbContext {
                    state: &self.state,
                    cur: &info,
                    addr,
                    constrained_intra: self.params.constrained_intra,
                };
                info.transform_8x8 = syntax::decode_transform_size_8x8(d, cx, &c);
            }
            let use_8x8 = info.transform_8x8;
            decode_intra(
                d,
                cx,
                &self.state,
                addr,
                &mut info,
                self.params.constrained_intra,
                use_8x8,
            )?;
        }
        if let MbType::Inter(shape) = ty {
            decode_inter(d, cx, &self.state, addr, &mut info, shape, num_ref_idx_l0)?;
        }
        if ty.has_coded_block_pattern() {
            let c = MbContext {
                state: &self.state,
                cur: &info,
                addr,
                constrained_intra: self.params.constrained_intra,
            };
            let (l, ch) = syntax::decode_coded_block_pattern(d, cx, &c);
            info.cbp_luma = l;
            info.cbp_chroma = ch;
        }
        if allow_8x8 && matches!(ty, MbType::Inter(_)) && info.cbp_luma != 0 {
            let c = MbContext {
                state: &self.state,
                cur: &info,
                addr,
                constrained_intra: self.params.constrained_intra,
            };
            info.transform_8x8 = syntax::decode_transform_size_8x8(d, cx, &c);
        }
        if info.cbp_luma != 0 || info.cbp_chroma != 0 || matches!(ty, MbType::Intra16x16 { .. }) {
            // `mb_qp_delta` changes context after a non-zero delta in the
            // preceding macroblock of this slice. `available` also excludes
            // the macroblock before `first_mb_in_slice`, whose inferred
            // previous delta is zero.
            let previous_was_nonzero = addr
                .checked_sub(1)
                .and_then(|prev| self.state.available(prev, addr))
                .is_some_and(|prev| prev.qp_delta_nonzero);
            let delta = syntax::decode_mb_qp_delta(d, cx, previous_was_nonzero);
            info.qp = mb::next_qp(qp, delta);
            info.qp_delta_nonzero = delta != 0;
        }
        let residual = decode_residual(
            d,
            cx,
            &self.state,
            addr,
            &mut info,
            self.params.constrained_intra,
        );
        Ok((info, residual))
    }
}

fn skip_mv(state: &PictureState, addr: MbAddr) -> (i32, i32) {
    // P_Skip predicts as one 16x16 partition, so its neighbour C is the
    // macroblock above and to the right, not a block inside the one above.
    // A skipped macroblock has no earlier partition of its own, and all
    // three neighbours of a 16x16 partition lie outside it.
    let (a, b, c) = partition_neighbours(state, addr, &MbInfo::skipped(), 0, 0, 16);
    let n = &state.neighbours;
    inter::predict_skip_mv(a, b, c, n.mb_a(addr).is_some(), n.mb_b(addr).is_some())
}

/// Neighbours A, B and C of a partition, in the form vector prediction wants.
///
/// C falls back to D when it is unavailable, which spec 8.4.1.3.2 requires and
/// which happens for every partition along the right edge of a picture as well
/// as for any partition whose above-right block is not yet decoded.
fn partition_neighbours(
    state: &PictureState,
    addr: MbAddr,
    cur: &MbInfo,
    x: usize,
    y: usize,
    width: usize,
) -> (inter::Neighbour, inter::Neighbour, inter::Neighbour) {
    let n = &state.neighbours;
    (
        state.motion(n.luma_partition_neighbour(addr, x, y, -1, 0), addr, cur),
        state.motion(n.luma_partition_neighbour(addr, x, y, 0, -1), addr, cur),
        state.motion(
            n.luma_partition_neighbour(addr, x, y, width as i32, -1)
                .or_else(|| n.luma_partition_neighbour(addr, x, y, -1, -1)),
            addr,
            cur,
        ),
    )
}

fn decode_intra(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    state: &PictureState,
    addr: MbAddr,
    info: &mut MbInfo,
    constrained: bool,
    use_8x8: bool,
) -> Result<(), Error> {
    if info.mb_type == MbType::IntraNxN {
        let blocks = if use_8x8 { 4 } else { 16 };
        for blk in 0..blocks {
            let n = &state.neighbours;
            let (left, above, dst) = if use_8x8 {
                // Spec 8.3.2.1: an 8x8 block predicts its mode from a
                // *particular* 4x4 sub-block of each neighbouring 8x8 block —
                // the top-right one for the neighbour to the left, the
                // bottom-left one for the neighbour above. Those are the two
                // sub-blocks that actually touch this block's edges.
                //
                // It only matters when the neighbouring macroblock is coded
                // Intra_4x4 and so has four genuinely different modes in that
                // 8x8; when it is Intra_8x8 all four hold the same mode.
                // Getting it wrong therefore mispredicts the mode only where
                // an 8x8 macroblock sits beside a 4x4 one — and a mispredicted
                // mode decodes the same number of bins, so it corrupts the
                // picture without desynchronising CABAC.
                let sub = |dx, dy, corner| {
                    n.luma_8x8_neighbour(addr, blk, dx, dy)
                        .map(|b| super::neighbour::BlockRef {
                            mb: b.mb,
                            blk: b.blk * 4 + corner,
                        })
                };
                (sub(-1, 0, 1), sub(0, -1, 2), blk * 4)
            } else {
                (n.luma_4x4_a(addr, blk), n.luma_4x4_b(addr, blk), blk)
            };
            // A preceding block in this macroblock is not in `state` yet;
            // resolve it against the in-progress information instead.  Using
            // the old stored value turns it into DC and eventually makes the
            // syntax-derived mode illegal at a picture edge.
            let mode_of = |block: Option<super::neighbour::BlockRef>| {
                let block = block?;
                let neighbour = if block.mb == addr {
                    &*info
                } else {
                    state.at(block)
                };
                if constrained && !neighbour.is_intra() {
                    None
                } else {
                    Some(neighbour.intra_modes[block.blk as usize])
                }
            };
            // If either neighbour is unavailable, the specification derives
            // DC directly; it does not include a substituted DC mode in the
            // `min` operation.
            let predicted = match (mode_of(left), mode_of(above)) {
                (Some(left), Some(above)) => left.min(above),
                _ => DC_PRED_MODE,
            };
            let mode = syntax::decode_intra_pred_mode(d, cx, predicted);
            if use_8x8 {
                // State is indexed as 4x4 blocks even when the syntax codes
                // one mode per 8x8 block. Populate every 4x4 location so
                // later macroblocks derive the right neighbour predictor.
                info.intra_modes[dst as usize..dst as usize + 4].fill(mode);
            } else {
                info.intra_modes[dst as usize] = mode;
            }
        }
    }
    let c = MbContext {
        state,
        cur: info,
        addr,
        constrained_intra: constrained,
    };
    info.chroma_mode = syntax::decode_intra_chroma_pred_mode(d, cx, &c);
    Ok(())
}

fn decode_inter(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    state: &PictureState,
    addr: MbAddr,
    info: &mut MbInfo,
    shape: Partitioning,
    num_ref_idx_l0: usize,
) -> Result<(), Error> {
    // Spec 7.3.5.1 and 7.3.5.2: every partition's `ref_idx_l0` is coded
    // before any `mvd_l0`, not interleaved with it. The two loops cannot be
    // merged, and merging them desynchronises CABAC on the first macroblock
    // that has more than one partition.
    let codes_ref_idx = num_ref_idx_l0 > 1 && shape.codes_ref_idx();

    if shape.has_sub_partitions() {
        let mut sub = [super::mb::SubMbType::S8x8; 4];
        for item in &mut sub {
            *item = syntax::decode_sub_mb_type(d, cx)
                .ok_or_else(|| Error::Decode("invalid CABAC sub macroblock type".into()))?;
        }
        if codes_ref_idx {
            for (part_i, part) in shape.parts().iter().enumerate() {
                let _ = part_i;
                let ref_idx = decode_ref_idx(d, cx, state, addr, info, part.x, part.y);
                info.fill_ref_idx(part.x, part.y, part.width, part.height, ref_idx);
            }
        }
        for (part_i, part) in shape.parts().iter().enumerate() {
            for child in sub[part_i].parts() {
                let x = part.x + child.x;
                let y = part.y + child.y;
                decode_motion(
                    d,
                    cx,
                    state,
                    addr,
                    info,
                    x,
                    y,
                    child.width,
                    child.height,
                    inter::Partition::Other,
                );
            }
        }
        return Ok(());
    }

    if codes_ref_idx {
        for part in shape.parts() {
            let ref_idx = decode_ref_idx(d, cx, state, addr, info, part.x, part.y);
            info.fill_ref_idx(part.x, part.y, part.width, part.height, ref_idx);
        }
    }
    for (part_i, part) in shape.parts().iter().enumerate() {
        decode_motion(
            d,
            cx,
            state,
            addr,
            info,
            part.x,
            part.y,
            part.width,
            part.height,
            shape.mv_prediction(part_i),
        );
    }
    Ok(())
}

/// `ref_idx_l0` for the partition whose top-left corner is at (`x`, `y`).
fn decode_ref_idx(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    state: &PictureState,
    addr: MbAddr,
    info: &MbInfo,
    x: usize,
    y: usize,
) -> i8 {
    let c = MbContext {
        state,
        cur: info,
        addr,
        constrained_intra: false,
    };
    syntax::decode_ref_idx(d, cx, &c, super::neighbour::luma_4x4_index(x, y)) as i8
}

#[allow(clippy::too_many_arguments)]
fn decode_motion(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    state: &PictureState,
    addr: MbAddr,
    info: &mut MbInfo,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    partition: inter::Partition,
) {
    let blk = super::neighbour::luma_4x4_index(x, y);
    // Whatever `ref_idx_l0` decoding left here, which is 0 when the syntax
    // element was absent.
    let ref_idx = info.ref_idx_of_block(blk);
    let c = MbContext {
        state,
        cur: info,
        addr,
        constrained_intra: false,
    };
    let mvd = [
        syntax::decode_mvd(d, cx, &c, blk, 0) as i16,
        syntax::decode_mvd(d, cx, &c, blk, 1) as i16,
    ];
    let (a, b, c_mv) = partition_neighbours(state, addr, info, x, y, width);
    let pred = inter::predict_mv(a, b, c_mv, ref_idx, partition);
    let mv = [
        pred.0.saturating_add(i32::from(mvd[0])) as i16,
        pred.1.saturating_add(i32::from(mvd[1])) as i16,
    ];
    info.fill_mvd(x, y, width, height, mvd);
    info.fill_motion(x, y, width, height, mv);
}

fn decode_residual(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    state: &PictureState,
    addr: MbAddr,
    info: &mut MbInfo,
    constrained: bool,
) -> Residual {
    let mut out = Residual::default();
    let intra16 = matches!(info.mb_type, MbType::Intra16x16 { .. });
    if intra16
        && decode_cbf(
            d,
            cx,
            state,
            addr,
            info,
            constrained,
            syntax::CbfBlock::LumaDc,
        )
    {
        info.cbf_luma_dc = true;
        residual::decode_coefficients(d, cx, BlockCat::Intra16x16Dc, &mut out.luma_dc);
    }
    if info.transform_8x8 {
        for blk in 0..4u8 {
            if info.luma_8x8_coded(blk) {
                // In 4:2:0 the 8x8 residual path does not code four
                // `coded_block_flag`s. Its coded-block-pattern bit instead
                // implies non-zero state for the whole 8x8 region. Record
                // that state per 4x4 block: the next macroblock's 4x4 CBF
                // contexts read these boundary blocks even when it does not
                // itself use an 8x8 transform.
                for block in blk * 4..blk * 4 + 4 {
                    info.set_luma_cbf(block, true);
                }
                residual::decode_coefficients(
                    d,
                    cx,
                    BlockCat::Luma8x8,
                    &mut out.luma_8x8[blk as usize],
                );
            }
        }
    } else {
        for blk in 0..16u8 {
            if info.cbp_luma >> (blk / 4) & 1 == 0 {
                continue;
            }
            if decode_cbf(
                d,
                cx,
                state,
                addr,
                info,
                constrained,
                syntax::CbfBlock::Luma(blk),
            ) {
                info.set_luma_cbf(blk, true);
                let cat = if intra16 {
                    BlockCat::Intra16x16Ac
                } else {
                    BlockCat::Luma4x4
                };
                let levels = &mut out.luma[blk as usize];
                if intra16 {
                    residual::decode_coefficients(d, cx, cat, &mut levels[1..]);
                } else {
                    residual::decode_coefficients(d, cx, cat, levels);
                }
            }
        }
    }
    // Chroma DC for both components precedes every chroma AC block.  The
    // order is part of the CABAC bitstream: decoding Cb AC before Cr DC
    // changes the arithmetic state and corrupts the next macroblock.
    if info.cbp_chroma != 0 {
        for comp in 0..2 {
            if decode_cbf(
                d,
                cx,
                state,
                addr,
                info,
                constrained,
                syntax::CbfBlock::ChromaDc(comp),
            ) {
                info.cbf_chroma_dc[comp] = true;
                residual::decode_coefficients(d, cx, BlockCat::ChromaDc, &mut out.chroma_dc[comp]);
            }
        }
    }
    if info.cbp_chroma == 2 {
        for comp in 0..2 {
            for blk in 0..4u8 {
                if decode_cbf(
                    d,
                    cx,
                    state,
                    addr,
                    info,
                    constrained,
                    syntax::CbfBlock::ChromaAc(comp, blk),
                ) {
                    info.set_chroma_cbf(comp, blk, true);
                    residual::decode_coefficients(
                        d,
                        cx,
                        BlockCat::ChromaAc,
                        &mut out.chroma[comp][blk as usize][1..],
                    );
                }
            }
        }
    }
    out
}

fn decode_cbf(
    d: &mut ArithDecoder<'_>,
    cx: &mut ContextState,
    state: &PictureState,
    addr: MbAddr,
    info: &MbInfo,
    constrained: bool,
    block: syntax::CbfBlock,
) -> bool {
    let c = MbContext {
        state,
        cur: info,
        addr,
        constrained_intra: constrained,
    };
    let inc = syntax::coded_block_flag_ctx_inc(&c, block);
    residual::decode_coded_block_flag(
        d,
        cx,
        syntax::cbf_category(block, matches!(info.mb_type, MbType::Intra16x16 { .. })),
        inc,
    )
}

#[cfg(test)]
mod tests {
    use super::super::picture::MmcoOp;
    use super::*;

    fn config() -> PictureConfig {
        PictureConfig {
            width_mbs: 1,
            height_mbs: 1,
            max_refs: 1,
            max_frame_num: 16,
            crop: Cropping::default(),
        }
    }

    #[test]
    fn finish_reports_concealed_macroblock_count() {
        let config = PictureConfig {
            width_mbs: 2,
            height_mbs: 2,
            max_refs: 1,
            max_frame_num: 16,
            crop: Cropping::default(),
        };
        let mut decoder = PictureDecoder::new(config);
        decoder.state.begin_macroblock(0, 0);
        decoder.state.begin_macroblock(3, 0);

        let finished = decoder
            .finish(
                Cropping::default(),
                Duration::from_millis(40),
                &RefMarking::SlidingWindow,
            )
            .expect("finish");

        assert_eq!(finished.frame.concealed_macroblocks, 2);
        assert_eq!(finished.frame.pts, Duration::from_millis(40));
    }

    #[test]
    fn reference_picture_enters_the_dpb() {
        let finished = PictureDecoder::new(config())
            .finish(
                Cropping::default(),
                Duration::ZERO,
                &RefMarking::SlidingWindow,
            )
            .expect("finish");
        assert_eq!(finished.dpb.len(), 1);
        assert!(finished.recycled.is_empty());
    }

    #[test]
    fn disposable_picture_is_recycled_without_displacing_a_reference() {
        let mut decoder = PictureDecoder::new(config());
        let mut reference = Picture::new(1, 1);
        reference.frame_num = 7;
        let _ = decoder.dpb.push(reference);
        decoder.picture.frame_num = 8;

        let finished = decoder
            .finish(Cropping::default(), Duration::ZERO, &RefMarking::None)
            .expect("finish");

        assert_eq!(finished.dpb.len(), 1);
        assert_eq!(finished.dpb.get(0).unwrap().frame_num, 7);
        assert_eq!(finished.recycled[0].frame_num, 8);
    }

    #[test]
    fn adaptive_mmco_marks_prior_reference_unused() {
        let mut decoder = PictureDecoder::new(config());
        let mut prior = Picture::new(1, 1);
        prior.frame_num = 0;
        let _ = decoder.dpb.push(prior);
        decoder.picture.frame_num = 1;

        let marking = RefMarking::Adaptive(vec![MmcoOp::ShortTermUnused {
            difference_of_pic_nums_minus1: 0,
        }]);
        let finished = decoder
            .finish(Cropping::default(), Duration::ZERO, &marking)
            .expect("finish");

        assert_eq!(finished.dpb.len(), 1);
        assert_eq!(finished.dpb.get(0).unwrap().frame_num, 1);
        assert_eq!(finished.recycled.len(), 1);
        assert_eq!(finished.recycled[0].frame_num, 0);
    }
}
