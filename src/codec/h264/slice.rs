//! Slice-header parsing and CABAC payload hand-off.
//!
//! `h264-reader` intentionally exposes a syntax reader, not its bit cursor.
//! CABAC starts after one-bits that pad the slice header to a byte boundary, so a
//! decoder needs that cursor. `CountingBits` supplies it while still letting
//! `h264-reader` own every slice-header grammar detail.

use h264_reader::nal::pps::{PicParameterSet, PicScalingMatrix};
use h264_reader::nal::slice::{
    DecRefPicMarking, MemoryManagementControlOperation, ModificationOfPicNums, NumRefIdxActive,
    PredWeightTable, RefPicListModifications, SliceFamily, SliceHeader,
};
use h264_reader::nal::sps::{FrameMbsFlags, ScalingList, SeqParameterSet, SeqScalingMatrix};
use h264_reader::nal::{NalHeader, UnitType};
use h264_reader::rbsp::{self, BitRead, BitReaderError, Numeric, Primitive};
use h264_reader::Context;

use crate::Error;

use super::picture::{Cropping, MmcoOp, RefListMod, RefMarking};
use super::recon::{ScalingListSyntax, ScalingLists};

/// Header details consumed by the macroblock layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceInfo {
    pub first_mb: usize,
    pub frame_num: u16,
    pub kind: SliceKind,
    pub idr: bool,
    /// NAL header `nal_ref_idc`. Zero marks a disposable picture that must
    /// be displayed but must not enter the decoded picture buffer.
    pub nal_ref_idc: u8,
    pub slice_qp: u8,
    /// `cabac_init_idc` for P slices. I slices always use table 0.
    pub cabac_init_idc: u8,
    pub picture: PictureConfig,
    pub constrained_intra: bool,
    pub chroma_qp_offset: [i32; 2],
    pub transform_8x8_enabled: bool,
    pub deblocking: Deblocking,
    /// How many entries reference list 0 has. `ref_idx_l0` is only coded when
    /// there is more than one to choose between, so getting this wrong
    /// desynchronises CABAC at the first inter macroblock.
    pub num_ref_idx_l0: usize,
}

/// Per-slice control of the in-loop deblocking filter, from spec 7.4.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Deblocking {
    /// 0 filters every edge, 1 disables the filter, 2 leaves slice
    /// boundaries unfiltered.
    pub disable_idc: u8,
    /// `slice_alpha_c0_offset_div2 * 2`, already doubled into the units
    /// [`super::deblock::Thresholds`] wants.
    pub alpha_offset: i32,
    /// `slice_beta_offset_div2 * 2`.
    pub beta_offset: i32,
}

impl Deblocking {
    /// Whether any edge in this slice is filtered at all.
    pub fn enabled(&self) -> bool {
        self.disable_idc != 1
    }

    /// Whether edges against a *different* slice are filtered.
    pub fn crosses_slices(&self) -> bool {
        self.disable_idc != 1 && self.disable_idc != 2
    }
}

/// SPS/PPS values fixed for every macroblock of this picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureConfig {
    pub width_mbs: usize,
    pub height_mbs: usize,
    pub max_refs: usize,
    pub max_frame_num: u32,
    pub crop: Cropping,
}

/// Slice families this decoder accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceKind {
    P,
    I,
}

/// A parsed CABAC slice. `data` begins at CABAC's arithmetic-decoder input.
#[derive(Debug)]
pub struct CabacSlice {
    pub info: SliceInfo,
    /// Dequantisation weights resolved from this slice's SPS/PPS.
    ///
    /// Carried beside the header rather than inside [`SliceInfo`] so the
    /// header stays `Copy` while still updating when a new PPS arrives with
    /// the same picture size.
    pub scaling: ScalingLists,
    /// Short-term `ref_pic_list_modification` commands for list 0. Empty when
    /// the slice uses the default descending-`PicNum` order.
    pub list_mods: Vec<RefListMod>,
    /// How this picture updates the DPB after decode (spec 8.2.5). Disposable
    /// NALs (`nal_ref_idc == 0`) carry [`RefMarking::None`].
    pub marking: RefMarking,
    /// RBSP bytes holding CABAC data. `bit_offset` identifies its first bit.
    pub data: Vec<u8>,
    pub bit_offset: u8,
}

