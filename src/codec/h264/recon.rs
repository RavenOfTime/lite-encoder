//! Macroblock reconstruction: prediction plus residual, into the picture.
//!
//! The syntax layer decides *what* a macroblock is; this decides what it
//! looks like. Prediction is formed from already-reconstructed neighbours
//! (intra) or from a reference picture (inter), the residual is dequantised
//! and inverse transformed, and the two are added with clipping.
//!
//! # Order matters
//!
//! Intra prediction reads reconstructed samples, not predicted ones, so a
//! block's residual has to be added before the next block predicts from it.
//! That is why reconstruction walks blocks in decoding order and writes
//! straight into the picture rather than assembling a macroblock off to one
//! side. The deblocking filter, by contrast, must *not* run until the whole
//! picture is reconstructed, since it reads across macroblock edges in both
//! directions.
//!
//! # Speed
//!
//! Sample availability is resolved per sample rather than per edge, and
//! prediction goes through the general routines in [`super::intra`] and
//! [`super::inter`] even where a specialised path would be far quicker. That
//! follows the same reasoning as the six-tap filter in `inter`: match the
//! spec's structure closely enough to audit, and optimise once real streams
//! decode correctly.

use super::intra::{self, Intra16x16Mode, IntraChromaMode, IntraNxNMode, Neighbours};
use super::mb::{self, MbType};
use super::neighbour::{luma_4x4_index, luma_4x4_origin, luma_8x8_origin, MbAddr};
use super::picture::{Dpb, Picture};
use super::residual::{ZIGZAG_4X4, ZIGZAG_8X8};
use super::state::{MbInfo, PictureState};
use super::transform;
use crate::Error;

/// The dequantisation weights in force, from the sequence and picture
/// parameter sets.
///
/// Six 4x4 lists and two 8x8 ones, indexed as the spec indexes them: intra
/// then inter, luma then Cb then Cr. Cameras almost always send flat lists,
/// but a non-flat list changes every coefficient in the picture, so it cannot
/// be assumed away.
#[derive(Debug, Clone)]
pub struct ScalingLists {
    pub weight_4x4: [[u8; 16]; 6],
    /// Luma only: 4:2:0 High profile defines just the intra and inter luma
    /// 8x8 lists.
    pub weight_8x8: [[u8; 64]; 2],
}

impl Default for ScalingLists {
    fn default() -> Self {
        Self {
            weight_4x4: [transform::FLAT_WEIGHT_SCALE_4X4; 6],
            weight_8x8: [transform::FLAT_WEIGHT_SCALE_8X8; 2],
        }
    }
}

impl ScalingLists {
    /// The 4x4 list for a plane, spec table 7-2 ordering.
    fn list_4x4(&self, intra: bool, plane: usize) -> &[u8; 16] {
        &self.weight_4x4[if intra { plane } else { 3 + plane }]
    }

    fn list_8x8(&self, intra: bool) -> &[u8; 64] {
        &self.weight_8x8[usize::from(!intra)]
    }
}

/// Picture-wide parameters reconstruction needs.
#[derive(Debug, Clone)]
pub struct ReconParams {
    pub scaling: ScalingLists,
    /// `chroma_qp_index_offset` and `second_chroma_qp_index_offset`, per
    /// component. High profile lets the two chroma planes be quantised
    /// differently, which is why this is a pair and not one value.
    pub chroma_qp_offset: [i32; 2],
    pub constrained_intra: bool,
}

/// One macroblock's decoded coefficient levels, before dequantisation.
///
/// Held as a block rather than streamed into the picture because the syntax
/// order and the reconstruction order differ: every coefficient of the
/// macroblock is coded before any of it can be reconstructed, since intra
/// prediction of the second block needs the first block's *final* samples.
#[derive(Debug, Clone)]
pub struct Residual {
    /// The `I_16x16` luma DC block, in scan order.
    pub luma_dc: [i32; 16],
    /// Per 4x4 luma block, in scan order. For `I_16x16` these are AC only and
    /// position 0 is unused.
    pub luma: [[i32; 16]; 16],
    /// Per 8x8 luma block, when `transform_size_8x8_flag` is set.
    pub luma_8x8: [[i32; 64]; 4],
    /// Per component, the four chroma DC coefficients.
    pub chroma_dc: [[i32; 4]; 2],
    /// Per component, per 4x4 chroma block: AC coefficients only.
    pub chroma: [[[i32; 16]; 4]; 2],
}

