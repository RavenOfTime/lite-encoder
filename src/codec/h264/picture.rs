//! Decoded pictures and the reference picture buffer.
//!
//! # Why this is small
//!
//! The decoded picture buffer is one of the more intricate parts of H.264 in
//! general, and almost none of that intricacy is reachable here. Without B
//! frames there is no reordering, so output order is decode order and picture
//! order counts never have to be computed. Without long-term references there
//! is no adaptive marking to track. What remains is a short queue of recent
//! pictures, ordered so that reference index 0 means "the most recent one",
//! and a rule for dropping the oldest when it fills.
//!
//! Refusing the rest is deliberate. A decoder that half-implements reference
//! reordering does not fail on the stream that uses it; it silently predicts
//! from the wrong picture, which looks like a motion compensation bug and is
//! among the most expensive things to chase. See [`super`] for the scope.

use std::time::Duration;

use super::inter::Plane;
use super::neighbour::Neighbourhood;
use crate::media::Frame;

/// A decoded picture, in 4:2:0 planar 8-bit.
///
/// Dimensions are the *coded* ones, a whole number of macroblocks. The
/// display rectangle is usually smaller: 1080 is not a multiple of 16, so
/// every 1080p stream codes 1088 lines and crops eight of them away. Cropping
/// happens on the way out, in [`Self::to_frame`], because prediction and
/// deblocking work on coded samples throughout.
#[derive(Debug, Clone)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub planes: [Vec<u8>; 3],
    pub strides: [usize; 3],
    /// `frame_num` from the slice header, which is what reference lists are
    /// ordered by.
    pub frame_num: u16,
}

impl Picture {
    /// Whether this picture is already the size a given stream needs.
    ///
    /// Recycled buffers are only interchangeable within one coded video
    /// sequence; a new sequence can change the picture size, and a buffer of
    /// the wrong size has to be let go rather than resized.
    pub fn is_sized(&self, width_mbs: usize, height_mbs: usize) -> bool {
        self.width == width_mbs * 16 && self.height == height_mbs * 16
    }

    /// Returns a recycled picture to a state ready for a new decode, keeping
    /// its allocations.
    ///
    /// Does *not* wipe the planes: at 1080p that is three megabytes of stores
    /// every frame, and a complete picture overwrites every sample anyway.
    /// Uncovered macroblocks — from packet loss, for example — are painted
    /// mid-grey afterwards by [`Self::grey_uncovered`], so a hole never shows
    /// whatever reference frame last occupied this buffer.
    pub fn reset(&mut self) {
        self.frame_num = 0;
    }

    /// Paints mid-grey into every macroblock no slice claimed.
    ///
    /// Call after the picture's slices have been decoded and before the frame
    /// is displayed or entered into the DPB. A fully covered picture is a
    /// no-op beyond walking the availability map.
    pub fn grey_uncovered(&mut self, neighbourhood: &Neighbourhood) {
        debug_assert_eq!(neighbourhood.width_mbs() * 16, self.width);
        debug_assert_eq!(neighbourhood.height_mbs() * 16, self.height);
        for addr in 0..neighbourhood.len() {
            if neighbourhood.slice_of(addr).is_some() {
                continue;
            }
            let (x, y) = neighbourhood.origin(addr);
            for row in 0..16 {
                let start = (y + row) * self.strides[0] + x;
                self.planes[0][start..start + 16].fill(128);
            }
            let (cx, cy) = (x / 2, y / 2);
            for plane in 1..3 {
                for row in 0..8 {
                    let start = (cy + row) * self.strides[plane] + cx;
                    self.planes[plane][start..start + 8].fill(128);
                }
            }
        }
    }

    /// An all-grey picture of `width_mbs` by `height_mbs` macroblocks.
    ///
    /// Mid-grey rather than black: a picture that is somehow displayed before
    /// being fully decoded is then visibly wrong rather than plausibly dark.
    pub fn new(width_mbs: usize, height_mbs: usize) -> Self {
        let (width, height) = (width_mbs * 16, height_mbs * 16);
        let (cw, ch) = (width / 2, height / 2);
        Self {
            width,
            height,
            planes: [
                vec![128; width * height],
                vec![128; cw * ch],
                vec![128; cw * ch],
            ],
            strides: [width, cw, cw],
            frame_num: 0,
        }
    }