/// Parses one complete VCL NAL and locates its byte-aligned CABAC payload.
///
/// The returned payload excludes NAL header, emulation-prevention bytes,
/// slice header, and `cabac_alignment_one_bit` padding. That syntax element is
/// a one-bit repeated until the next byte boundary (not one followed by
/// zeroes). CAVLC is deliberately
/// rejected here: it begins at an arbitrary bit and belongs to its own reader.
pub fn parse_cabac(ctx: &Context, nal: &[u8]) -> Result<CabacSlice, Error> {
    let header = nal_header(nal)?;
    let idr = header.nal_unit_type() == UnitType::SliceLayerWithoutPartitioningIdr;
    if !matches!(
        header.nal_unit_type(),
        UnitType::SliceLayerWithoutPartitioningNonIdr | UnitType::SliceLayerWithoutPartitioningIdr
    ) {
        return Err(decode("not a VCL slice NAL"));
    }

    let rbsp = rbsp::decode_nal(nal).map_err(Error::Io)?;
    let mut bits = CountingBits::new(rbsp::BitReader::new(&rbsp[..]));
    let (header_data, sps, pps) = SliceHeader::from_bits(ctx, &mut bits, header)
        .map_err(|e| decode(format!("invalid slice header: {e:?}")))?;
    if !pps.entropy_coding_mode_flag {
        return Err(decode("CAVLC slice; CABAC decoder required"));
    }
    if !matches!(sps.frame_mbs_flags, FrameMbsFlags::Frames) {
        return Err(decode("interlaced pictures are outside decoder scope"));
    }
    if sps.chroma_info.separate_colour_plane_flag
        || sps.chroma_info.bit_depth_luma_minus8 != 0
        || sps.chroma_info.bit_depth_chroma_minus8 != 0
    {
        return Err(decode("only 8-bit 4:2:0 pictures are supported"));
    }

    let kind = match header_data.slice_type.family {
        SliceFamily::P => SliceKind::P,
        SliceFamily::I => SliceKind::I,
        SliceFamily::B => return Err(decode("B slices are outside decoder scope")),
        SliceFamily::SP | SliceFamily::SI => {
            return Err(decode("SP/SI slices are outside decoder scope"))
        }
    };
    reject_pred_weight_table(header_data.pred_weight_table.as_ref())?;
    if header_data.redundant_pic_cnt.is_some_and(|count| count > 0) {
        return Err(decode("redundant coded pictures are unsupported"));
    }
    // The slice header may override the PPS default; when it does not, the
    // PPS value stands.
    let num_ref_idx_l0 = match header_data.num_ref_idx_active {
        Some(NumRefIdxActive::P {
            num_ref_idx_l0_active_minus1,
        })
        | Some(NumRefIdxActive::B {
            num_ref_idx_l0_active_minus1,
            ..
        }) => num_ref_idx_l0_active_minus1,
        None => pps.num_ref_idx_l0_default_active_minus1,
    } as usize
        + 1;
    let list_mods = short_term_list_mods(&header_data.ref_pic_list_modification, num_ref_idx_l0)?;
    let marking = ref_marking(&header_data.dec_ref_pic_marking)?;
    let slice_qp = (26 + pps.pic_init_qp_minus26 + header_data.slice_qp_delta)
        .try_into()
        .map_err(|_| decode("slice quantiser outside 0..=51"))?;
    let cabac_init_idc = header_data.cabac_init_idc.unwrap_or(0);
    if cabac_init_idc > 2 {
        return Err(decode("cabac_init_idc outside 0..=2"));
    }
    let deblocking = Deblocking {
        disable_idc: header_data.disable_deblocking_filter_idc,
        alpha_offset: bits.alpha_offset_div2 * 2,
        beta_offset: bits.beta_offset_div2 * 2,
    };
    let header_end = bits.bits_read();
    let cabac_start = skip_cabac_alignment(&rbsp, header_end)?;
    let crop = sps
        .frame_cropping
        .as_ref()
        .map_or(Cropping::default(), |crop| {
            Cropping::from_sps_offsets(
                crop.left_offset,
                crop.right_offset,
                crop.top_offset,
                crop.bottom_offset,
            )
        });
    let second_chroma_offset = pps
        .extension
        .as_ref()
        .map_or(pps.chroma_qp_index_offset, |extra| {
            extra.second_chroma_qp_index_offset
        });

    Ok(CabacSlice {
        info: SliceInfo {
            first_mb: header_data.first_mb_in_slice as usize,
            frame_num: header_data.frame_num,
            kind,
            idr,
            nal_ref_idc: header.nal_ref_idc(),
            slice_qp,
            cabac_init_idc: cabac_init_idc as u8,
            picture: PictureConfig {
                width_mbs: sps.pic_width_in_mbs() as usize,
                height_mbs: sps.pic_height_in_map_units() as usize,
                max_refs: sps.max_num_ref_frames as usize,
                max_frame_num: 1 << sps.log2_max_frame_num(),
                crop,
            },
            constrained_intra: pps.constrained_intra_pred_flag,
            chroma_qp_offset: [pps.chroma_qp_index_offset, second_chroma_offset],
            transform_8x8_enabled: pps
                .extension
                .as_ref()
                .is_some_and(|extra| extra.transform_8x8_mode_flag),
            deblocking,
            num_ref_idx_l0,
        },
        scaling: scaling_lists(sps, pps),
        list_mods,
        marking,
        data: rbsp[cabac_start / 8..].to_vec(),
        bit_offset: (cabac_start % 8) as u8,
    })
}

