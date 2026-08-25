//! Picture-level deblocking: the driver over [`super::deblock`]'s primitives.
//!
//! The filter is *in loop*, which fixes when it may run. Reconstruction reads
//! unfiltered neighbours for intra prediction, so nothing here may touch a
//! sample until the whole picture is reconstructed; motion compensation reads
//! filtered references, so every sample must be touched before the picture
//! enters the DPB. Between those two points is the only correct moment, and
//! [`super::picture_decode::PictureDecoder::finish`] is where it is.
//!
//! Edge order is not an implementation detail either. Macroblocks are filtered
//! in raster order, and within one macroblock every vertical edge is filtered
//! left to right before any horizontal edge is filtered top to bottom. Later
//! edges read samples earlier ones already modified, so any other order
//! produces different output — not merely a different rounding, but a
//! divergence that motion compensation then carries into every later picture.

use super::deblock::{
    boundary_strength, filter_chroma_edge, filter_luma_edge, Edge, EdgeBlock, EdgeKind, Thresholds,
};
use super::mb::chroma_qp;
use super::neighbour::{luma_4x4_index, MbAddr};
use super::picture::Picture;
use super::slice::Deblocking;
use super::state::{MbInfo, PictureState};

/// Everything the filter needs that is not per-macroblock.
pub struct FilterParams<'a> {
    /// Deblocking control per slice, indexed by the slice id the decode loop
    /// used. A picture can mix slices that filter with slices that do not.
    pub slices: &'a [Deblocking],
    /// `chroma_qp_index_offset` for Cb and Cr.
    pub chroma_qp_offset: [i32; 2],
}

/// Applies the in-loop deblocking filter to a fully reconstructed picture.
pub fn filter_picture(picture: &mut Picture, state: &PictureState, params: &FilterParams<'_>) {
    for addr in 0..state.neighbours.len() {
        let Some(control) = slice_control(state, params, addr) else {
            continue;
        };
        if !control.enabled() {
            continue;
        }
        filter_macroblock(picture, state, params, addr, control);
    }
}

/// The deblocking control governing the macroblock at `addr`.
///
/// `None` for a macroblock no slice ever claimed, which happens when a picture
/// arrives with slices missing. Those samples are painted mid-grey by
/// [`super::picture::Picture::grey_uncovered`] before the filter runs; leaving
/// them unfiltered is better than filtering them against a neighbour state
/// that was never written for this picture.
fn slice_control(
    state: &PictureState,
    params: &FilterParams<'_>,
    addr: MbAddr,
) -> Option<Deblocking> {
    let slice = state.neighbours.slice_of(addr)?;
    params.slices.get(slice as usize).copied()
}

