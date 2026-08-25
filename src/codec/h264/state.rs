//! Per-macroblock decoded state for the picture in progress.
//!
//! Decoding a macroblock is not a function of the bitstream alone. Nearly
//! every syntax element in it is coded against its neighbours: motion vectors
//! against the predicted vector, intra modes against the modes above and to
//! the left, and CABAC's context selection against a dozen different
//! properties of the macroblocks already decoded. This module is what those
//! lookups read.
//!
//! It stores only what a *later* macroblock, or the deblocking filter, will
//! ask for. Reconstructed samples live in the picture buffer, not here, and
//! anything consumed and finished with inside a single macroblock stays local
//! to the decode loop.
//!
//! # Substitutions
//!
//! Several fields carry a defined value for macroblocks that do not really
//! have one — intra modes on an inter macroblock, motion vectors on an intra
//! one. Those are the substitutions the spec itself specifies for exactly
//! this case, applied once at write time. Doing it here rather than at each
//! of the several read sites is what keeps the prediction code from being a
//! thicket of availability special-cases.

use super::mb::{MbType, Partitioning};
use super::neighbour::{BlockRef, MbAddr, Neighbourhood};

/// The intra 4x4 prediction mode the spec substitutes for a macroblock that
/// has no intra modes of its own. Spec 8.3.1.1.
pub const DC_PRED_MODE: u8 = 2;

/// Reference index meaning "no reference": the macroblock is intra coded, or
/// is not available at all. Spec 8.4.1.3 treats the two identically.
pub const NO_REF: i8 = -1;

/// What a decoded macroblock left behind for its neighbours.
///
/// Fixed-size and `Copy`: one per macroblock of the picture, allocated once.
/// A 1080p picture holds around 8100 of these, so the arrays are sized rather
/// than boxed.
#[derive(Debug, Clone, Copy)]
pub struct MbInfo {
    pub mb_type: MbType,
    /// `transform_size_8x8_flag`. Only meaningful for `I_NxN` and inter
    /// macroblocks; false everywhere else.
    pub transform_8x8: bool,
    /// The luma quantiser this macroblock was decoded at, which deblocking
    /// averages across each edge.
    pub qp: u8,
    /// `CodedBlockPatternLuma`, one bit per 8x8 block.
    pub cbp_luma: u8,
    /// `CodedBlockPatternChroma`: 0, 1 or 2.
    pub cbp_chroma: u8,
    /// `intra_chroma_pred_mode`, which is the CABAC context for the next
    /// macroblock's. Zero on macroblocks that do not code one, which is the
    /// value the spec's context derivation substitutes for them anyway.
    pub chroma_mode: u8,
    /// Whether `mb_qp_delta` was non-zero, which is the context for the next
    /// macroblock's delta.
    pub qp_delta_nonzero: bool,
    /// Intra prediction mode per 4x4 luma block, in the quadrant-major scan.
    /// [`DC_PRED_MODE`] on macroblocks that are not `I_NxN`; for `I_8x8` each
    /// 8x8 mode is replicated across its four blocks, which is what the
    /// spec's prediction rule reads.
    pub intra_modes: [u8; 16],
    /// Motion vector per 4x4 luma block, in quarter-pel units. Zero on intra
    /// macroblocks.
    pub mv: [[i16; 2]; 16],
    /// Coded motion vector difference per 4x4 luma block. Kept separately
    /// from `mv` because CABAC's context for the next `mvd` is the magnitude
    /// of the neighbouring *differences*, not of the vectors themselves.
    pub mvd: [[i16; 2]; 16],
    /// Reference index per 8x8 partition. [`NO_REF`] on intra macroblocks.
    pub ref_idx: [i8; 4],
    /// `coded_block_flag` per 4x4 luma block, one bit each in scan order.
    pub cbf_luma: u16,
    /// `coded_block_flag` for the `I_16x16` luma DC block.
    pub cbf_luma_dc: bool,
    /// `coded_block_flag` per chroma component's DC block.
    pub cbf_chroma_dc: [bool; 2],
    /// `coded_block_flag` per chroma component, one bit per 4x4 AC block.
    pub cbf_chroma_ac: [u8; 2],
}

