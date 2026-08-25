//! Macroblock addressing and neighbour derivation (spec clause 6.4).
//!
//! This is the layer everything above it reads from. Macroblock type decode,
//! intra mode prediction, motion vector prediction, CABAC's `ctxIdxInc`, the
//! coded-block-pattern context and the deblocking filter's boundary strength
//! all ask the same two questions: *which block is to my left and above me*,
//! and *am I allowed to look at it*. Getting either wrong does not produce an
//! obviously broken picture, it produces a subtly wrong one, so the rules live
//! in one place with the spec's own coordinate conventions rather than being
//! open-coded five times.
//!
//! # Availability
//!
//! A neighbour is available only if it exists within the picture, belongs to
//! the same slice, and has already been decoded. Slices are independently
//! decodable by design — that is the whole point of them — so a macroblock at
//! the top of a slice has no neighbour above it even though one sits there in
//! the picture. Every accessor here returns `Option`, and `None` always means
//! "not available" in the spec's sense rather than "out of bounds".
//!
//! # Scope
//!
//! Frame macroblocks only, and one slice group. Interlacing would double
//! every derivation in clause 6.4 (each has a separate MBAFF branch), and
//! flexible macroblock ordering would replace raster neighbour arithmetic
//! with a slice-group map. Both are rejected at the parameter-set stage; see
//! [`super`].

/// A macroblock address: its index in raster scan order across the picture.
pub type MbAddr = usize;

/// A neighbouring block, wherever it turned out to live.
///
/// The point of the type is that `blk` is an index *within `mb`*, not within
/// the macroblock that asked. Callers reading a neighbour's intra mode or
/// motion vector need both halves, and pairing them here removes the
/// commonest way to get this wrong: indexing the neighbour's array with the
/// current macroblock's block index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRef {
    pub mb: MbAddr,
    pub blk: u8,
}

/// A neighbouring sample, in the coordinate system of the macroblock that
/// turned out to contain it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleRef {
    pub mb: MbAddr,
    /// Spec `xW`: 0..15 for luma, 0..7 for 4:2:0 chroma.
    pub x: usize,
    /// Spec `yW`.
    pub y: usize,
}

/// Which macroblocks of the current picture have been decoded, and by which
/// slice, so that neighbour availability can be answered.
#[derive(Debug, Clone)]
pub struct Neighbourhood {
    width: usize,
    height: usize,
    /// Slice each macroblock belongs to; `None` until it is decoded.
    ///
    /// Decoded-ness and slice membership are one field because availability
    /// needs both and they can never disagree: a macroblock acquires a slice
    /// exactly when decoding of it begins.
    slice: Vec<Option<u32>>,
}