impl Default for Residual {
    fn default() -> Self {
        Self {
            luma_dc: [0; 16],
            luma: [[0; 16]; 16],
            luma_8x8: [[0; 64]; 4],
            chroma_dc: [[0; 4]; 2],
            chroma: [[[0; 16]; 4]; 2],
        }
    }
}

/// Reconstructs one macroblock into `picture`.
///
/// `info` must already hold the macroblock's decoded prediction data: type,
/// intra modes, motion vectors and reference indices.
pub fn reconstruct(
    picture: &mut Picture,
    state: &PictureState,
    dpb: &Dpb,
    addr: MbAddr,
    info: &MbInfo,
    residual: &Residual,
    params: &ReconParams,
) -> Result<(), Error> {
    if info.is_intra() {
        predict_intra_luma(picture, state, addr, info, residual, params)?;
    } else {
        predict_inter(picture, dpb, state, addr, info)?;
        add_luma_residual(picture, state, addr, info, residual, params);
    }
    reconstruct_chroma(picture, state, dpb, addr, info, residual, params)?;
    Ok(())
}

// -- Intra luma -----------------------------------------------------------

fn predict_intra_luma(
    picture: &mut Picture,
    state: &PictureState,
    addr: MbAddr,
    info: &MbInfo,
    residual: &Residual,
    params: &ReconParams,
) -> Result<(), Error> {
    let (mb_x, mb_y) = state.neighbours.origin(addr);

    match info.mb_type {
        MbType::Intra16x16 { mode, .. } => {
            let mut pred = [0u8; 256];
            let mut top = [0u8; 32];
            let mut left = [0u8; 16];
            let n = gather_luma_neighbours(picture, state, addr, 0, 0, 16, 16, params, &mut top, &mut left);
            if !intra::predict_16x16(mode, &n, &mut pred) {
                return Err(unavailable(mode));
            }
            blit(picture, 0, mb_x, mb_y, 16, 16, &pred);
            add_intra_16x16_residual(picture, addr, state, info, residual, params);
        }
        MbType::IntraNxN if info.transform_8x8 => {
            for blk in 0..4u8 {
                let (bx, by) = luma_8x8_origin(blk);
                let mode = nxn_mode(info.intra_modes[blk as usize * 4])?;
                let mut pred = [0u8; 64];
                let mut top = [0u8; 32];
                let mut left = [0u8; 16];
                let n =
                    gather_luma_neighbours(picture, state, addr, bx, by, 8, blk * 4, params, &mut top, &mut left);
                if !intra::predict_8x8(mode, &n, &mut pred) {
                    return Err(unavailable(mode));
                }
                blit(picture, 0, mb_x + bx, mb_y + by, 8, 8, &pred);

                if info.luma_8x8_coded(blk) {
                    let mut block = dezigzag_8x8(&residual.luma_8x8[blk as usize]);
                    transform::dequant_8x8(&mut block, info.qp, params.scaling.list_8x8(true));
                    transform::inverse_8x8(&mut block);
                    let offset = (mb_y + by) * picture.strides[0] + mb_x + bx;
                    transform::add_residual_8x8(&mut picture.planes[0], offset, picture.strides[0], &block);
                }
            }
        }
        MbType::IntraNxN => {
            for blk in 0..16u8 {
                let (bx, by) = luma_4x4_origin(blk);
                let mode = nxn_mode(info.intra_modes[blk as usize])?;
                let mut pred = [0u8; 16];
                let mut top = [0u8; 32];
                let mut left = [0u8; 16];
                let n = gather_luma_neighbours(picture, state, addr, bx, by, 4, blk, params, &mut top, &mut left);
                if !intra::predict_4x4(mode, &n, &mut pred) {
                    return Err(unavailable(mode));
                }
                blit(picture, 0, mb_x + bx, mb_y + by, 4, 4, &pred);

                if info.luma_cbf(blk) {
                    let mut block = dezigzag_4x4(&residual.luma[blk as usize]);
                    transform::dequant_4x4(&mut block, info.qp, params.scaling.list_4x4(true, 0), false);
                    transform::inverse_4x4(&mut block);
                    let offset = (mb_y + by) * picture.strides[0] + mb_x + bx;
                    transform::add_residual_4x4(&mut picture.planes[0], offset, picture.strides[0], &block);
                }
            }
        }
        // I_PCM writes its samples directly and has no residual; the slice
        // layer does that before calling here.
        _ => {}
    }
    Ok(())
}