/// Resolves SPS/PPS scaling matrices into the WeightScale tables reconstruction
/// indexes. Flat when neither set carries a matrix — the camera case.
fn scaling_lists(sps: &SeqParameterSet, pps: &PicParameterSet) -> ScalingLists {
    let sps_syntax = sps
        .chroma_info
        .scaling_matrix
        .as_ref()
        .map(seq_scaling_syntax);
    let pps_syntax = pps
        .extension
        .as_ref()
        .and_then(|extra| extra.pic_scaling_matrix.as_ref())
        .map(pic_scaling_syntax);
    ScalingLists::from_syntax(
        sps_syntax.as_ref().map(|(a, b)| (a, b)),
        pps_syntax.as_ref().map(|(a, b)| (a, b.as_ref())),
    )
}

fn seq_scaling_syntax(
    matrix: &SeqScalingMatrix,
) -> ([ScalingListSyntax<16>; 6], [ScalingListSyntax<64>; 2]) {
    (
        take_six_4x4(&matrix.scaling_list4x4),
        take_two_8x8(&matrix.scaling_list8x8),
    )
}

fn pic_scaling_syntax(
    matrix: &PicScalingMatrix,
) -> (
    [ScalingListSyntax<16>; 6],
    Option<[ScalingListSyntax<64>; 2]>,
) {
    (
        take_six_4x4(&matrix.scaling_list4x4),
        matrix
            .scaling_list8x8
            .as_ref()
            .map(|lists| take_two_8x8(lists)),
    )
}

fn take_six_4x4(lists: &[ScalingList<16>]) -> [ScalingListSyntax<16>; 6] {
    let mut out = [ScalingListSyntax::NotPresent; 6];
    for (dst, src) in out.iter_mut().zip(lists.iter()) {
        *dst = convert_list(src);
    }
    out
}

fn take_two_8x8(lists: &[ScalingList<64>]) -> [ScalingListSyntax<64>; 2] {
    let mut out = [ScalingListSyntax::NotPresent; 2];
    for (dst, src) in out.iter_mut().zip(lists.iter()) {
        *dst = convert_list(src);
    }
    out
}

fn convert_list<const N: usize>(list: &ScalingList<N>) -> ScalingListSyntax<N> {
    match list {
        ScalingList::NotPresent => ScalingListSyntax::NotPresent,
        ScalingList::UseDefault => ScalingListSyntax::UseDefault,
        ScalingList::List(values) => {
            let mut scan = [0u8; N];
            for (dst, src) in scan.iter_mut().zip(values.iter()) {
                *dst = src.get();
            }
            ScalingListSyntax::Scan(scan)
        }
    }
}

/// Consumes the CABAC header-alignment bits from spec 7.3.4.
///
/// The grammar repeats `cabac_alignment_one_bit` while the cursor is not
/// byte-aligned. A 29-bit header therefore has `111` at bits 29..31 and CABAC
/// itself starts at bit 32. This check turns a malformed boundary into a clear
/// parse error instead of allowing the arithmetic decoder to desynchronise.
fn skip_cabac_alignment(rbsp: &[u8], header_end: usize) -> Result<usize, Error> {
    let padding = (8 - header_end % 8) % 8;
    if header_end + padding > rbsp.len() * 8 {
        return Err(decode("truncated CABAC alignment"));
    }
    for bit_pos in header_end..header_end + padding {
        let byte = rbsp[bit_pos / 8];
        if (byte >> (7 - bit_pos % 8)) & 1 == 0 {
            return Err(decode("invalid CABAC alignment bit"));
        }
    }
    Ok(header_end + padding)
}