impl Neighbourhood {
    /// `width` and `height` are in macroblocks (spec `PicWidthInMbs` and
    /// `FrameHeightInMbs`).
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            slice: vec![None; width * height],
        }
    }

    pub fn width_mbs(&self) -> usize {
        self.width
    }

    pub fn height_mbs(&self) -> usize {
        self.height
    }

    /// Spec `PicSizeInMbs`.
    pub fn len(&self) -> usize {
        self.slice.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slice.is_empty()
    }

    /// Forgets every macroblock, for the start of a new picture.
    ///
    /// Availability is per-picture: a decoder that carried this over would
    /// happily predict from the previous frame's macroblocks and produce a
    /// picture that is wrong in a way no single-frame test would catch.
    pub fn begin_picture(&mut self) {
        self.slice.fill(None);
    }

    /// Records that `addr` is being decoded as part of slice `slice_id`.
    ///
    /// Call this before decoding the macroblock, not after: the macroblock
    /// layer asks about its own neighbours while decoding, and those queries
    /// need the current macroblock's slice to compare against.
    pub fn begin_macroblock(&mut self, addr: MbAddr, slice_id: u32) {
        self.slice[addr] = Some(slice_id);
    }

    /// The slice a decoded macroblock belongs to, or `None` if it is not
    /// decoded.
    pub fn slice_of(&self, addr: MbAddr) -> Option<u32> {
        self.slice.get(addr).copied().flatten()
    }

    /// Spec 6.4.8: whether `addr` may be used when decoding `curr`.
    pub fn available(&self, addr: MbAddr, curr: MbAddr) -> bool {
        addr < self.len()
            && self.slice_of(addr).is_some()
            && self.slice_of(addr) == self.slice_of(curr)
    }

    /// Column of `addr`, spec `mbAddrX % PicWidthInMbs`.
    pub fn mb_x(&self, addr: MbAddr) -> usize {
        addr % self.width
    }

    /// Row of `addr`.
    pub fn mb_y(&self, addr: MbAddr) -> usize {
        addr / self.width
    }

    /// Spec 6.4.1: the luma sample coordinates of a macroblock's top-left
    /// corner within the picture.
    pub fn origin(&self, addr: MbAddr) -> (usize, usize) {
        (self.mb_x(addr) * 16, self.mb_y(addr) * 16)
    }

    /// Spec 6.4.11.1 `mbAddrA`: the macroblock to the left.
    pub fn mb_a(&self, curr: MbAddr) -> Option<MbAddr> {
        // A macroblock in column zero has no left neighbour, and the raster
        // address that would be one is the last macroblock of the row above.
        (self.mb_x(curr) > 0)
            .then(|| curr - 1)
            .filter(|&n| self.available(n, curr))
    }

    /// Spec 6.4.11.1 `mbAddrB`: the macroblock above.
    pub fn mb_b(&self, curr: MbAddr) -> Option<MbAddr> {
        curr.checked_sub(self.width)
            .filter(|&n| self.available(n, curr))
    }

    /// Spec 6.4.11.1 `mbAddrC`: the macroblock above and to the right.
    pub fn mb_c(&self, curr: MbAddr) -> Option<MbAddr> {
        (self.mb_x(curr) + 1 < self.width)
            .then(|| curr.checked_sub(self.width - 1))
            .flatten()
            .filter(|&n| self.available(n, curr))
    }

    /// Spec 6.4.11.1 `mbAddrD`: the macroblock above and to the left.
    pub fn mb_d(&self, curr: MbAddr) -> Option<MbAddr> {
        (self.mb_x(curr) > 0)
            .then(|| curr.checked_sub(self.width + 1))
            .flatten()
            .filter(|&n| self.available(n, curr))
    }

    /// Spec 6.4.12, table 6-3: resolve a luma location that may fall outside
    /// the current macroblock.
    ///
    /// `x` and `y` are relative to the current macroblock's top-left luma
    /// sample and may be negative or past its edge; the result names whichever
    /// macroblock actually contains that sample, with the position rewritten
    /// into *its* coordinates.
    pub fn luma_location(&self, curr: MbAddr, x: i32, y: i32) -> Option<SampleRef> {
        self.location(curr, x, y, 16)
    }

    /// Spec 6.4.12 for chroma. 4:2:0 only, so the macroblock is 8x8 chroma
    /// samples and the table is otherwise identical.
    pub fn chroma_location(&self, curr: MbAddr, x: i32, y: i32) -> Option<SampleRef> {
        self.location(curr, x, y, 8)
    }

    fn location(&self, curr: MbAddr, x: i32, y: i32, size: i32) -> Option<SampleRef> {
        let mb = match (x < 0, x >= size, y < 0, y >= size) {
            // Below the macroblock, or right of it on the same rows: those
            // samples are not yet decoded, whichever macroblock holds them.
            (_, _, _, true) | (_, true, false, false) => return None,
            (true, _, true, _) => self.mb_d(curr)?,
            (true, _, false, _) => self.mb_a(curr)?,
            (false, false, true, _) => self.mb_b(curr)?,
            (false, true, true, _) => self.mb_c(curr)?,
            _ => curr,
        };
        Some(SampleRef {
            mb,
            // Wraps a one-macroblock overshoot in either direction back into
            // range; the match above has already rejected anything further.
            x: (x + size) as usize % size as usize,
            y: (y + size) as usize % size as usize,
        })
    }

    /// Spec 6.4.11.4: the 4x4 luma block containing the sample at `(dx, dy)`
    /// relative to the top-left corner of block `blk`.
    ///
    /// The conventional offsets are A `(-1, 0)`, B `(0, -1)`, C `(4, -1)` and
    /// D `(-1, -1)`; [`Self::luma_4x4_a`] and friends spell those out.
    pub fn luma_4x4_neighbour(&self, curr: MbAddr, blk: u8, dx: i32, dy: i32) -> Option<BlockRef> {
        let (bx, by) = luma_4x4_origin(blk);
        let at = self.luma_location(curr, bx as i32 + dx, by as i32 + dy)?;
        let found = luma_4x4_index(at.x, at.y);
        self.decoded_before(at.mb, found, curr, blk)
    }

    /// Block A: the 4x4 luma block to the left.
    pub fn luma_4x4_a(&self, curr: MbAddr, blk: u8) -> Option<BlockRef> {
        self.luma_4x4_neighbour(curr, blk, -1, 0)
    }

    /// Block B: the 4x4 luma block above.
    pub fn luma_4x4_b(&self, curr: MbAddr, blk: u8) -> Option<BlockRef> {
        self.luma_4x4_neighbour(curr, blk, 0, -1)
    }

    /// Block C: the 4x4 luma block above and to the right.
    ///
    /// Frequently unavailable even well inside a picture, because for half the
    /// block indices it names a block later in decoding order. Motion vector
    /// prediction has a documented substitution for that case (spec 8.4.1.3.2,
    /// replace C with D); it is the caller's, since only the caller knows
    /// whether the substitution applies.
    pub fn luma_4x4_c(&self, curr: MbAddr, blk: u8) -> Option<BlockRef> {
        self.luma_4x4_neighbour(curr, blk, 4, -1)
    }

    /// Block D: the 4x4 luma block above and to the left.
    pub fn luma_4x4_d(&self, curr: MbAddr, blk: u8) -> Option<BlockRef> {
        self.luma_4x4_neighbour(curr, blk, -1, -1)
    }

    /// Spec 6.4.11.7: a neighbouring 4x4 block of a *partition*.
    ///
    /// Motion vector prediction asks for the neighbours of a partition, which
    /// is not the same question as the neighbours of its top-left 4x4 block.
    /// A and B coincide either way, but C sits at `(x + width, y - 1)`: for a
    /// 16x16 partition that is the macroblock above and to the right, whereas
    /// [`Self::luma_4x4_c`] would name a block inside the macroblock directly
    /// above. Using the wrong one predicts from the wrong vector, which
    /// decodes to a wrong picture without any error to notice it by.
    ///
    /// `x` and `y` locate the partition within the current macroblock.
    pub fn luma_partition_neighbour(
        &self,
        curr: MbAddr,
        x: usize,
        y: usize,
        dx: i32,
        dy: i32,
    ) -> Option<BlockRef> {
        let at = self.luma_location(curr, x as i32 + dx, y as i32 + dy)?;
        let found = luma_4x4_index(at.x, at.y);
        self.decoded_before(at.mb, found, curr, luma_4x4_index(x, y))
    }

    /// Spec 6.4.11.2, for the 8x8 luma blocks the transform-size-8x8 path and
    /// the coded block pattern work in.
    pub fn luma_8x8_neighbour(&self, curr: MbAddr, blk: u8, dx: i32, dy: i32) -> Option<BlockRef> {
        let (bx, by) = luma_8x8_origin(blk);
        let at = self.luma_location(curr, bx as i32 + dx, by as i32 + dy)?;
        let found = luma_8x8_index(at.x, at.y);
        self.decoded_before(at.mb, found, curr, blk)
    }

    /// Spec 6.4.11.5, for the four 4x4 blocks of one 4:2:0 chroma component.
    pub fn chroma_4x4_neighbour(
        &self,
        curr: MbAddr,
        blk: u8,
        dx: i32,
        dy: i32,
    ) -> Option<BlockRef> {
        let (bx, by) = chroma_4x4_origin(blk);
        let at = self.chroma_location(curr, bx as i32 + dx, by as i32 + dy)?;
        let found = chroma_4x4_index(at.x, at.y);
        self.decoded_before(at.mb, found, curr, blk)
    }

    /// Rejects a block inside the current macroblock that has not been decoded
    /// yet.
    ///
    /// Block scan order *is* decoding order for all three scans here, so
    /// "later" is just a larger index. This only ever bites within the current
    /// macroblock: any other macroblock the location derivation returned has
    /// already been established as available, and therefore fully decoded.
    fn decoded_before(&self, mb: MbAddr, found: u8, curr: MbAddr, blk: u8) -> Option<BlockRef> {
        (mb != curr || found < blk).then_some(BlockRef { mb, blk: found })
    }
}