/// Adds the residual of an `I_16x16` macroblock, whose DC coefficients went
/// through a second transform of their own.
fn add_intra_16x16_residual(
    picture: &mut Picture,
    addr: MbAddr,
    state: &PictureState,
    info: &MbInfo,
    residual: &Residual,
    params: &ReconParams,
) {
    let (mb_x, mb_y) = state.neighbours.origin(addr);
    let weights = params.scaling.list_4x4(true, 0);

    // The DC stage runs once for the whole macroblock; its outputs become
    // position 0 of each 4x4 block.
    let mut dc = [0i32; 16];
    for (scan, &level) in residual.luma_dc.iter().enumerate() {
        dc[ZIGZAG_4X4[scan]] = level;
    }
    transform::dequant_luma_dc(&mut dc, info.qp, weights);

    for blk in 0..16u8 {
        let (bx, by) = luma_4x4_origin(blk);
        let mut block = dezigzag_4x4(&residual.luma[blk as usize]);
        transform::dequant_4x4(&mut block, info.qp, weights, true);
        // The DC values are indexed by 4x4 block in the quadrant-major scan,
        // laid out as a 4x4 grid of blocks.
        block[0] = dc[dc_index(blk)];
        transform::inverse_4x4(&mut block);
        let offset = (mb_y + by) * picture.strides[0] + mb_x + bx;
        transform::add_residual_4x4(&mut picture.planes[0], offset, picture.strides[0], &block);
    }
}

/// Where a 4x4 block's DC coefficient sits in the 4x4 DC array.
///
/// The DC array is a raster of the sixteen blocks, but the blocks themselves
/// are numbered quadrant-major, so this is a re-scan and not the identity.
fn dc_index(blk: u8) -> usize {
    let (x, y) = luma_4x4_origin(blk);
    (y / 4) * 4 + x / 4
}

// -- Inter ----------------------------------------------------------------

fn predict_inter(
    picture: &mut Picture,
    dpb: &Dpb,
    state: &PictureState,
    addr: MbAddr,
    info: &MbInfo,
) -> Result<(), Error> {
    let (mb_x, mb_y) = state.neighbours.origin(addr);

    // Prediction is per 4x4 block rather than per partition. The result is
    // identical — every block of a partition shares its vector — and it means
    // this does not need to know how the macroblock was partitioned.
    for blk in 0..16u8 {
        let (bx, by) = luma_4x4_origin(blk);
        let ref_idx = info.ref_idx_of_block(blk);
        let reference = dpb.get(ref_idx.max(0) as usize).ok_or_else(|| {
            Error::Decode(format!(
                "macroblock {addr} references picture {ref_idx}, which is not in the buffer"
            ))
        })?;
        let mv = (
            i32::from(info.mv[blk as usize][0]),
            i32::from(info.mv[blk as usize][1]),
        );

        let mut pred = [0u8; 16];
        super::inter::predict_luma(
            &reference.luma(),
            (mb_x + bx) as i32,
            (mb_y + by) as i32,
            mv,
            4,
            4,
            &mut pred,
        );
        blit(picture, 0, mb_x + bx, mb_y + by, 4, 4, &pred);
    }
    Ok(())
}

fn add_luma_residual(
    picture: &mut Picture,
    state: &PictureState,
    addr: MbAddr,
    info: &MbInfo,
    residual: &Residual,
    params: &ReconParams,
) {
    let (mb_x, mb_y) = state.neighbours.origin(addr);
    let stride = picture.strides[0];

    if info.transform_8x8 {
        for blk in 0..4u8 {
            if !info.luma_8x8_coded(blk) {
                continue;
            }
            let (bx, by) = luma_8x8_origin(blk);
            let mut block = dezigzag_8x8(&residual.luma_8x8[blk as usize]);
            transform::dequant_8x8(&mut block, info.qp, params.scaling.list_8x8(false));
            transform::inverse_8x8(&mut block);
            transform::add_residual_8x8(
                &mut picture.planes[0],
                (mb_y + by) * stride + mb_x + bx,
                stride,
                &block,
            );
        }
        return;
    }

    for blk in 0..16u8 {
        if !info.luma_cbf(blk) {
            continue;
        }
        let (bx, by) = luma_4x4_origin(blk);
        let mut block = dezigzag_4x4(&residual.luma[blk as usize]);
        transform::dequant_4x4(&mut block, info.qp, params.scaling.list_4x4(false, 0), false);
        transform::inverse_4x4(&mut block);
        transform::add_residual_4x4(
            &mut picture.planes[0],
            (mb_y + by) * stride + mb_x + bx,
            stride,
            &block,
        );
    }
}