impl MbInfo {
    /// The state a macroblock leaves behind when it is skipped.
    ///
    /// No residual, no coded vector, no intra mode. The motion vectors are
    /// filled in afterwards from the skip prediction, since a skipped
    /// macroblock still moves and still deblocks against its neighbours.
    pub fn skipped() -> Self {
        Self {
            mb_type: MbType::PSkip,
            transform_8x8: false,
            qp: 0,
            cbp_luma: 0,
            cbp_chroma: 0,
            chroma_mode: 0,
            qp_delta_nonzero: false,
            intra_modes: [DC_PRED_MODE; 16],
            mv: [[0; 2]; 16],
            mvd: [[0; 2]; 16],
            ref_idx: [0; 4],
            cbf_luma: 0,
            cbf_luma_dc: false,
            cbf_chroma_dc: [false; 2],
            cbf_chroma_ac: [0; 2],
        }
    }

    /// A blank macroblock of the given type, before its syntax is decoded.
    pub fn new(mb_type: MbType, qp: u8) -> Self {
        let intra = mb_type.is_intra();
        Self {
            mb_type,
            qp,
            ref_idx: [if intra { NO_REF } else { 0 }; 4],
            ..Self::skipped()
        }
    }

    pub fn is_intra(&self) -> bool {
        self.mb_type.is_intra()
    }

    pub fn is_skipped(&self) -> bool {
        self.mb_type == MbType::PSkip
    }

    /// Whether this macroblock predicts with `I_NxN`, and so has real intra
    /// modes for a neighbour to predict from.
    pub fn is_intra_nxn(&self) -> bool {
        self.mb_type == MbType::IntraNxN
    }

    /// The reference index covering a 4x4 luma block.
    pub fn ref_idx_of_block(&self, blk: u8) -> i8 {
        // Both indices are quadrant-major, so the 8x8 partition containing a
        // 4x4 block is just its index divided by four.
        self.ref_idx[blk as usize / 4]
    }

    /// `coded_block_flag` for a 4x4 luma block.
    pub fn luma_cbf(&self, blk: u8) -> bool {
        self.cbf_luma >> blk & 1 == 1
    }

    pub fn set_luma_cbf(&mut self, blk: u8, set: bool) {
        let bit = 1 << blk;
        if set {
            self.cbf_luma |= bit;
        } else {
            self.cbf_luma &= !bit;
        }
    }

    /// `coded_block_flag` for a 4x4 chroma AC block of component `comp`.
    pub fn chroma_cbf(&self, comp: usize, blk: u8) -> bool {
        self.cbf_chroma_ac[comp] >> blk & 1 == 1
    }

    pub fn set_chroma_cbf(&mut self, comp: usize, blk: u8, set: bool) {
        let bit = 1 << blk;
        if set {
            self.cbf_chroma_ac[comp] |= bit;
        } else {
            self.cbf_chroma_ac[comp] &= !bit;
        }
    }

    /// Whether the 8x8 luma block at `blk` has any coded coefficients.
    pub fn luma_8x8_coded(&self, blk: u8) -> bool {
        self.cbp_luma >> blk & 1 == 1
    }

    /// Sets every 4x4 block's motion vector and the partition's reference
    /// index over a rectangle of the macroblock.
    ///
    /// Motion is decoded per partition but read per 4x4 block — by the next
    /// macroblock's vector prediction and by deblocking, neither of which
    /// knows or cares how this one was partitioned. Expanding once at write
    /// time is cheaper than reconstructing the partition layout at each read.
    pub fn fill_motion(&mut self, x: usize, y: usize, width: usize, height: usize, mv: [i16; 2]) {
        for by in (y..y + height).step_by(4) {
            for bx in (x..x + width).step_by(4) {
                self.mv[super::neighbour::luma_4x4_index(bx, by) as usize] = mv;
            }
        }
    }

    /// As [`Self::fill_motion`], for the coded difference.
    pub fn fill_mvd(&mut self, x: usize, y: usize, width: usize, height: usize, mvd: [i16; 2]) {
        for by in (y..y + height).step_by(4) {
            for bx in (x..x + width).step_by(4) {
                self.mvd[super::neighbour::luma_4x4_index(bx, by) as usize] = mvd;
            }
        }
    }

    /// Sets the reference index across the 8x8 partitions a rectangle covers.
    pub fn fill_ref_idx(&mut self, x: usize, y: usize, width: usize, height: usize, ref_idx: i8) {
        for by in (y..y + height).step_by(8) {
            for bx in (x..x + width).step_by(8) {
                self.ref_idx[super::neighbour::luma_8x8_index(bx, by) as usize] = ref_idx;
            }
        }
    }
}