/// Spec 6.4.3: the luma sample offset of a 4x4 block within its macroblock.
///
/// The scan is not raster. Blocks are grouped into 8x8 quadrants taken in
/// raster order, and the four blocks inside each quadrant are then taken in
/// raster order themselves, so block 2 sits below block 0 rather than to the
/// right of block 1. Everything indexed by `luma4x4BlkIdx` — residual
/// coefficients, intra modes, motion vectors — follows this order, which is
/// why it gets a named function rather than being inlined.
pub fn luma_4x4_origin(blk: u8) -> (usize, usize) {
    let (quadrant, within) = (blk as usize / 4, blk as usize % 4);
    (
        (quadrant % 2) * 8 + (within % 2) * 4,
        (quadrant / 2) * 8 + (within / 2) * 4,
    )
}

/// Spec 6.4.13.1: the inverse of [`luma_4x4_origin`], for any sample within
/// the block rather than only its corner.
pub fn luma_4x4_index(x: usize, y: usize) -> u8 {
    (8 * (y / 8) + 4 * (x / 8) + 2 * ((y % 8) / 4) + (x % 8) / 4) as u8
}

/// Spec 6.4.5: the luma sample offset of an 8x8 block. Plain raster order.
pub fn luma_8x8_origin(blk: u8) -> (usize, usize) {
    ((blk as usize % 2) * 8, (blk as usize / 2) * 8)
}

