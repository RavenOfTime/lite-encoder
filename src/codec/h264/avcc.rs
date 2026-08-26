//! Annex B ⇄ AVCC bitstream reframing.
//!
//! Annex B (start codes) is what elementary streams and our decoder use.
//! ISOBMFF (MP4) and Matroska's `V_MPEG4/ISO/AVC` both require the other
//! format instead: each NAL length-prefixed, parameter sets pulled out of the
//! per-sample data and into an `avcC` configuration record carried once as
//! track `extra_data`.
//!
//! This is a reframing, not a transcode: no bit of picture data changes, only
//! how NAL boundaries are marked. It is the pure-Rust equivalent of ffmpeg's
//! `h264_mp4toannexb` bitstream filter, run in the direction MP4/MKV muxing
//! needs. Callers must run it explicitly when remuxing Annex B input into
//! either container — muxers stay codec-agnostic and do not do this for you.

use super::annexb::nal_units;
use crate::Error;

/// One access unit's VCL (and SEI) NALs, start codes stripped and each
/// prefixed with its 4-byte big-endian length instead.
///
/// Parameter sets (SPS/PPS) and access unit delimiters are dropped: once
/// `parameter_set_record` has captured them, they must not recur in-band, or
/// strict AVCC readers will reject the sample.
pub fn access_unit_to_avcc(annexb_unit: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nal_units(annexb_unit) {
        let kind = match nal.first() {
            Some(b) => b & 0x1f,
            None => continue,
        };
        // 1=non-IDR slice, 5=IDR slice, 6=SEI. 7/8/9 (SPS/PPS/AUD) are
        // exactly the types `parameter_set_record` already carries or that
        // have no place in an AVCC sample.
        if !matches!(kind, 1 | 5 | 6) {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// Build an `avcC` configuration record from Annex B `extra_data` — start
/// code-prefixed SPS then PPS, exactly what [`super::super::annexb`]'s
/// demuxer stores.
///
/// Only the single-SPS, single-PPS case is supported: cameras and the
/// encoders this crate targets don't renegotiate parameter sets mid-stream,
/// and ISO/IEC 14496-15 allows more only to cover that rarer case.
pub fn parameter_set_record(annexb_extra_data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut sps: Option<&[u8]> = None;
    let mut pps: Option<&[u8]> = None;
    for nal in nal_units(annexb_extra_data) {
        match nal.first().map(|b| b & 0x1f) {
            Some(7) if sps.is_none() => sps = Some(nal),
            Some(8) if pps.is_none() => pps = Some(nal),
            _ => {}
        }
    }
    let sps = sps.ok_or_else(|| Error::Mux("no SPS in H.264 extra_data".into()))?;
    let pps = pps.ok_or_else(|| Error::Mux("no PPS in H.264 extra_data".into()))?;
    if sps.len() < 4 {
        return Err(Error::Mux("SPS too short to carry profile/level".into()));
    }

    let mut out = Vec::with_capacity(11 + sps.len() + pps.len());
    out.push(1); // configurationVersion
    out.push(sps[1]); // AVCProfileIndication
    out.push(sps[2]); // profile_compatibility
    out.push(sps[3]); // AVCLevelIndication
    out.push(0xFF); // reserved(6)=1s, lengthSizeMinusOne=3 (4-byte lengths)
    out.push(0xE1); // reserved(3)=1s, numOfSequenceParameterSets=1
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1); // numOfPictureParameterSets
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::Demuxer;

    const FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/tapo-1080p-cabac-8x8.h264");

    #[test]
    fn access_unit_carries_no_start_codes_or_parameter_sets() {
        let au = super::super::annexb::access_units(FIXTURE)[0];
        let avcc = access_unit_to_avcc(au);
        assert!(!avcc.is_empty());
        // First 4 bytes are a NAL length, not a start code.
        let len = u32::from_be_bytes(avcc[..4].try_into().unwrap()) as usize;
        assert_eq!(len + 4, avcc.len(), "single-NAL first access unit");
        let kind = avcc[4] & 0x1f;
        assert_eq!(kind, 5, "IDR slice, not SPS/PPS/AUD");
    }

    #[test]
    fn parameter_set_record_has_the_avcc_header_shape() {
        let extra_data = crate::demux::AnnexBDemuxer::new(FIXTURE, 22)
            .unwrap()
            .tracks()[0]
            .extra_data
            .clone();
        let record = parameter_set_record(&extra_data).unwrap();
        assert_eq!(record[0], 1, "configurationVersion");
        assert_eq!(record[4] & 0x03, 3, "4-byte NAL length field");
        assert_eq!(record[5] & 0x1F, 1, "exactly one SPS");
    }

    #[test]
    fn rejects_extra_data_missing_a_parameter_set() {
        assert!(parameter_set_record(&[]).is_err());
    }
}
