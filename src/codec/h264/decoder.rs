//! H.264 front end: parameter-set lifetime plus slice hand-off.
//!
//! This is intentionally below `media::Decoder` for now. It owns long-lived
//! SPS/PPS state and turns one Annex B access unit into CABAC slices; picture
//! assembly calls it before running macroblock reconstruction.

use h264_reader::nal::pps::PicParameterSet;
use h264_reader::nal::sps::{ChromaFormat, FrameMbsFlags, SeqParameterSet};
use h264_reader::nal::{Nal, RefNal, UnitType};
use h264_reader::Context;

use crate::media::{Decoder, Frame, Packet};
use crate::Error;
use std::time::Duration;

use super::annexb;
use super::picture::{Dpb, Picture};
use super::picture_decode::PictureDecoder;
use super::slice::{self, CabacSlice, PictureConfig};
use super::state::PictureState;

/// Persistent parsing state for one H.264 elementary stream.
///
/// It also owns the decoder's working buffers between pictures. Those are
/// large — a 1080p picture is about 3 MB and the macroblock state another
/// 1.5 MB — and allocating them per picture means faulting in fresh pages
/// every frame, which measured as more of the decode time than reconstruction
/// and deblocking combined. They live here because this is the only object
/// that outlives a picture.
#[derive(Debug, Default)]
pub struct Frontend {
    context: Context,
    dpb: Option<Dpb>,
    /// Picture buffers no longer referenced, ready to decode into again.
    spare: Vec<Picture>,
    state: Option<PictureState>,
    /// The configuration `spare`, `state` and `dpb` were sized for. A new
    /// coded video sequence can change the picture size, and buffers from the
    /// old one are then the wrong shape rather than merely stale.
    config: Option<PictureConfig>,
}

impl Frontend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers parameter sets then returns CABAC payloads from `access_unit`.
    ///
    /// Input must use Annex B start codes. Parameter sets can arrive in this
    /// access unit or an earlier one; `Context` keeps them until replaced by a
    /// newer set with the same ID, as H.264 requires.
    pub fn parse_access_unit(&mut self, access_unit: &[u8]) -> Result<Vec<CabacSlice>, Error> {
        let mut slices = Vec::new();
        for bytes in annexb::nal_units(access_unit) {
            let nal = RefNal::new(bytes, &[], true);
            let kind = nal
                .header()
                .map_err(|e| Error::Decode(format!("invalid NAL header: {e:?}")))?
                .nal_unit_type();
            match kind {
                UnitType::SeqParameterSet => self.put_sps(&nal)?,
                UnitType::PicParameterSet => self.put_pps(&nal)?,
                UnitType::SliceLayerWithoutPartitioningNonIdr
                | UnitType::SliceLayerWithoutPartitioningIdr => {
                    slices.push(slice::parse_cabac(&self.context, bytes)?);
                }
                // Data partitioning, SVC/MVC, auxiliary/depth pictures, and
                // reserved/unspecified NAL types cannot be ignored: doing so
                // would turn a supported-looking access unit into corrupted
                // output with media silently missing.
                UnitType::SliceDataPartitionALayer
                | UnitType::SliceDataPartitionBLayer
                | UnitType::SliceDataPartitionCLayer
                | UnitType::SeqParameterSetExtension
                | UnitType::PrefixNALUnit
                | UnitType::SubsetSeqParameterSet
                | UnitType::DepthParameterSet
                | UnitType::SliceLayerWithoutPartitioningAux
                | UnitType::SliceExtension
                | UnitType::SliceExtensionViewComponent
                | UnitType::Reserved(_)
                | UnitType::Unspecified(_) => {
                    return Err(Error::Decode(format!(
                        "unsupported H.264 NAL unit type {}",
                        kind.id()
                    )));
                }
                _ => {}
            }
        }
        Ok(slices)
    }

    /// Parses and decodes one complete picture access unit.
    pub fn decode_access_unit(
        &mut self,
        access_unit: &[u8],
        pts: Duration,
    ) -> Result<Vec<Frame>, Error> {
        let slices = self.parse_access_unit(access_unit)?;
        let Some(first) = slices.first() else {
            return Ok(Vec::new());
        };
        let config = first.info.picture;
        let nal_ref_idc = first.info.nal_ref_idc;
        let marking = first.marking.clone();
        if slices.iter().any(|slice| slice.info.picture != config) {
            return Err(Error::Decode(
                "access unit mixes picture configurations".into(),
            ));
        }
        if slices
            .iter()
            .any(|slice| slice.info.nal_ref_idc != nal_ref_idc)
        {
            return Err(Error::Decode("access unit mixes nal_ref_idc values".into()));
        }
        if slices.iter().any(|slice| slice.marking != marking) {
            return Err(Error::Decode(
                "access unit mixes dec_ref_pic_marking values".into(),
            ));
        }
        if (nal_ref_idc == 0) != matches!(marking, super::picture::RefMarking::None) {
            return Err(Error::Decode(
                "nal_ref_idc and dec_ref_pic_marking disagree".into(),
            ));
        }
        if self.config != Some(config) {
            self.spare.clear();
            self.state = None;
            self.dpb = None;
            self.config = Some(config);
        }
        let dpb = self
            .dpb
            .take()
            .unwrap_or_else(|| Dpb::new(config.max_refs, config.max_frame_num));
        let mut buffer = self
            .spare
            .pop()
            .unwrap_or_else(|| Picture::new(config.width_mbs, config.height_mbs));
        buffer.reset();
        let mut state = self
            .state
            .take()
            .unwrap_or_else(|| PictureState::new(config.width_mbs, config.height_mbs));
        state.begin_picture();

        let mut picture = PictureDecoder::with_resources(config, dpb, buffer, state);
        for (slice_id, slice) in slices.iter().enumerate() {
            picture.decode_slice(slice, slice_id as u32)?;
        }
        let finished = picture.finish(config.crop, pts, &marking)?;
        self.dpb = Some(finished.dpb);
        self.state = Some(finished.state);
        self.spare.extend(finished.recycled);
        Ok(vec![finished.frame])
    }

    fn put_sps(&mut self, nal: &RefNal<'_>) -> Result<(), Error> {
        let sps = SeqParameterSet::from_bits(nal.rbsp_bits())
            .map_err(|e| Error::Decode(format!("invalid SPS: {e:?}")))?;
        validate_sps(&sps)?;
        self.context.put_seq_param_set(sps);
        Ok(())
    }

    fn put_pps(&mut self, nal: &RefNal<'_>) -> Result<(), Error> {
        let pps = PicParameterSet::from_bits(&self.context, nal.rbsp_bits())
            .map_err(|e| Error::Decode(format!("invalid PPS: {e:?}")))?;
        validate_pps(&pps)?;
        self.context.put_pic_param_set(pps);
        Ok(())
    }
}