/// Collects short-term L0 modifications; rejects long-term reordering and
/// command counts that overrun list 0.
fn short_term_list_mods(
    mods: &Option<RefPicListModifications>,
    num_ref_idx_l0: usize,
) -> Result<Vec<RefListMod>, Error> {
    let commands = match mods {
        Some(RefPicListModifications::P {
            ref_pic_list_modification_l0,
        }) => ref_pic_list_modification_l0.as_slice(),
        Some(RefPicListModifications::B { .. }) => {
            return Err(decode(
                "B-slice ref_pic_list_modification is outside decoder scope",
            ))
        }
        Some(RefPicListModifications::I) | None => return Ok(Vec::new()),
    };
    // Spec 7.4.3.1: the number of reordering commands may not exceed
    // num_ref_idx_l0_active_minus1 + 1, because 8.2.4.3.1 places command i at
    // index i of a list that is only that long. The bound is a conformance
    // requirement, so a corrupt or hostile stream can break it; reject the
    // slice here rather than let the reorder run off the end of the list.
    if commands.len() > num_ref_idx_l0 {
        return Err(decode(format!(
            "ref_pic_list_modification carries {} commands for {num_ref_idx_l0} active reference indices",
            commands.len()
        )));
    }
    let mut out = Vec::with_capacity(commands.len());
    for command in commands {
        match command {
            ModificationOfPicNums::Subtract(v) => out.push(RefListMod::Subtract(*v)),
            ModificationOfPicNums::Add(v) => out.push(RefListMod::Add(*v)),
            ModificationOfPicNums::LongTermRef(_) => {
                return Err(decode(
                    "long-term ref_pic_list_modification is outside decoder scope",
                ))
            }
        }
    }
    Ok(out)
}

fn reject_pred_weight_table(table: Option<&PredWeightTable>) -> Result<(), Error> {
    if table.is_some() {
        return Err(decode("weighted prediction is outside decoder scope"));
    }
    Ok(())
}

/// Maps `dec_ref_pic_marking` into the short-term ops this decoder supports.
fn ref_marking(marking: &Option<DecRefPicMarking>) -> Result<RefMarking, Error> {
    Ok(match marking {
        None => RefMarking::None,
        Some(DecRefPicMarking::SlidingWindow) => RefMarking::SlidingWindow,
        Some(DecRefPicMarking::Idr {
            long_term_reference_flag,
            ..
        }) => {
            if *long_term_reference_flag {
                return Err(decode(
                    "IDR long_term_reference_flag is outside decoder scope",
                ));
            }
            // `no_output_of_prior_pics_flag` is a no-op here: there are no B
            // slices and therefore no delayed pictures waiting to be output.
            RefMarking::Idr
        }
        Some(DecRefPicMarking::Adaptive(ops)) => {
            let mut out = Vec::with_capacity(ops.len());
            for op in ops {
                match op {
                    MemoryManagementControlOperation::ShortTermUnusedForRef {
                        difference_of_pic_nums_minus1,
                    } => out.push(MmcoOp::ShortTermUnused {
                        difference_of_pic_nums_minus1: *difference_of_pic_nums_minus1,
                    }),
                    MemoryManagementControlOperation::AllRefPicturesUnused => {
                        out.push(MmcoOp::AllUnused)
                    }
                    MemoryManagementControlOperation::LongTermUnusedForRef { .. }
                    | MemoryManagementControlOperation::ShortTermUsedForLongTerm { .. }
                    | MemoryManagementControlOperation::MaxUsedLongTermFrameRef { .. }
                    | MemoryManagementControlOperation::CurrentUsedForLongTerm { .. } => {
                        return Err(decode(
                            "long-term dec_ref_pic_marking is outside decoder scope",
                        ))
                    }
                }
            }
            RefMarking::Adaptive(out)
        }
    })
}

fn nal_header(nal: &[u8]) -> Result<NalHeader, Error> {
    let Some(&first) = nal.first() else {
        return Err(decode("empty NAL"));
    };
    NalHeader::new(first).map_err(|e| decode(format!("invalid NAL header: {e:?}")))
}