/// Decoded state for every macroblock of the picture, alongside the
/// availability map that says which of it may be read.
///
/// The two are one type because they are never useful apart: a neighbour's
/// state is meaningless without knowing whether the neighbour is available,
/// and reading it anyway is precisely the bug the availability rules exist to
/// prevent.
#[derive(Debug, Clone)]
pub struct PictureState {
    pub neighbours: Neighbourhood,
    mbs: Vec<MbInfo>,
}

impl PictureState {
    pub fn new(width_mbs: usize, height_mbs: usize) -> Self {
        Self {
            neighbours: Neighbourhood::new(width_mbs, height_mbs),
            mbs: vec![MbInfo::skipped(); width_mbs * height_mbs],
        }
    }

    /// Starts a new picture, discarding all macroblock state.
    pub fn begin_picture(&mut self) {
        self.neighbours.begin_picture();
        // The contents are never read before being written, since
        // availability gates every read, but resetting keeps a state from one
        // picture from being mistaken for this one's while debugging.
        self.mbs.fill(MbInfo::skipped());
    }

    /// Marks a macroblock as belonging to `slice_id`, before decoding it.
    ///
    /// Availability compares slices, so a macroblock has to be on the map
    /// before its own neighbour queries will answer: until it is, it has no
    /// slice to compare its neighbours against and every one of them looks
    /// like it belongs to a different slice.
    pub fn begin_macroblock(&mut self, addr: MbAddr, slice_id: u32) {
        self.neighbours.begin_macroblock(addr, slice_id);
    }

    /// Records a decoded macroblock and marks it available.
    pub fn put(&mut self, addr: MbAddr, slice_id: u32, info: MbInfo) {
        self.mbs[addr] = info;
        self.neighbours.begin_macroblock(addr, slice_id);
    }

    /// The macroblock at `addr`, without an availability check.
    ///
    /// For the decode loop's own macroblock and for callers that have already
    /// established availability; prefer [`Self::available`] otherwise.
    pub fn get(&self, addr: MbAddr) -> &MbInfo {
        &self.mbs[addr]
    }

    pub fn get_mut(&mut self, addr: MbAddr) -> &mut MbInfo {
        &mut self.mbs[addr]
    }

    /// The macroblock at `addr`, if it may be read when decoding `curr`.
    pub fn available(&self, addr: MbAddr, curr: MbAddr) -> Option<&MbInfo> {
        self.neighbours
            .available(addr, curr)
            .then(|| &self.mbs[addr])
    }

    /// The state a neighbouring block reference points at.
    pub fn at(&self, block: BlockRef) -> &MbInfo {
        &self.mbs[block.mb]
    }

    /// The motion of a neighbouring 4x4 block, in the form vector prediction
    /// wants it.
    ///
    /// `None` for an unavailable neighbour; an intra neighbour yields
    /// [`NO_REF`], which [`super::inter::predict_mv`] treats as a zero vector
    /// against no reference.
    ///
    /// `curr` and `cur` name the macroblock currently being decoded. A
    /// partition predicts from the partition to its left, and for the
    /// right-hand partitions of a macroblock that is one this same macroblock
    /// decoded moments ago — which is not in `self` yet, because a macroblock
    /// is only stored once all of it has been decoded. They are parameters
    /// rather than an optional convenience so that no caller can silently
    /// predict from the previous picture's macroblock instead.
    pub fn motion(
        &self,
        block: Option<BlockRef>,
        curr: MbAddr,
        cur: &MbInfo,
    ) -> super::inter::Neighbour {
        let Some(block) = block else {
            return super::inter::Neighbour::UNAVAILABLE;
        };
        let info = if block.mb == curr { cur } else { self.at(block) };
        if info.is_intra() {
            return super::inter::Neighbour::UNAVAILABLE;
        }
        super::inter::Neighbour {
            mv: (
                info.mv[block.blk as usize][0] as i32,
                info.mv[block.blk as usize][1] as i32,
            ),
            ref_idx: info.ref_idx_of_block(block.blk),
        }
    }

    /// The intra prediction mode of a neighbouring 4x4 block, as spec 8.3.1.1
    /// wants it.
    ///
    /// A neighbour that is unavailable, or that is inter coded while
    /// `constrained_intra_pred_flag` is set, forces DC. The unavailable case
    /// additionally forces the *current* block to DC, which the caller
    /// handles: the two situations produce the same substituted mode but
    /// differ in whether the mode may be predicted at all.
    pub fn intra_mode(&self, block: Option<BlockRef>, constrained_intra: bool) -> Option<u8> {
        let block = block?;
        let info = self.at(block);
        if !info.is_intra() && constrained_intra {
            return Some(DC_PRED_MODE);
        }
        Some(info.intra_modes[block.blk as usize])
    }
}