fn filter_macroblock(
    picture: &mut Picture,
    state: &PictureState,
    params: &FilterParams<'_>,
    addr: MbAddr,
    control: Deblocking,
) {
    let n = &state.neighbours;
    let cur = state.get(addr);
    let (mb_x, mb_y) = (n.mb_x(addr), n.mb_y(addr));

    // A macroblock edge is filtered only when the neighbour exists and was
    // claimed by a slice — an unclaimed hole has no usable MbInfo — and,
    // when `disable_deblocking_filter_idc` is 2, only when it belongs to this
    // same slice.
    let across = |neighbour: Option<MbAddr>| -> Option<MbAddr> {
        let neighbour = neighbour?;
        let ns = n.slice_of(neighbour)?;
        if control.crosses_slices() || Some(ns) == n.slice_of(addr) {
            Some(neighbour)
        } else {
            None
        }
    };
    let left = across((mb_x > 0).then(|| addr - 1));
    let above = across((mb_y > 0).then(|| addr - n.width_mbs()));

    for kind in [EdgeKind::Vertical, EdgeKind::Horizontal] {
        let outer = match kind {
            EdgeKind::Vertical => left,
            EdgeKind::Horizontal => above,
        };
        for e in 0..4usize {
            // Only the outermost edge crosses into another macroblock, and
            // when that neighbour is unavailable the edge is not filtered at
            // all rather than filtered against this macroblock's own samples.
            let other = if e == 0 {
                match outer {
                    Some(addr) => addr,
                    None => continue,
                }
            } else {
                addr
            };
            let bs = edge_strengths(state, addr, other, kind, e);
            if bs.iter().all(|&b| b == 0) {
                continue;
            }
            // An 8x8 transform has no transform-block boundary at 4 or 12, so
            // those internal edges do not exist to be filtered. Chroma is
            // unaffected: 4:2:0 chroma always uses the 4x4 transform, and its
            // two edges line up with luma 0 and 8 regardless.
            if e == 0 || !cur.transform_8x8 || e % 2 == 0 {
                let t = Thresholds::new(
                    average_qp(cur.qp, state.get(other).qp),
                    control.alpha_offset,
                    control.beta_offset,
                );
                let (x, y) = edge_origin(kind, mb_x * 16, mb_y * 16, e * 4);
                filter_luma_edge(
                    &mut picture.planes[0],
                    picture.strides[0],
                    &Edge {
                        kind,
                        x,
                        y,
                        length: 16,
                        bs: &bs,
                    },
                    &t,
                );
            }
            if e % 2 != 0 {
                continue;
            }
            for comp in 0..2 {
                let offset = params.chroma_qp_offset[comp];
                let t = Thresholds::new(
                    average_qp(
                        chroma_qp(cur.qp, offset),
                        chroma_qp(state.get(other).qp, offset),
                    ),
                    control.alpha_offset,
                    control.beta_offset,
                );
                let (x, y) = edge_origin(kind, mb_x * 8, mb_y * 8, e * 2);
                filter_chroma_edge(
                    &mut picture.planes[1 + comp],
                    picture.strides[1 + comp],
                    &Edge {
                        kind,
                        x,
                        y,
                        length: 8,
                        bs: &bs,
                    },
                    &t,
                );
            }
        }
    }
}

/// Spec 8.7.2.2: the filter works from the mean of the two quantisers, rounded
/// up, so a coarsely quantised neighbour widens the filter on both sides.
fn average_qp(p: u8, q: u8) -> i32 {
    (i32::from(p) + i32::from(q) + 1) >> 1
}

/// Sample origin of the `q` side of edge `e` within a macroblock.
fn edge_origin(kind: EdgeKind, mb_x: usize, mb_y: usize, offset: usize) -> (usize, usize) {
    match kind {
        EdgeKind::Vertical => (mb_x + offset, mb_y),
        EdgeKind::Horizontal => (mb_x, mb_y + offset),
    }
}

/// Boundary strength for each of the four 4x4 groups along one edge.
///
/// Strength is derived from luma block state even for the chroma edges, which
/// is why it is computed once per edge and shared: chroma has no coded block
/// pattern or motion of its own.
fn edge_strengths(
    state: &PictureState,
    addr: MbAddr,
    other: MbAddr,
    kind: EdgeKind,
    e: usize,
) -> [u8; 4] {
    let cur = state.get(addr);
    let neighbour = state.get(other);
    let mut bs = [0u8; 4];
    for (group, strength) in bs.iter_mut().enumerate() {
        // `q` is always inside this macroblock; `p` is the block one step back
        // across the edge, which for the outermost edge is the last row or
        // column of the neighbouring macroblock.
        let back = if e == 0 { 12 } else { (e - 1) * 4 };
        let (p_blk, q_blk) = match kind {
            EdgeKind::Vertical => (
                luma_4x4_index(back, group * 4),
                luma_4x4_index(e * 4, group * 4),
            ),
            EdgeKind::Horizontal => (
                luma_4x4_index(group * 4, back),
                luma_4x4_index(group * 4, e * 4),
            ),
        };
        *strength = boundary_strength(
            edge_block(neighbour, p_blk),
            edge_block(cur, q_blk),
            e == 0,
        );
    }
    bs
}

fn edge_block(info: &MbInfo, blk: u8) -> EdgeBlock {
    let mv = info.mv[blk as usize];
    EdgeBlock {
        intra: info.is_intra(),
        has_coeffs: info.luma_cbf(blk),
        mv: (i32::from(mv[0]), i32::from(mv[1])),
        ref_idx: info.ref_idx_of_block(blk),
    }
}