    /// The luma plane, as inter prediction wants it.
    pub fn luma(&self) -> Plane<'_> {
        Plane {
            data: &self.planes[0],
            width: self.width,
            height: self.height,
            stride: self.strides[0],
        }
    }

    /// One chroma plane, `comp` being 0 for U and 1 for V.
    pub fn chroma(&self, comp: usize) -> Plane<'_> {
        Plane {
            data: &self.planes[comp + 1],
            width: self.width / 2,
            height: self.height / 2,
            stride: self.strides[comp + 1],
        }
    }

    /// Copies out the display rectangle as a [`Frame`].
    pub fn to_frame(&self, crop: Cropping, pts: Duration) -> Frame {
        let (width, height) = crop.display_size(self.width, self.height);
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        Frame {
            pts,
            width: width as u32,
            height: height as u32,
            planes: [
                self.crop_plane(0, crop.left, crop.top, width, height),
                self.crop_plane(1, crop.left / 2, crop.top / 2, cw, ch),
                self.crop_plane(2, crop.left / 2, crop.top / 2, cw, ch),
            ],
            strides: [width, cw, cw],
        }
    }

    /// Copies the display rectangle of one plane out, row by row.
    ///
    /// Written as whole-row `extend_from_slice` rather than as an iterator
    /// over samples: the row form compiles to one `memcpy` per row, and at
    /// 1080p the per-sample form was costing several milliseconds a frame —
    /// more than reconstruction of the entire picture.
    fn crop_plane(&self, plane: usize, x: usize, y: usize, width: usize, height: usize) -> Vec<u8> {
        let stride = self.strides[plane];
        let mut out = Vec::with_capacity(width * height);
        for row in 0..height {
            let start = (y + row) * stride + x;
            out.extend_from_slice(&self.planes[plane][start..start + width]);
        }
        out
    }
}

/// The display rectangle, as an inset from the coded picture in luma samples.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cropping {
    pub left: usize,
    pub right: usize,
    pub top: usize,
    pub bottom: usize,
}

impl Cropping {
    /// Builds a crop from the sequence parameter set's offsets.
    ///
    /// The offsets are in chroma samples for 4:2:0, so each is worth two luma
    /// samples. That factor is why a stream can only crop an even number of
    /// columns, and why 1088-to-1080 works but an odd inset could not be
    /// expressed at all.
    pub fn from_sps_offsets(left: u32, right: u32, top: u32, bottom: u32) -> Self {
        Self {
            left: left as usize * 2,
            right: right as usize * 2,
            top: top as usize * 2,
            bottom: bottom as usize * 2,
        }
    }

    /// The visible size of a coded picture of the given dimensions.
    pub fn display_size(&self, width: usize, height: usize) -> (usize, usize) {
        (
            width.saturating_sub(self.left + self.right).max(1),
            height.saturating_sub(self.top + self.bottom).max(1),
        )
    }
}

/// A short-term `ref_pic_list_modification` command (spec 7.4.3.1).
///
/// Long-term commands are rejected at parse time; this decoder has no
/// long-term reference support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefListMod {
    /// `modification_of_pic_nums_idc == 0`: predicted PicNum minus
    /// (`abs_diff_pic_num_minus1` + 1).
    Subtract(u32),
    /// `modification_of_pic_nums_idc == 1`: predicted PicNum plus
    /// (`abs_diff_pic_num_minus1` + 1).
    Add(u32),
}

/// How a decoded picture updates the DPB (spec 8.2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefMarking {
    /// `nal_ref_idc == 0`: display only, never a reference.
    None,
    /// Non-IDR reference using the sliding window of 8.2.5.3.
    SlidingWindow,
    /// Non-IDR reference with short-term MMCO ops (8.2.5.4).
    Adaptive(Vec<MmcoOp>),
    /// IDR reference; the DPB was already cleared at slice start.
    Idr,
}

/// Short-term memory-management control operations this decoder accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmcoOp {
    /// Mark the short-term picture with
    /// `PicNum = CurrPicNum − (difference_of_pic_nums_minus1 + 1)` unused.
    ShortTermUnused { difference_of_pic_nums_minus1: u32 },
    /// Mark every reference picture unused (MMCO 5).
    AllUnused,
}