/// The 4x4 luma block index of the sub-partition covering a partition's
/// top-left corner.
///
/// Motion vector prediction reads its neighbours relative to the partition
/// being predicted, not to the macroblock, so it needs the partition's corner
/// as a block index. Spec 6.4.2.1.
pub fn partition_block(partitioning: Partitioning, part: usize) -> u8 {
    let p = partitioning.parts()[part];
    super::neighbour::luma_4x4_index(p.x, p.y)
}

#[cfg(test)]
mod tests {
    use super::super::intra::Intra16x16Mode;
    use super::*;

    fn inter_mb() -> MbInfo {
        MbInfo::new(MbType::Inter(Partitioning::P16x16), 26)
    }

    fn intra_mb() -> MbInfo {
        MbInfo::new(
            MbType::Intra16x16 {
                mode: Intra16x16Mode::Dc,
                cbp_luma: 0,
                cbp_chroma: 0,
            },
            26,
        )
    }

    #[test]
    fn an_intra_macroblock_has_no_reference_and_an_inter_one_has_reference_zero() {
        assert_eq!(intra_mb().ref_idx, [NO_REF; 4]);
        assert_eq!(inter_mb().ref_idx, [0; 4]);
        assert!(intra_mb().is_intra());
        assert!(!inter_mb().is_intra());
    }

    #[test]
    fn a_skipped_macroblock_carries_no_residual_and_no_intra_mode() {
        let mb = MbInfo::skipped();
        assert!(mb.is_skipped());
        assert_eq!(mb.cbp_luma, 0);
        assert_eq!(mb.cbp_chroma, 0);
        assert_eq!(mb.cbf_luma, 0);
        assert_eq!(mb.intra_modes, [DC_PRED_MODE; 16]);
        // Skip is an inter mode, so it still references picture zero.
        assert!(!mb.is_intra());
        assert_eq!(mb.ref_idx, [0; 4]);
    }

    #[test]
    fn coded_block_flags_round_trip_per_block() {
        let mut mb = inter_mb();
        for blk in 0..16 {
            assert!(!mb.luma_cbf(blk));
            mb.set_luma_cbf(blk, true);
            assert!(mb.luma_cbf(blk));
        }
        assert_eq!(mb.cbf_luma, u16::MAX);
        mb.set_luma_cbf(7, false);
        assert!(!mb.luma_cbf(7));
        assert!(mb.luma_cbf(6) && mb.luma_cbf(8));

        for comp in 0..2 {
            for blk in 0..4 {
                mb.set_chroma_cbf(comp, blk, true);
                assert!(mb.chroma_cbf(comp, blk));
            }
        }
        assert_eq!(mb.cbf_chroma_ac, [0b1111; 2]);
    }

    /// A 4x4 block's reference index comes from the 8x8 partition containing
    /// it, and both scans are quadrant-major so the mapping is a division.
    #[test]
    fn a_blocks_reference_index_comes_from_its_partition() {
        let mut mb = MbInfo::new(MbType::Inter(Partitioning::P8x8), 26);
        mb.ref_idx = [0, 1, 2, 3];
        for blk in 0..16u8 {
            assert_eq!(mb.ref_idx_of_block(blk), (blk / 4) as i8);
        }
        // And the partition indices really are the macroblock quadrants.
        assert_eq!(super::super::neighbour::luma_4x4_index(8, 0) / 4, 1);
        assert_eq!(super::super::neighbour::luma_4x4_index(0, 8) / 4, 2);
    }

    #[test]
    fn filling_motion_covers_exactly_the_partition() {
        let mut mb = inter_mb();
        // The left half of an 8x16 split.
        mb.fill_motion(0, 0, 8, 16, [4, -8]);
        for blk in 0..16u8 {
            let (x, _) = super::super::neighbour::luma_4x4_origin(blk);
            let expected = if x < 8 { [4, -8] } else { [0, 0] };
            assert_eq!(mb.mv[blk as usize], expected, "block {blk}");
        }
    }

    #[test]
    fn filling_a_reference_index_covers_the_partitions_it_spans() {
        let mut mb = MbInfo::new(MbType::Inter(Partitioning::P16x8), 26);
        mb.fill_ref_idx(0, 0, 16, 8, 2);
        // The top 16x8 partition spans both upper 8x8 quadrants.
        assert_eq!(mb.ref_idx, [2, 2, 0, 0]);
    }