// -- Chroma ---------------------------------------------------------------

fn reconstruct_chroma(
    picture: &mut Picture,
    state: &PictureState,
    dpb: &Dpb,
    addr: MbAddr,
    info: &MbInfo,
    residual: &Residual,
    params: &ReconParams,
) -> Result<(), Error> {
    if info.mb_type == MbType::IPcm {
        return Ok(());
    }
    let (mb_x, mb_y) = state.neighbours.origin(addr);
    let (cx0, cy0) = (mb_x / 2, mb_y / 2);

    for comp in 0..2 {
        let plane = comp + 1;
        let stride = picture.strides[plane];

        if info.is_intra() {
            let mode = IntraChromaMode::from_id(info.chroma_mode)
                .ok_or_else(|| Error::Decode(format!("chroma mode {}", info.chroma_mode)))?;
            let mut pred = [0u8; 64];
            let mut top = [0u8; 32];
            let mut left = [0u8; 16];
            let n = gather_chroma_neighbours(picture, state, addr, plane, params, &mut top, &mut left);
            if !intra::predict_chroma_8x8(mode, &n, &mut pred) {
                return Err(Error::Decode(format!(
                    "chroma mode {mode:?} needs neighbours that macroblock {addr} does not have"
                )));
            }
            blit(picture, plane, cx0, cy0, 8, 8, &pred);
        } else {
            for blk in 0..4u8 {
                let (bx, by) = ((blk as usize % 2) * 4, (blk as usize / 2) * 4);
                // Chroma uses the luma block's vector; the co-located 4x4
                // luma block of this 4x4 chroma block is its 8x8 quadrant.
                let luma_blk = luma_4x4_index(bx * 2, by * 2);
                let ref_idx = info.ref_idx_of_block(luma_blk);
                let reference = dpb.get(ref_idx.max(0) as usize).ok_or_else(|| {
                    Error::Decode(format!("macroblock {addr} references picture {ref_idx}"))
                })?;
                let mv = (
                    i32::from(info.mv[luma_blk as usize][0]),
                    i32::from(info.mv[luma_blk as usize][1]),
                );
                let mut pred = [0u8; 16];
                super::inter::predict_chroma(
                    &reference.chroma(comp),
                    (cx0 + bx) as i32,
                    (cy0 + by) as i32,
                    mv,
                    4,
                    4,
                    &mut pred,
                );
                blit(picture, plane, cx0 + bx, cy0 + by, 4, 4, &pred);
            }
        }

        if info.cbp_chroma == 0 {
            continue;
        }

        let qp = mb::chroma_qp(info.qp, params.chroma_qp_offset[comp]);
        let weights = params.scaling.list_4x4(info.is_intra(), comp + 1);

        let mut dc = residual.chroma_dc[comp];
        transform::dequant_chroma_dc(&mut dc, qp, weights);

        for blk in 0..4usize {
            // Chroma DC is always coded; AC only when the pattern says 2.
            let mut block = if info.cbp_chroma == 2 {
                dezigzag_4x4(&residual.chroma[comp][blk])
            } else {
                [0; 16]
            };
            transform::dequant_4x4(&mut block, qp, weights, true);
            block[0] = dc[blk];
            transform::inverse_4x4(&mut block);
            let (bx, by) = ((blk % 2) * 4, (blk / 2) * 4);
            transform::add_residual_4x4(
                &mut picture.planes[plane],
                (cy0 + by) * stride + cx0 + bx,
                stride,
                &block,
            );
        }
    }
    Ok(())
}

// -- Neighbour samples ----------------------------------------------------