/// The short-term reference pictures available for prediction.
///
/// Ordered most-recent-first, which for a P slice with no reference list
/// modification is exactly the list the spec derives: descending `PicNum`.
/// Reference index 0 is therefore the previous picture, which in surveillance
/// footage is what essentially every macroblock uses. When the slice header
/// carries modifications, [`Dpb::list0`] applies them before prediction.
#[derive(Debug, Clone)]
pub struct Dpb {
    refs: Vec<Picture>,
    capacity: usize,
    max_frame_num: u32,
}

impl Dpb {
    /// `capacity` is the sequence parameter set's `max_num_ref_frames`, and
    /// `max_frame_num` its `MaxFrameNum`.
    pub fn new(capacity: usize, max_frame_num: u32) -> Self {
        Self {
            // At least one, or a stream declaring zero reference frames would
            // leave P slices with nothing to predict from.
            refs: Vec::new(),
            capacity: capacity.max(1),
            max_frame_num,
        }
    }

    /// Empties the buffer, as an IDR picture requires.
    ///
    /// An IDR is a hard boundary: nothing before it may be referenced, which
    /// is the property that makes it a seek point and a recovery point after
    /// packet loss.
    pub fn clear(&mut self) {
        self.refs.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.refs.len()
    }

    /// The reference picture at index `idx` of the default list 0 order.
    pub fn get(&self, idx: usize) -> Option<&Picture> {
        self.refs.get(idx)
    }

    /// Builds reference list 0 for the current picture (spec 8.2.4).
    ///
    /// Starts from the default descending-`PicNum` order already stored in
    /// `refs`, applies any short-term modifications, then truncates to
    /// `num_active` entries (`num_ref_idx_l0_active_minus1 + 1`).
    pub fn list0(
        &self,
        curr_pic_num: u16,
        mods: &[RefListMod],
        num_active: usize,
    ) -> Result<Vec<&Picture>, crate::Error> {
        let num_active = num_active.max(1);
        let mut list: Vec<Option<&Picture>> = self.refs.iter().map(Some).collect();
        list.resize(num_active + 1, None);

        if !mods.is_empty() {
            let max_pic_num = i64::from(self.max_frame_num);
            let curr = i64::from(curr_pic_num);
            let mut pred = curr;
            for (ref_idx, command) in mods.iter().enumerate() {
                let abs_diff = match *command {
                    RefListMod::Subtract(v) | RefListMod::Add(v) => i64::from(v) + 1,
                };
                let no_wrap = match *command {
                    RefListMod::Subtract(_) => {
                        if pred - abs_diff < 0 {
                            pred - abs_diff + max_pic_num
                        } else {
                            pred - abs_diff
                        }
                    }
                    RefListMod::Add(_) => {
                        if pred + abs_diff >= max_pic_num {
                            pred + abs_diff - max_pic_num
                        } else {
                            pred + abs_diff
                        }
                    }
                };
                pred = no_wrap;
                let pic_num = if no_wrap > curr {
                    no_wrap - max_pic_num
                } else {
                    no_wrap
                };
                let picture = self
                    .refs
                    .iter()
                    .find(|p| pic_num_of(p.frame_num, curr_pic_num, self.max_frame_num) == pic_num)
                    .ok_or_else(|| {
                        crate::Error::Decode(format!(
                            "ref_pic_list_modification targets missing PicNum {pic_num}"
                        ))
                    })?;
                // Spec 8.2.4.3.1: shift right from ref_idx, insert, then
                // compact so the inserted PicNum is not duplicated.
                for c_idx in (ref_idx + 1..=num_active).rev() {
                    list[c_idx] = list[c_idx - 1];
                }
                list[ref_idx] = Some(picture);
                let mut n_idx = ref_idx + 1;
                for c_idx in ref_idx + 1..=num_active {
                    let keep = list[c_idx].is_some_and(|p| {
                        pic_num_of(p.frame_num, curr_pic_num, self.max_frame_num) != pic_num
                    });
                    if keep {
                        list[n_idx] = list[c_idx];
                        n_idx += 1;
                    }
                }
                while n_idx <= num_active {
                    list[n_idx] = None;
                    n_idx += 1;
                }
            }
        }

        let mut out = Vec::with_capacity(num_active);
        for entry in list.into_iter().take(num_active) {
            match entry {
                Some(picture) => out.push(picture),
                None => break,
            }
        }
        Ok(out)
    }