/// Spec 6.4.13.2: the 8x8 block containing a luma sample.
pub fn luma_8x8_index(x: usize, y: usize) -> u8 {
    (2 * (y / 8) + x / 8) as u8
}

/// The chroma sample offset of a 4:2:0 chroma 4x4 block. Plain raster order.
pub fn chroma_4x4_origin(blk: u8) -> (usize, usize) {
    ((blk as usize % 2) * 4, (blk as usize / 2) * 4)
}

/// Spec 6.4.13.3: the chroma 4x4 block containing a chroma sample.
pub fn chroma_4x4_index(x: usize, y: usize) -> u8 {
    (2 * (y / 4) + x / 4) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4x3 picture with every macroblock decoded into slice 0, which is the
    /// uninteresting case the interesting ones are measured against.
    fn full() -> Neighbourhood {
        let mut n = Neighbourhood::new(4, 3);
        for addr in 0..n.len() {
            n.begin_macroblock(addr, 0);
        }
        n
    }

    #[test]
    fn the_scan_order_of_4x4_luma_blocks_round_trips() {
        for blk in 0..16u8 {
            let (x, y) = luma_4x4_origin(blk);
            assert_eq!(luma_4x4_index(x, y), blk, "block {blk} at ({x}, {y})");
            // Any sample inside the block must map back to it, not just the
            // corner: the residual and deblocking paths both index by
            // arbitrary sample positions.
            for (dx, dy) in [(0, 0), (3, 0), (0, 3), (3, 3), (2, 1)] {
                assert_eq!(luma_4x4_index(x + dx, y + dy), blk);
            }
        }
    }

    /// The scan is quadrant-major, not raster. Spelling out the layout catches
    /// an implementation that is self-consistent but scanning the wrong way.
    #[test]
    fn the_4x4_luma_scan_is_grouped_into_8x8_quadrants() {
        let corners: Vec<(usize, usize)> = (0..16).map(luma_4x4_origin).collect();
        assert_eq!(corners[0], (0, 0));
        assert_eq!(corners[1], (4, 0));
        assert_eq!(corners[2], (0, 4));
        assert_eq!(corners[3], (4, 4));
        // Block 4 opens the top-right quadrant, not the second raster row.
        assert_eq!(corners[4], (8, 0));
        assert_eq!(corners[8], (0, 8));
        assert_eq!(corners[15], (12, 12));
    }

    #[test]
    fn the_8x8_and_chroma_scans_round_trip() {
        for blk in 0..4u8 {
            let (x, y) = luma_8x8_origin(blk);
            assert_eq!(luma_8x8_index(x, y), blk);
            assert_eq!(luma_8x8_index(x + 7, y + 7), blk);

            let (x, y) = chroma_4x4_origin(blk);
            assert_eq!(chroma_4x4_index(x, y), blk);
            assert_eq!(chroma_4x4_index(x + 3, y + 3), blk);
        }
    }

    #[test]
    fn an_interior_macroblock_has_all_four_neighbours() {
        let n = full();
        // Macroblock 5 is at column 1, row 1.
        assert_eq!(n.mb_a(5), Some(4));
        assert_eq!(n.mb_b(5), Some(1));
        assert_eq!(n.mb_c(5), Some(2));
        assert_eq!(n.mb_d(5), Some(0));
    }

    #[test]
    fn the_left_edge_has_no_left_or_above_left_neighbour() {
        let n = full();
        // Macroblock 4 opens row 1; macroblock 3 sits to its left in raster
        // address order but is in the previous row of the picture.
        assert_eq!(n.mb_a(4), None);
        assert_eq!(n.mb_d(4), None);
        assert_eq!(n.mb_b(4), Some(0));
        assert_eq!(n.mb_c(4), Some(1));
    }

    #[test]
    fn the_right_edge_has_no_above_right_neighbour() {
        let n = full();
        // Macroblock 7 ends row 1; macroblock 4 is not above-right of it.
        assert_eq!(n.mb_c(7), None);
        assert_eq!(n.mb_b(7), Some(3));
        assert_eq!(n.mb_a(7), Some(6));
    }

    #[test]
    fn the_first_macroblock_of_a_picture_has_no_neighbours() {
        let mut n = Neighbourhood::new(4, 3);
        n.begin_macroblock(0, 0);
        assert_eq!(
            (n.mb_a(0), n.mb_b(0), n.mb_c(0), n.mb_d(0)),
            (None, None, None, None)
        );
    }

    #[test]
    fn undecoded_macroblocks_are_unavailable() {
        let mut n = Neighbourhood::new(4, 3);
        // Decode the first row and the first macroblock of the second.
        for addr in 0..=4 {
            n.begin_macroblock(addr, 0);
        }
        n.begin_macroblock(5, 0);
        assert_eq!(n.mb_a(5), Some(4));
        // Macroblock 6 exists in the picture but has not been reached.
        assert_eq!(n.mb_a(7), None);
    }

    /// Slices are independently decodable, so a neighbour in another slice is
    /// as unavailable as one outside the picture.
    #[test]
    fn neighbours_in_another_slice_are_unavailable() {
        let mut n = Neighbourhood::new(4, 3);
        for addr in 0..4 {
            n.begin_macroblock(addr, 0);
        }
        for addr in 4..8 {
            n.begin_macroblock(addr, 1);
        }
        // Within slice 1, the left neighbour is fine.
        assert_eq!(n.mb_a(5), Some(4));
        // Above is in slice 0, so it is not.
        assert_eq!(n.mb_b(5), None);
        assert_eq!(n.mb_c(5), None);
        assert_eq!(n.mb_d(5), None);
    }

    #[test]
    fn a_new_picture_forgets_every_macroblock() {
        let mut n = full();
        n.begin_picture();
        n.begin_macroblock(5, 0);
        assert_eq!((n.mb_a(5), n.mb_b(5)), (None, None));
    }

    #[test]
    fn a_location_inside_the_macroblock_stays_there() {
        let n = full();
        assert_eq!(
            n.luma_location(5, 7, 9),
            Some(SampleRef { mb: 5, x: 7, y: 9 })
        );
    }

    /// Table 6-3 in full, for a macroblock that has every neighbour, so that
    /// the coordinate rewriting is checked rather than just the addresses.
    #[test]
    fn locations_outside_the_macroblock_land_in_the_right_neighbour() {
        let n = full();
        assert_eq!(
            n.luma_location(5, -1, 3),
            Some(SampleRef { mb: 4, x: 15, y: 3 })
        );
        assert_eq!(
            n.luma_location(5, 3, -1),
            Some(SampleRef { mb: 1, x: 3, y: 15 })
        );
        assert_eq!(
            n.luma_location(5, 16, -1),
            Some(SampleRef { mb: 2, x: 0, y: 15 })
        );
        assert_eq!(
            n.luma_location(5, -1, -1),
            Some(SampleRef {
                mb: 0,
                x: 15,
                y: 15
            })
        );
    }

    /// Samples below the macroblock, or to its right on the same rows, belong
    /// to macroblocks that have not been decoded yet.
    #[test]
    fn locations_below_or_right_of_the_macroblock_are_unavailable() {
        let n = full();
        assert_eq!(n.luma_location(5, 3, 16), None);
        assert_eq!(n.luma_location(5, 16, 3), None);
        assert_eq!(n.luma_location(5, 16, 16), None);
        assert_eq!(n.luma_location(5, -1, 16), None);
    }

    #[test]
    fn chroma_locations_use_the_8x8_macroblock_size() {
        let n = full();
        assert_eq!(
            n.chroma_location(5, -1, 2),
            Some(SampleRef { mb: 4, x: 7, y: 2 })
        );
        assert_eq!(
            n.chroma_location(5, 2, -1),
            Some(SampleRef { mb: 1, x: 2, y: 7 })
        );
        // 8 is outside a chroma macroblock even though it is inside a luma one.
        assert_eq!(n.chroma_location(5, 8, 2), None);
        assert_eq!(
            n.chroma_location(5, 7, 7),
            Some(SampleRef { mb: 5, x: 7, y: 7 })
        );
    }

    #[test]
    fn interior_4x4_blocks_neighbour_each_other_within_the_macroblock() {
        let n = full();
        // Block 3 is at (4, 4): its left is block 2 and its above is block 1.
        assert_eq!(n.luma_4x4_a(5, 3), Some(BlockRef { mb: 5, blk: 2 }));
        assert_eq!(n.luma_4x4_b(5, 3), Some(BlockRef { mb: 5, blk: 1 }));
        assert_eq!(n.luma_4x4_d(5, 3), Some(BlockRef { mb: 5, blk: 0 }));
    }

    /// The rule that makes block C awkward: for block 3 it names block 4,
    /// which the quadrant-major scan has not reached yet.
    #[test]
    fn a_block_later_in_scan_order_is_not_available_as_a_neighbour() {
        let n = full();
        assert_eq!(n.luma_4x4_c(5, 3), None);
        // Block 5 sits at the top of the macroblock, so its C crosses into the
        // above-right macroblock instead and is perfectly available.
        assert_eq!(n.luma_4x4_c(5, 5), Some(BlockRef { mb: 2, blk: 10 }));
    }

    #[test]
    fn blocks_on_the_macroblock_edge_neighbour_the_adjacent_macroblock() {
        let n = full();
        // Block 0 is at the top-left corner, so all four of its neighbours
        // are in other macroblocks, at the far edge of each.
        assert_eq!(n.luma_4x4_a(5, 0), Some(BlockRef { mb: 4, blk: 5 }));
        assert_eq!(n.luma_4x4_b(5, 0), Some(BlockRef { mb: 1, blk: 10 }));
        assert_eq!(n.luma_4x4_d(5, 0), Some(BlockRef { mb: 0, blk: 15 }));
        assert_eq!(n.luma_4x4_c(5, 0), Some(BlockRef { mb: 1, blk: 11 }));
    }

    #[test]
    fn block_neighbours_inherit_macroblock_unavailability() {
        let mut n = Neighbourhood::new(4, 3);
        n.begin_macroblock(0, 0);
        // Nothing to the left of or above macroblock 0, so its edge blocks
        // have no neighbours outside it.
        assert_eq!(n.luma_4x4_a(0, 0), None);
        assert_eq!(n.luma_4x4_b(0, 0), None);
        assert_eq!(n.chroma_4x4_neighbour(0, 0, -1, 0), None);
        // But its interior blocks still do.
        assert_eq!(n.luma_4x4_a(0, 1), Some(BlockRef { mb: 0, blk: 0 }));
    }

    #[test]
    fn eight_by_eight_and_chroma_blocks_cross_macroblocks_the_same_way() {
        let n = full();
        assert_eq!(
            n.luma_8x8_neighbour(5, 0, -1, 0),
            Some(BlockRef { mb: 4, blk: 1 })
        );
        assert_eq!(
            n.luma_8x8_neighbour(5, 0, 0, -1),
            Some(BlockRef { mb: 1, blk: 2 })
        );
        assert_eq!(
            n.luma_8x8_neighbour(5, 2, -1, 0),
            Some(BlockRef { mb: 4, blk: 3 })
        );
        // 8x8 block 1 is in the top row, so its above-right crosses into the
        // macroblock above-right rather than staying inside this one.
        assert_eq!(
            n.luma_8x8_neighbour(5, 1, 8, -1),
            Some(BlockRef { mb: 2, blk: 2 })
        );
        // Block 3's above-right is off the right edge of the macroblock, on a
        // row that has already been decoded, and so is unavailable.
        assert_eq!(n.luma_8x8_neighbour(5, 3, 8, -1), None);

        assert_eq!(
            n.chroma_4x4_neighbour(5, 0, -1, 0),
            Some(BlockRef { mb: 4, blk: 1 })
        );
        assert_eq!(
            n.chroma_4x4_neighbour(5, 0, 0, -1),
            Some(BlockRef { mb: 1, blk: 2 })
        );
        assert_eq!(
            n.chroma_4x4_neighbour(5, 3, -1, 0),
            Some(BlockRef { mb: 5, blk: 2 })
        );
    }

    #[test]
    fn macroblock_origins_tile_the_picture() {
        let n = full();
        assert_eq!(n.origin(0), (0, 0));
        assert_eq!(n.origin(3), (48, 0));
        assert_eq!(n.origin(4), (0, 16));
        assert_eq!(n.origin(11), (48, 32));
    }
}