impl Decoder for Frontend {
    fn decode(&mut self, pkt: &Packet) -> Result<Vec<Frame>, Error> {
        self.decode_access_unit(&pkt.data, pkt.pts)
    }

    fn flush(&mut self) -> Result<Vec<Frame>, Error> {
        Ok(Vec::new())
    }
}

fn validate_sps(sps: &SeqParameterSet) -> Result<(), Error> {
    if sps.chroma_info.chroma_format != ChromaFormat::YUV420
        || sps.chroma_info.separate_colour_plane_flag
    {
        return Err(Error::Decode("only 4:2:0 pictures are supported".into()));
    }
    if sps.chroma_info.bit_depth_luma_minus8 != 0 || sps.chroma_info.bit_depth_chroma_minus8 != 0 {
        return Err(Error::Decode("only 8-bit pictures are supported".into()));
    }
    if !matches!(sps.frame_mbs_flags, FrameMbsFlags::Frames) {
        return Err(Error::Decode("interlaced pictures are unsupported".into()));
    }
    if sps.gaps_in_frame_num_value_allowed_flag {
        return Err(Error::Decode("frame number gaps are unsupported".into()));
    }
    Ok(())
}

fn validate_pps(pps: &PicParameterSet) -> Result<(), Error> {
    if pps.slice_groups.is_some() {
        return Err(Error::Decode("FMO slice groups are unsupported".into()));
    }
    if pps.weighted_pred_flag {
        return Err(Error::Decode("weighted P prediction is unsupported".into()));
    }
    if pps.redundant_pic_cnt_present_flag {
        return Err(Error::Decode(
            "redundant coded pictures are unsupported".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAMERA_FIXTURE: &[u8] =
        include_bytes!("../../../tests/fixtures/tapo-1080p-cabac-8x8.h264");

    fn camera_parameter_sets() -> (SeqParameterSet, PicParameterSet) {
        let sps_bytes = annexb::nal_units(CAMERA_FIXTURE)
            .find(|nal| nal.first().is_some_and(|header| header & 0x1f == 7))
            .expect("fixture SPS missing");
        let sps_nal = RefNal::new(sps_bytes, &[], true);
        let sps = SeqParameterSet::from_bits(sps_nal.rbsp_bits()).expect("fixture SPS invalid");

        let pps_bytes = annexb::nal_units(CAMERA_FIXTURE)
            .find(|nal| nal.first().is_some_and(|header| header & 0x1f == 8))
            .expect("fixture PPS missing");
        let pps_nal = RefNal::new(pps_bytes, &[], true);
        let mut context = Context::default();
        context.put_seq_param_set(sps.clone());
        let pps =
            PicParameterSet::from_bits(&context, pps_nal.rbsp_bits()).expect("fixture PPS invalid");
        (sps, pps)
    }

    #[test]
    fn metadata_only_access_units_need_no_parameter_sets() {
        let mut frontend = Frontend::new();
        let access_unit = [0, 0, 1, 9, 0xf0, 0, 0, 1, 6, 0x80];
        assert!(frontend.parse_access_unit(&access_unit).unwrap().is_empty());
    }

    #[test]
    fn malformed_nal_header_is_rejected() {
        let mut frontend = Frontend::new();
        let access_unit = [0, 0, 1, 0x80];
        assert!(frontend.parse_access_unit(&access_unit).is_err());
    }

    #[test]
    fn slice_info_preserves_nal_ref_idc() {
        let mut frontend = Frontend::new();
        for access_unit in annexb::access_units(CAMERA_FIXTURE) {
            let expected = annexb::nal_units(access_unit)
                .find(|nal| {
                    nal.first()
                        .is_some_and(|header| header & 0x1f == 1 || header & 0x1f == 5)
                })
                .map(|nal| (nal[0] & 0x60) >> 5);
            let slices = frontend.parse_access_unit(access_unit).unwrap();
            if let Some(expected) = expected {
                assert!(!slices.is_empty());
                assert!(slices
                    .iter()
                    .all(|slice| slice.info.nal_ref_idc == expected));
            }
        }
    }

    #[test]
    fn unsupported_slice_partitions_and_extensions_are_rejected() {
        for nal_type in [2, 3, 4, 13, 14, 15, 16, 19, 20, 21, 22, 24] {
            let mut frontend = Frontend::new();
            let access_unit = [0, 0, 1, nal_type, 0x80];
            let error = frontend.parse_access_unit(&access_unit).unwrap_err();
            assert!(error
                .to_string()
                .contains("unsupported H.264 NAL unit type"));
        }
    }

    #[test]
    fn tapo_fixture_parameter_sets_reject_unsupported_reference_flags() {
        let (mut sps, mut pps) = camera_parameter_sets();
        assert!(!sps.gaps_in_frame_num_value_allowed_flag);
        assert!(!pps.weighted_pred_flag);
        assert!(!pps.redundant_pic_cnt_present_flag);

        sps.gaps_in_frame_num_value_allowed_flag = true;
        assert!(validate_sps(&sps)
            .unwrap_err()
            .to_string()
            .contains("frame number gaps"));

        pps.weighted_pred_flag = true;
        assert!(validate_pps(&pps)
            .unwrap_err()
            .to_string()
            .contains("weighted P prediction"));

        pps.weighted_pred_flag = false;
        pps.redundant_pic_cnt_present_flag = true;
        assert!(validate_pps(&pps)
            .unwrap_err()
            .to_string()
            .contains("redundant coded pictures"));
    }

    #[test]
    fn unsupported_sps_and_pps_features_are_rejected() {
        let (sps, mut pps) = camera_parameter_sets();

        for chroma in [
            ChromaFormat::Monochrome,
            ChromaFormat::YUV422,
            ChromaFormat::YUV444,
        ] {
            let mut unsupported = sps.clone();
            unsupported.chroma_info.chroma_format = chroma;
            assert!(validate_sps(&unsupported)
                .unwrap_err()
                .to_string()
                .contains("only 4:2:0"));
        }
        let mut high_depth = sps.clone();
        high_depth.chroma_info.bit_depth_luma_minus8 = 2;
        assert!(validate_sps(&high_depth)
            .unwrap_err()
            .to_string()
            .contains("only 8-bit"));

        let mut interlaced = sps.clone();
        interlaced.frame_mbs_flags = FrameMbsFlags::Fields {
            mb_adaptive_frame_field_flag: true,
        };
        assert!(validate_sps(&interlaced)
            .unwrap_err()
            .to_string()
            .contains("interlaced"));

        pps.slice_groups = Some(h264_reader::nal::pps::SliceGroup::Dispersed {
            num_slice_groups_minus1: 1,
        });
        assert!(validate_pps(&pps).unwrap_err().to_string().contains("FMO"));
    }
}