    /// Adds a picture as the most recent reference, dropping the oldest if
    /// the buffer is full.
    ///
    /// This is the sliding window of spec 8.2.5.3. "Oldest" is by
    /// `FrameNumWrap`, not by arrival, which matters because `frame_num`
    /// wraps: without accounting for the wrap, the picture immediately after
    /// one would look like the oldest in the buffer and be evicted first.
    ///
    /// The evicted picture is handed back rather than dropped. Its buffers are
    /// exactly the size the next picture needs, and at 1080p re-allocating
    /// them every frame costs more than all the deblocking in the picture.
    #[must_use = "the evicted picture's buffers are worth reusing"]
    pub fn push(&mut self, picture: Picture) -> Option<Picture> {
        let current = picture.frame_num;
        self.refs.insert(0, picture);
        if self.refs.len() > self.capacity {
            let max = self.max_frame_num;
            let oldest = self
                .refs
                .iter()
                .enumerate()
                .min_by_key(|(_, p)| frame_num_wrap(p.frame_num, current, max))
                .map(|(i, _)| i);
            if let Some(oldest) = oldest {
                return Some(self.refs.remove(oldest));
            }
        }
        None
    }

    /// Applies short-term MMCO ops for the picture that just finished decode
    /// (spec 8.2.5.4), returning any pictures marked unused so their buffers
    /// can be recycled. Call before pushing the current picture.
    pub fn apply_mmco(
        &mut self,
        curr_pic_num: u16,
        ops: &[MmcoOp],
    ) -> Result<Vec<Picture>, crate::Error> {
        let mut recycled = Vec::new();
        for op in ops {
            match *op {
                MmcoOp::AllUnused => {
                    recycled.append(&mut self.refs);
                }
                MmcoOp::ShortTermUnused {
                    difference_of_pic_nums_minus1,
                } => {
                    let pic_num =
                        i64::from(curr_pic_num) - (i64::from(difference_of_pic_nums_minus1) + 1);
                    let idx = self
                        .refs
                        .iter()
                        .position(|p| {
                            pic_num_of(p.frame_num, curr_pic_num, self.max_frame_num) == pic_num
                        })
                        .ok_or_else(|| {
                            crate::Error::Decode(format!("MMCO targets missing PicNum {pic_num}"))
                        })?;
                    recycled.push(self.refs.remove(idx));
                }
            }
        }
        Ok(recycled)
    }

    /// Marks `picture` as a short-term reference after applying `marking`.
    ///
    /// Adaptive marking runs first; the current picture is then inserted. The
    /// sliding-window path is used for `SlidingWindow` and `Idr` (IDR already
    /// cleared the buffer at slice start). Returns every picture freed for
    /// buffer reuse.
    pub fn mark_reference(
        &mut self,
        picture: Picture,
        marking: &RefMarking,
    ) -> Result<Vec<Picture>, crate::Error> {
        let mut recycled = match marking {
            RefMarking::None => {
                return Err(crate::Error::Decode(
                    "non-reference picture cannot enter the DPB".into(),
                ))
            }
            RefMarking::Adaptive(ops) => self.apply_mmco(picture.frame_num, ops)?,
            RefMarking::SlidingWindow | RefMarking::Idr => Vec::new(),
        };
        if let Some(evicted) = self.push(picture) {
            recycled.push(evicted);
        }
        Ok(recycled)
    }
}

/// Spec `FrameNumWrap`: a picture's `frame_num` expressed relative to the
/// current one, so that ordering survives the counter wrapping.
fn frame_num_wrap(frame_num: u16, current: u16, max_frame_num: u32) -> i64 {
    pic_num_of(frame_num, current, max_frame_num)
}