/// Gathers the reference samples an intra luma prediction reads.
///
/// `blk` is the 4x4 block index the prediction is for, used to reject
/// neighbours inside this macroblock that have not been reconstructed yet.
/// For a 16x16 prediction it is 0, which rejects the whole macroblock, as it
/// should: an `I_16x16` prediction reads only outside itself.
#[allow(clippy::too_many_arguments)]
fn gather_luma_neighbours<'buf>(
    picture: &Picture,
    state: &PictureState,
    addr: MbAddr,
    bx: usize,
    by: usize,
    size: usize,
    blk: u8,
    params: &ReconParams,
    top: &'buf mut [u8; 32],
    left: &'buf mut [u8; 16],
) -> Neighbours<'buf> {
    let sample = |x: i32, y: i32| intra_sample(picture, state, addr, x, y, blk, params, 0);

    // The top row, plus the top-right group when it exists. Supplying only
    // `size` samples tells the predictor to replicate the last one, which is
    // the substitution the spec makes for an absent top-right.
    let has_top = sample(bx as i32, by as i32 - 1).is_some();
    let mut top_len = 0;
    if has_top {
        for i in 0..size {
            top[i] = sample(bx as i32 + i as i32, by as i32 - 1).unwrap_or(0);
        }
        top_len = size;
        if (0..size).all(|i| sample((bx + size + i) as i32, by as i32 - 1).is_some()) {
            for i in 0..size {
                top[size + i] = sample((bx + size + i) as i32, by as i32 - 1).unwrap_or(0);
            }
            top_len = 2 * size;
        }
    }

    let has_left = sample(bx as i32 - 1, by as i32).is_some();
    if has_left {
        for i in 0..size {
            left[i] = sample(bx as i32 - 1, by as i32 + i as i32).unwrap_or(0);
        }
    }

    Neighbours {
        top: has_top.then(|| &top[..top_len]),
        left: has_left.then(|| &left[..size]),
        top_left: sample(bx as i32 - 1, by as i32 - 1),
    }
}

/// The chroma counterpart. A chroma prediction covers the whole macroblock,
/// so its neighbours are always in other macroblocks.
fn gather_chroma_neighbours<'buf>(
    picture: &Picture,
    state: &PictureState,
    addr: MbAddr,
    plane: usize,
    params: &ReconParams,
    top: &'buf mut [u8; 32],
    left: &'buf mut [u8; 16],
) -> Neighbours<'buf> {
    let sample = |x: i32, y: i32| chroma_sample(picture, state, addr, x, y, params, plane);

    let has_top = sample(0, -1).is_some();
    if has_top {
        for i in 0..8 {
            top[i] = sample(i as i32, -1).unwrap_or(0);
        }
    }
    let has_left = sample(-1, 0).is_some();
    if has_left {
        for i in 0..8 {
            left[i] = sample(-1, i as i32).unwrap_or(0);
        }
    }

    Neighbours {
        top: has_top.then(|| &top[..8]),
        left: has_left.then(|| &left[..8]),
        top_left: sample(-1, -1),
    }
}

/// One reconstructed luma sample, if intra prediction may read it.
///
/// `(x, y)` are relative to the current macroblock and may be negative.
/// Returns `None` when the sample is in an unavailable macroblock, in a block
/// of this macroblock that has not been reconstructed yet, or — under
/// `constrained_intra_pred_flag` — in an inter-coded macroblock.
fn intra_sample(
    picture: &Picture,
    state: &PictureState,
    addr: MbAddr,
    x: i32,
    y: i32,
    blk: u8,
    params: &ReconParams,
    plane: usize,
) -> Option<u8> {
    let at = state.neighbours.luma_location(addr, x, y)?;
    if at.mb == addr {
        // Inside this macroblock: readable only if that block came earlier.
        if luma_4x4_index(at.x, at.y) >= blk {
            return None;
        }
    } else if params.constrained_intra && !state.at(at_ref(at.mb)).is_intra() {
        return None;
    }

    let (mb_x, mb_y) = state.neighbours.origin(at.mb);
    let stride = picture.strides[plane];
    Some(picture.planes[plane][(mb_y + at.y) * stride + mb_x + at.x])
}

fn chroma_sample(
    picture: &Picture,
    state: &PictureState,
    addr: MbAddr,
    x: i32,
    y: i32,
    params: &ReconParams,
    plane: usize,
) -> Option<u8> {
    let at = state.neighbours.chroma_location(addr, x, y)?;
    if at.mb == addr {
        return None;
    }
    if params.constrained_intra && !state.at(at_ref(at.mb)).is_intra() {
        return None;
    }
    let (mb_x, mb_y) = state.neighbours.origin(at.mb);
    let stride = picture.strides[plane];
    Some(picture.planes[plane][(mb_y / 2 + at.y) * stride + mb_x / 2 + at.x])
}