fn decode(message: impl Into<String>) -> Error {
    Error::Decode(message.into())
}

/// `BitRead` decorator which records exactly how much slice-header grammar
/// consumed. Exp-Golomb is expanded here because the dependency's convenience
/// method does not report its encoded width.
///
/// It also captures the two deblocking offsets. `h264-reader` parses those
/// elements — it has to, to reach the end of the header — but range-checks
/// and discards them rather than exposing them on [`SliceHeader`]. Reading
/// them out of the syntax-element name as they go past is the only way to
/// recover them without re-parsing the whole header, because they are
/// variable-length and preceded by variable-length reference-list and
/// reference-marking syntax.
struct CountingBits<R> {
    inner: R,
    bits: usize,
    alpha_offset_div2: i32,
    beta_offset_div2: i32,
}

impl<R> CountingBits<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            bits: 0,
            alpha_offset_div2: 0,
            beta_offset_div2: 0,
        }
    }

    fn bits_read(&self) -> usize {
        self.bits
    }
}

impl<R: BitRead> BitRead for CountingBits<R> {
    fn read_ue(&mut self, name: &'static str) -> Result<u32, BitReaderError> {
        let mut zeros = 0u32;
        while !self.read_bool(name)? {
            zeros += 1;
            if zeros > 31 {
                return Err(BitReaderError::ExpGolombTooLarge(name));
            }
        }
        if zeros == 0 {
            Ok(0)
        } else {
            Ok((1 << zeros) - 1 + self.read::<u32>(zeros, name)?)
        }
    }

    fn read_se(&mut self, name: &'static str) -> Result<i32, BitReaderError> {
        let value = self.read_ue(name)?;
        let sign = ((value & 1) as i32 * 2) - 1;
        let value = ((value >> 1) as i32 + (value & 1) as i32) * sign;
        match name {
            "slice_alpha_c0_offset_div2" => self.alpha_offset_div2 = value,
            "slice_beta_offset_div2" => self.beta_offset_div2 = value,
            _ => {}
        }
        Ok(value)
    }

    fn read_bool(&mut self, name: &'static str) -> Result<bool, BitReaderError> {
        let value = self.inner.read_bool(name)?;
        self.bits += 1;
        Ok(value)
    }

    fn read<U: Numeric>(
        &mut self,
        bit_count: u32,
        name: &'static str,
    ) -> Result<U, BitReaderError> {
        let value = self.inner.read(bit_count, name)?;
        self.bits += bit_count as usize;
        Ok(value)
    }

    fn read_to<V: Primitive>(&mut self, name: &'static str) -> Result<V, BitReaderError> {
        let value = self.inner.read_to(name)?;
        self.bits += std::mem::size_of::<V>() * 8;
        Ok(value)
    }

    fn skip(&mut self, bit_count: u32, name: &'static str) -> Result<(), BitReaderError> {
        self.inner.skip(bit_count, name)?;
        self.bits += bit_count as usize;
        Ok(())
    }

    fn has_more_rbsp_data(&mut self, name: &'static str) -> Result<bool, BitReaderError> {
        self.inner.has_more_rbsp_data(name)
    }

    fn finish_rbsp(self) -> Result<(), BitReaderError> {
        self.inner.finish_rbsp()
    }