/// Spec `PicNum` for a short-term frame reference (equals `FrameNumWrap`).
fn pic_num_of(frame_num: u16, current: u16, max_frame_num: u32) -> i64 {
    let (frame_num, current) = (i64::from(frame_num), i64::from(current));
    if frame_num > current {
        frame_num - i64::from(max_frame_num)
    } else {
        frame_num
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture_numbered(frame_num: u16) -> Picture {
        let mut p = Picture::new(2, 2);
        p.frame_num = frame_num;
        p
    }

    #[test]
    fn a_new_picture_is_sized_in_whole_macroblocks_and_is_grey() {
        let p = Picture::new(4, 3);
        assert_eq!((p.width, p.height), (64, 48));
        assert_eq!(p.planes[0].len(), 64 * 48);
        assert_eq!(p.planes[1].len(), 32 * 24);
        assert_eq!(p.strides, [64, 32, 32]);
        assert!(p.planes.iter().all(|plane| plane.iter().all(|&s| s == 128)));
    }

    /// A recycled buffer keeps its previous samples through [`Picture::reset`];
    /// only the holes after an incomplete decode are painted grey.
    #[test]
    fn grey_uncovered_paints_only_unclaimed_macroblocks() {
        let mut p = Picture::new(2, 2);
        for plane in &mut p.planes {
            plane.fill(40);
        }
        p.reset();
        assert!(
            p.planes.iter().all(|plane| plane.iter().all(|&s| s == 40)),
            "reset must not wipe the planes"
        );

        let mut neighbourhood = Neighbourhood::new(2, 2);
        neighbourhood.begin_macroblock(0, 0);
        neighbourhood.begin_macroblock(3, 0);
        p.grey_uncovered(&neighbourhood);

        // Macroblock 0 at (0,0): claimed, still 40.
        assert_eq!(p.planes[0][0], 40);
        assert_eq!(p.planes[1][0], 40);
        // Macroblock 1 at (16,0): hole, grey.
        assert_eq!(p.planes[0][16], 128);
        assert_eq!(p.planes[1][8], 128);
        // Macroblock 2 at (0,16): hole, grey.
        assert_eq!(p.planes[0][16 * 32], 128);
        // Macroblock 3 at (16,16): claimed, still 40.
        assert_eq!(p.planes[0][16 * 32 + 16], 40);
        assert_eq!(p.planes[2][8 * 16 + 8], 40);
    }

    #[test]
    fn the_planes_report_their_own_dimensions_for_edge_clamping() {
        let p = Picture::new(4, 3);
        let luma = p.luma();
        assert_eq!((luma.width, luma.height, luma.stride), (64, 48, 64));
        let chroma = p.chroma(1);
        assert_eq!((chroma.width, chroma.height, chroma.stride), (32, 24, 32));
    }

    /// The case every 1080p stream hits: 1080 is not a multiple of 16, so the
    /// picture is coded as 1088 lines and eight are cropped away.
    #[test]
    fn cropping_removes_the_padding_rows_of_a_1080_picture() {
        let crop = Cropping::from_sps_offsets(0, 0, 0, 4);
        assert_eq!(crop.bottom, 8);
        assert_eq!(crop.display_size(1920, 1088), (1920, 1080));
    }

    #[test]
    fn cropping_copies_the_right_rectangle() {
        let mut p = Picture::new(2, 2);
        // Mark the sample at (4, 6) so the crop offset can be checked rather
        // than just the resulting size.
        p.planes[0][6 * 32 + 4] = 200;
        let crop = Cropping::from_sps_offsets(2, 0, 3, 0);
        let frame = p.to_frame(crop, Duration::ZERO);

        assert_eq!((frame.width, frame.height), (28, 26));
        assert_eq!(frame.planes[0].len(), 28 * 26);
        // The marked sample moved left by 4 and up by 6.
        assert_eq!(frame.planes[0][0], 200);
        assert_eq!(frame.planes[1].len(), 14 * 13);
    }

    #[test]
    fn an_uncropped_picture_comes_out_whole() {
        let p = Picture::new(2, 2);
        let frame = p.to_frame(Cropping::default(), Duration::from_millis(40));
        assert_eq!((frame.width, frame.height), (32, 32));
        assert_eq!(frame.planes[0].len(), 32 * 32);
        assert_eq!(frame.pts, Duration::from_millis(40));
    }

    // The evicted picture these tests discard is the recycled buffer; only
    // the live decode path has anything to do with it.
    #[test]
    fn the_most_recently_pushed_picture_is_reference_zero() {
        let mut dpb = Dpb::new(4, 16);
        let _ = dpb.push(picture_numbered(1));
        let _ = dpb.push(picture_numbered(2));
        assert_eq!(dpb.get(0).unwrap().frame_num, 2);
        assert_eq!(dpb.get(1).unwrap().frame_num, 1);
        assert_eq!(dpb.len(), 2);
    }

    #[test]
    fn list0_subtract_reorders_short_term_references() {
        let mut dpb = Dpb::new(4, 16);
        let _ = dpb.push(picture_numbered(1));
        let _ = dpb.push(picture_numbered(2));
        let _ = dpb.push(picture_numbered(3));
        // Default L0 for curr=4 is [3, 2, 1]. Subtract(1) selects PicNum 2
        // (curr - 2) for index 0, leaving [2, 3, 1].
        let list = dpb.list0(4, &[RefListMod::Subtract(1)], 3).expect("list0");
        assert_eq!(
            list.iter().map(|p| p.frame_num).collect::<Vec<_>>(),
            vec![2, 3, 1]
        );
    }

    #[test]
    fn mmco_short_term_unused_removes_the_named_pic_num() {
        let mut dpb = Dpb::new(4, 16);
        let _ = dpb.push(picture_numbered(0));
        let _ = dpb.push(picture_numbered(1));
        let recycled = dpb
            .apply_mmco(
                2,
                &[MmcoOp::ShortTermUnused {
                    difference_of_pic_nums_minus1: 0,
                }],
            )
            .expect("mmco");
        assert_eq!(recycled.len(), 1);
        assert_eq!(recycled[0].frame_num, 1);
        assert_eq!(dpb.len(), 1);
        assert_eq!(dpb.get(0).unwrap().frame_num, 0);
    }

    #[test]
    fn the_buffer_never_grows_past_its_capacity() {
        let mut dpb = Dpb::new(2, 16);
        for frame_num in 0..6 {
            let _ = dpb.push(picture_numbered(frame_num));
        }
        assert_eq!(dpb.len(), 2);
        assert_eq!(dpb.get(0).unwrap().frame_num, 5);
        assert_eq!(dpb.get(1).unwrap().frame_num, 4);
    }

    /// The wrap is the reason eviction is by `FrameNumWrap` and not by
    /// arrival order: after the counter wraps, picture 0 is the *newest*, and
    /// evicting the numerically smallest would throw it away immediately.
    #[test]
    fn eviction_survives_the_frame_number_wrapping() {
        let mut dpb = Dpb::new(2, 16);
        let _ = dpb.push(picture_numbered(14));
        let _ = dpb.push(picture_numbered(15));
        // The counter wraps back to zero, which is newer than both.
        let _ = dpb.push(picture_numbered(0));

        assert_eq!(dpb.len(), 2);
        assert_eq!(dpb.get(0).unwrap().frame_num, 0);
        // 14 was the oldest and is gone; 15 remains.
        assert_eq!(dpb.get(1).unwrap().frame_num, 15);
    }

    #[test]
    fn an_idr_empties_the_buffer() {
        let mut dpb = Dpb::new(4, 16);
        let _ = dpb.push(picture_numbered(1));
        dpb.clear();
        assert!(dpb.is_empty());
        assert!(dpb.get(0).is_none());
    }

    /// A stream declaring no reference frames still has to hold one, or its
    /// P slices would have nothing to predict from.
    #[test]
    fn a_zero_capacity_buffer_still_holds_one_reference() {
        let mut dpb = Dpb::new(0, 16);
        let _ = dpb.push(picture_numbered(1));
        assert_eq!(dpb.len(), 1);
    }

    #[test]
    fn frame_numbers_order_correctly_across_the_wrap() {
        // Relative to picture 1, picture 15 is in the past, not the future.
        assert!(frame_num_wrap(15, 1, 16) < frame_num_wrap(0, 1, 16));
        assert!(frame_num_wrap(1, 1, 16) > frame_num_wrap(0, 1, 16));
    }
}