    #[test]
    fn a_four_by_four_sub_partition_fills_only_its_own_block() {
        let mut mb = inter_mb();
        mb.fill_mvd(4, 4, 4, 4, [1, 2]);
        let blk = super::super::neighbour::luma_4x4_index(4, 4);
        for i in 0..16 {
            let expected = if i == blk as usize { [1, 2] } else { [0, 0] };
            assert_eq!(mb.mvd[i], expected, "block {i}");
        }
    }

    #[test]
    fn partition_corners_map_to_the_right_block_index() {
        assert_eq!(partition_block(Partitioning::P16x16, 0), 0);
        assert_eq!(partition_block(Partitioning::P16x8, 1), 8);
        assert_eq!(partition_block(Partitioning::P8x16, 1), 4);
        let corners: Vec<u8> = (0..4)
            .map(|p| partition_block(Partitioning::P8x8, p))
            .collect();
        assert_eq!(corners, vec![0, 4, 8, 12]);
    }

    #[test]
    fn state_is_unreadable_until_the_macroblock_is_decoded() {
        let mut state = PictureState::new(4, 3);
        state.put(0, 0, intra_mb());
        state.put(1, 0, inter_mb());
        assert!(state.available(0, 1).is_some());
        // Macroblock 2 has not been decoded.
        assert!(state.available(2, 1).is_none());
    }

    #[test]
    fn an_intra_neighbour_offers_no_motion() {
        let mut state = PictureState::new(4, 3);
        let mut inter = inter_mb();
        inter.fill_motion(0, 0, 16, 16, [12, -4]);
        state.put(0, 0, intra_mb());
        state.put(1, 0, inter);

        // Macroblock 2 stands in for the one being decoded; none of these
        // neighbours belong to it, so what it holds does not matter.
        let decoding = MbInfo::skipped();
        let intra_block = BlockRef { mb: 0, blk: 5 };
        assert_eq!(
            state.motion(Some(intra_block), 2, &decoding),
            super::super::inter::Neighbour::UNAVAILABLE
        );
        assert_eq!(
            state.motion(None, 2, &decoding),
            super::super::inter::Neighbour::UNAVAILABLE
        );

        let inter_block = BlockRef { mb: 1, blk: 5 };
        let motion = state.motion(Some(inter_block), 2, &decoding);
        assert_eq!(motion.mv, (12, -4));
        assert_eq!(motion.ref_idx, 0);
    }

    /// The partition being predicted from may be one this same macroblock
    /// decoded a moment ago, which is not in the picture state yet.
    #[test]
    fn the_macroblock_being_decoded_is_read_from_its_own_state() {
        let state = PictureState::new(4, 3);
        let mut decoding = inter_mb();
        decoding.fill_motion(0, 0, 8, 16, [-7, 3]);

        let own_block = BlockRef { mb: 1, blk: 1 };
        let motion = state.motion(Some(own_block), 1, &decoding);
        assert_eq!(motion.mv, (-7, 3));
    }

    /// Under constrained intra prediction an inter neighbour's mode is
    /// replaced by DC rather than being used, which is the whole point of the
    /// flag: intra macroblocks must not depend on inter-coded data.
    #[test]
    fn constrained_intra_prediction_substitutes_dc_for_inter_neighbours() {
        let mut state = PictureState::new(4, 3);
        let mut nxn = MbInfo::new(MbType::IntraNxN, 26);
        nxn.intra_modes = [7; 16];
        state.put(0, 0, nxn);
        state.put(1, 0, inter_mb());

        let from_intra = Some(BlockRef { mb: 0, blk: 0 });
        let from_inter = Some(BlockRef { mb: 1, blk: 0 });
        assert_eq!(state.intra_mode(from_intra, true), Some(7));
        assert_eq!(state.intra_mode(from_inter, true), Some(DC_PRED_MODE));
        // Unconstrained, the inter macroblock's substituted DC is used as-is.
        assert_eq!(state.intra_mode(from_inter, false), Some(DC_PRED_MODE));
        assert_eq!(state.intra_mode(None, false), None);
    }

    #[test]
    fn a_new_picture_makes_every_macroblock_unavailable_again() {
        let mut state = PictureState::new(4, 3);
        state.put(0, 0, intra_mb());
        state.begin_picture();
        state.put(1, 0, inter_mb());
        assert!(state.available(0, 1).is_none());
    }
}