fn at_ref(mb: MbAddr) -> super::neighbour::BlockRef {
    super::neighbour::BlockRef { mb, blk: 0 }
}

// -- Helpers --------------------------------------------------------------

/// Writes a predicted block into a picture plane.
fn blit(picture: &mut Picture, plane: usize, x: usize, y: usize, w: usize, h: usize, src: &[u8]) {
    let stride = picture.strides[plane];
    for row in 0..h {
        let dst = (y + row) * stride + x;
        picture.planes[plane][dst..dst + w].copy_from_slice(&src[row * w..row * w + w]);
    }
}

/// Scan order to raster order for a 4x4 block.
///
/// `levels` is indexed by scan position throughout, including for the
/// categories whose DC was coded in a separate block: those leave index 0
/// empty rather than packing the AC coefficients down onto it, so there is no
/// shift to apply here. Applying one anyway moves every AC coefficient a
/// position further along the scan, which turns a horizontal frequency into a
/// vertical one — invisible on symmetric content, and badly wrong on a
/// vertical edge.
fn dezigzag_4x4(levels: &[i32; 16]) -> [i32; 16] {
    let mut block = [0i32; 16];
    for (scan, &level) in levels.iter().enumerate() {
        block[ZIGZAG_4X4[scan]] = level;
    }
    block
}

fn dezigzag_8x8(levels: &[i32; 64]) -> [i32; 64] {
    let mut block = [0i32; 64];
    for (scan, &level) in levels.iter().enumerate() {
        block[ZIGZAG_8X8[scan]] = level;
    }
    block
}

fn nxn_mode(id: u8) -> Result<IntraNxNMode, Error> {
    IntraNxNMode::from_id(id).ok_or_else(|| Error::Decode(format!("intra mode {id} is not defined")))
}

fn unavailable(mode: impl std::fmt::Debug) -> Error {
    Error::Decode(format!(
        "intra mode {mode:?} needs neighbouring samples that are not available"
    ))
}

