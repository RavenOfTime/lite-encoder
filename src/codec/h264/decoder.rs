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
use super::picture::Dpb;
use super::picture_decode::PictureDecoder;
use super::slice::{self, CabacSlice};

/// Persistent parsing state for one H.264 elementary stream.
#[derive(Debug, Default)]
pub struct Frontend {
    context: Context,
    dpb: Option<Dpb>,
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
        if slices.iter().any(|slice| slice.info.picture != config) {
            return Err(Error::Decode(
                "access unit mixes picture configurations".into(),
            ));
        }
        let dpb = self
            .dpb
            .take()
            .unwrap_or_else(|| Dpb::new(config.max_refs, config.max_frame_num));
        let mut picture = PictureDecoder::with_dpb(config, dpb);
        for (slice_id, slice) in slices.iter().enumerate() {
            picture.decode_slice(slice, slice_id as u32)?;
        }
        let (decoded, dpb) = picture.finish();
        self.dpb = Some(dpb);
        Ok(vec![decoded.to_frame(config.crop, pts)])
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
    Ok(())
}

fn validate_pps(pps: &PicParameterSet) -> Result<(), Error> {
    if pps.slice_groups.is_some() {
        return Err(Error::Decode("FMO slice groups are unsupported".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