    fn finish_sei_payload(self) -> Result<(), BitReaderError> {
        self.inner.finish_sei_payload()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use h264_reader::nal::pps::PicParameterSet;
    use h264_reader::nal::sps::SeqParameterSet;
    use h264_reader::nal::{Nal, RefNal};

    const CAMERA_FIXTURE: &[u8] =
        include_bytes!("../../../tests/fixtures/tapo-1080p-cabac-8x8.h264");

    #[test]
    fn counts_all_exp_golomb_bits_not_only_its_value() {
        // ue(5) is `00110`: two leading zeroes, one, then two suffix bits.
        let mut bits = CountingBits::new(rbsp::BitReader::new(&[0b0011_0000][..]));
        assert_eq!(bits.read_ue("test").unwrap(), 5);
        assert_eq!(bits.bits_read(), 5);
    }

    #[test]
    fn cabac_alignment_uses_one_bits_to_reach_the_next_byte() {
        // A header ending at bit 29 is followed by three
        // `cabac_alignment_one_bit`s, so CABAC begins at bit 32.
        assert_eq!(
            skip_cabac_alignment(&[0, 0, 0, 0b0011_1111], 29).unwrap(),
            32
        );
        assert!(skip_cabac_alignment(&[0, 0, 0, 0b0011_1011], 29).is_err());
    }

    #[test]
    fn camera_style_ref_pic_list_modification_is_parsed() {
        // Hand-built non-IDR P slice against the camera fixture's SPS/PPS.
        // Header sets `ref_pic_list_modification_flag_l0` and one Subtract(0)
        // command — the same spelling the Tapo stream uses.
        let sps_bytes = super::super::annexb::nal_units(CAMERA_FIXTURE)
            .find(|nal| nal.first().is_some_and(|h| h & 0x1f == 7))
            .expect("fixture SPS");
        let pps_bytes = super::super::annexb::nal_units(CAMERA_FIXTURE)
            .find(|nal| nal.first().is_some_and(|h| h & 0x1f == 8))
            .expect("fixture PPS");
        let mut ctx = Context::default();
        let sps =
            SeqParameterSet::from_bits(RefNal::new(sps_bytes, &[], true).rbsp_bits()).expect("SPS");
        ctx.put_seq_param_set(sps);
        let pps = PicParameterSet::from_bits(&ctx, RefNal::new(pps_bytes, &[], true).rbsp_bits())
            .expect("PPS");
        ctx.put_pic_param_set(pps);

        let nal = [0x21, 0xe2, 0x27, 0x23, 0x5f, 0xff];
        let slice = parse_cabac(&ctx, &nal).expect("parse");
        assert_eq!(slice.list_mods, [RefListMod::Subtract(0)]);
        assert_eq!(slice.marking, RefMarking::SlidingWindow);
    }

    #[test]
    fn camera_fixture_p_slices_use_adaptive_mmco() {
        let mut frontend = super::super::decoder::Frontend::new();
        let mut saw_adaptive = false;
        for access_unit in super::super::annexb::access_units(CAMERA_FIXTURE) {
            let slices = frontend.parse_access_unit(access_unit).expect("parse");
            for slice in slices {
                if matches!(slice.marking, RefMarking::Adaptive(_)) {
                    assert_eq!(
                        slice.marking,
                        RefMarking::Adaptive(vec![MmcoOp::ShortTermUnused {
                            difference_of_pic_nums_minus1: 0,
                        }])
                    );
                    saw_adaptive = true;
                }
            }
        }
        assert!(saw_adaptive, "fixture P slices should carry MMCO-1");
    }

    #[test]
    fn long_term_mmco_is_rejected() {
        use h264_reader::nal::slice::MemoryManagementControlOperation;
        let marking = Some(DecRefPicMarking::Adaptive(vec![
            MemoryManagementControlOperation::CurrentUsedForLongTerm {
                long_term_frame_idx: 0,
            },
        ]));
        let err = ref_marking(&marking).unwrap_err();
        assert!(err.to_string().contains("long-term"));
    }

    #[test]
    fn long_term_ref_pic_list_modification_is_rejected() {
        let modified = Some(RefPicListModifications::P {
            ref_pic_list_modification_l0: vec![ModificationOfPicNums::LongTermRef(0)],
        });
        let err = short_term_list_mods(&modified, 1).unwrap_err();
        assert!(err.to_string().contains("long-term"));
    }

    #[test]
    fn ref_pic_list_modification_longer_than_list0_is_rejected() {
        let modified = Some(RefPicListModifications::P {
            ref_pic_list_modification_l0: vec![
                ModificationOfPicNums::Subtract(0),
                ModificationOfPicNums::Subtract(0),
            ],
        });
        let err = short_term_list_mods(&modified, 1).unwrap_err();
        assert!(
            err.to_string()
                .contains("carries 2 commands for 1 active reference indices"),
            "{err}"
        );
        // The same commands are legal once list 0 is long enough to hold them.
        assert_eq!(short_term_list_mods(&modified, 2).unwrap().len(), 2);
    }

    #[test]
    fn prediction_weight_table_is_rejected() {
        let table = PredWeightTable {
            luma_log2_weight_denom: 0,
            chroma_log2_weight_denom: Some(0),
            luma_weights: Vec::new(),
            chroma_weights: Vec::new(),
        };
        let err = reject_pred_weight_table(Some(&table)).unwrap_err();
        assert!(err.to_string().contains("weighted prediction"));
        assert!(reject_pred_weight_table(None).is_ok());
    }
}