/// So that `Intra16x16Mode` can go through [`unavailable`] too.
const _: fn(Intra16x16Mode) -> Error = unavailable;

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::mb::Partitioning;

    fn params() -> ReconParams {
        ReconParams {
            scaling: ScalingLists::default(),
            chroma_qp_offset: [0; 2],
            constrained_intra: false,
        }
    }

    /// De-zigzagging places every level at its scan position, and a block
    /// whose DC was coded separately — which reaches here with scan position 0
    /// left empty — must come out with the DC position still empty rather than
    /// with its AC coefficients shifted along by one.
    #[test]
    fn dezigzagging_places_every_level_at_its_scan_position() {
        let levels: [i32; 16] = std::array::from_fn(|i| i as i32 + 1);
        let block = dezigzag_4x4(&levels);
        assert_eq!(block.iter().filter(|&&v| v != 0).count(), 16);
        assert_eq!(block[0], 1);

        let mut ac = levels;
        ac[0] = 0;
        let ac = dezigzag_4x4(&ac);
        assert_eq!(ac[0], 0, "an AC block must not write the DC position");
        assert_eq!(ac.iter().filter(|&&v| v != 0).count(), 15);
        // Scan position 1 is the first horizontal frequency, raster index 1.
        // Landing on raster index 4 instead would make it a vertical one.
        assert_eq!(ac[1], 2);

        let levels: [i32; 64] = std::array::from_fn(|i| i as i32 + 1);
        assert_eq!(dezigzag_8x8(&levels).iter().filter(|&&v| v != 0).count(), 64);
    }

    /// The DC array is a raster of blocks while the blocks themselves are
    /// numbered quadrant-major, so the mapping is a permutation and not the
    /// identity.
    #[test]
    fn the_luma_dc_index_is_a_permutation_of_the_block_scan() {
        let indices: Vec<usize> = (0..16u8).map(dc_index).collect();
        let mut sorted = indices.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 16);
        // Block 1 is to the right of block 0, but block 4 opens the next
        // quadrant rather than continuing the raster row.
        assert_eq!(indices[0], 0);
        assert_eq!(indices[1], 1);
        assert_eq!(indices[2], 4);
        assert_eq!(indices[4], 2);
    }

    /// An intra macroblock at the top-left of a picture has no neighbours, so
    /// only the modes that need none can be used. DC is the one that always
    /// works, which is why it is the substituted default everywhere.
    #[test]
    fn the_first_macroblock_predicts_dc_from_nothing() {
        let mut picture = Picture::new(2, 2);
        let mut state = PictureState::new(2, 2);
        state.begin_macroblock(0, 0);

        let info = MbInfo::new(
            MbType::Intra16x16 {
                mode: Intra16x16Mode::Dc,
                cbp_luma: 0,
                cbp_chroma: 0,
            },
            26,
        );
        let dpb = Dpb::new(1, 16);
        reconstruct(
            &mut picture,
            &state,
            &dpb,
            0,
            &info,
            &Residual::default(),
            &params(),
        )
        .expect("reconstruct");

        // With no neighbours, DC prediction is the midpoint of the range.
        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(picture.planes[0][y * picture.strides[0] + x], 128);
            }
        }
    }

    /// Vertical prediction at the top edge has nothing above it, so it cannot
    /// be used there. The stream is malformed rather than the decoder being
    /// unable, and it must say so instead of predicting from zeroes.
    #[test]
    fn a_mode_without_its_neighbours_is_rejected() {
        let mut picture = Picture::new(2, 2);
        let mut state = PictureState::new(2, 2);
        state.begin_macroblock(0, 0);

        let info = MbInfo::new(
            MbType::Intra16x16 {
                mode: Intra16x16Mode::Vertical,
                cbp_luma: 0,
                cbp_chroma: 0,
            },
            26,
        );
        let dpb = Dpb::new(1, 16);
        let err = reconstruct(
            &mut picture,
            &state,
            &dpb,
            0,
            &info,
            &Residual::default(),
            &params(),
        )
        .expect_err("vertical prediction cannot work at the top edge");
        assert!(format!("{err}").contains("not available"), "{err}");
    }

    /// Inter prediction with a zero vector is a straight copy of the
    /// reference, which is the case that has to be exactly right before any
    /// fractional position can be.
    #[test]
    fn a_zero_vector_copies_the_reference_exactly() {
        let mut reference = Picture::new(2, 2);
        for (i, sample) in reference.planes[0].iter_mut().enumerate() {
            *sample = (i % 251) as u8;
        }
        let mut dpb = Dpb::new(1, 16);
        dpb.push(reference.clone());

        let mut picture = Picture::new(2, 2);
        let mut state = PictureState::new(2, 2);
        state.begin_macroblock(1, 0);

        let info = MbInfo::new(MbType::Inter(Partitioning::P16x16), 26);
        reconstruct(
            &mut picture,
            &state,
            &dpb,
            1,
            &info,
            &Residual::default(),
            &params(),
        )
        .expect("reconstruct");

        let stride = picture.strides[0];
        for y in 0..16 {
            for x in 16..32 {
                assert_eq!(
                    picture.planes[0][y * stride + x],
                    reference.planes[0][y * stride + x],
                    "sample ({x}, {y})"
                );
            }
        }
    }

    /// An inter macroblock with no reference picture is a stream error, not a
    /// reason to predict from grey.
    #[test]
    fn inter_prediction_without_a_reference_is_an_error() {
        let mut picture = Picture::new(2, 2);
        let mut state = PictureState::new(2, 2);
        state.begin_macroblock(0, 0);
        let dpb = Dpb::new(1, 16);
        let info = MbInfo::new(MbType::Inter(Partitioning::P16x16), 26);

        let err = reconstruct(
            &mut picture,
            &state,
            &dpb,
            0,
            &info,
            &Residual::default(),
            &params(),
        )
        .expect_err("there is no reference picture");
        assert!(format!("{err}").contains("not in the buffer"), "{err}");
    }

    /// The scaling list selection follows table 7-2: intra lists first, then
    /// inter, luma before the chroma planes.
    #[test]
    fn scaling_lists_are_selected_by_prediction_mode_and_plane() {
        let mut scaling = ScalingLists::default();
        for (i, list) in scaling.weight_4x4.iter_mut().enumerate() {
            list[0] = i as u8;
        }
        assert_eq!(scaling.list_4x4(true, 0)[0], 0);
        assert_eq!(scaling.list_4x4(true, 2)[0], 2);
        assert_eq!(scaling.list_4x4(false, 0)[0], 3);
        assert_eq!(scaling.list_4x4(false, 2)[0], 5);

        scaling.weight_8x8[1][0] = 9;
        assert_eq!(scaling.list_8x8(true)[0], 16);
        assert_eq!(scaling.list_8x8(false)[0], 9);
    }
}
