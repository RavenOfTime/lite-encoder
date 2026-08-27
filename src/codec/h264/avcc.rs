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
//! `h264_mp4toannexb` bitstream filter, and both directions live here:
//! [`access_unit_to_avcc`] / [`parameter_set_record`] for muxing into MP4 or
//! Matroska, and [`access_unit_to_annexb`] / [`annexb_parameter_sets`] for
//! handing an MP4 or Matroska sample to the decoder, which only speaks Annex
//! B. Callers must run these explicitly — muxers and the decoder stay
//! framing-agnostic and do not do it for you. [`is_avcc_record`] is how a
//! caller decides which direction, if any, a track needs.

use super::annexb::nal_units;
use crate::Error;

/// The four-byte Annex B start code. Three bytes is also legal, but four is
/// what every parameter set and access unit this crate emits uses.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Shortest possible `avcC` record: the five-byte fixed header, the SPS
/// count, and a PPS count. Anything shorter cannot be one.
const MIN_RECORD_LEN: usize = 7;

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

/// Whether `extra_data` is an `avcC` configuration record rather than Annex B
/// parameter sets.
///
/// One byte decides it, the same way ffmpeg's H.264 parser does:
/// `configurationVersion` is 1, while Annex B extra data always opens with a
/// three- or four-byte start code, so its first byte is zero.
pub fn is_avcc_record(extra_data: &[u8]) -> bool {
    extra_data.len() >= MIN_RECORD_LEN && extra_data[0] == 1
}

/// Size in bytes of the NAL length prefix on samples described by this
/// `avcC` record.
///
/// [`access_unit_to_avcc`] always writes four, but a file from another muxer
/// may use one or two, so readers have to honour the record rather than
/// assume.
pub fn nal_length_size(avcc_record: &[u8]) -> Result<usize, Error> {
    if !is_avcc_record(avcc_record) {
        return Err(Error::Demux("not an avcC configuration record".into()));
    }
    Ok((avcc_record[4] & 0x03) as usize + 1)
}

/// Annex B start code-prefixed SPS then PPS from an `avcC` record: the
/// inverse of [`parameter_set_record`].
///
/// This is what has to be prepended to an Annex B access unit — at minimum
/// the first, and in practice every keyframe, so the stream stays decodable
/// from any random access point — when feeding MP4 or Matroska samples to the
/// decoder. An `avcC` file carries its parameter sets once, out of band, and
/// never again.
pub fn annexb_parameter_sets(avcc_record: &[u8]) -> Result<Vec<u8>, Error> {
    if !is_avcc_record(avcc_record) {
        return Err(Error::Demux("not an avcC configuration record".into()));
    }
    let mut out = Vec::with_capacity(avcc_record.len() + 16);

    // numOfSequenceParameterSets is five bits; the PPS count is a plain u8.
    let sps_count = (avcc_record[5] & 0x1f) as usize;
    let pos = copy_parameter_sets(avcc_record, 6, sps_count, &mut out)?;

    let pps_count = *avcc_record
        .get(pos)
        .ok_or_else(|| Error::Demux("avcC record ends before its PPS count".into()))?
        as usize;
    copy_parameter_sets(avcc_record, pos + 1, pps_count, &mut out)?;

    if out.is_empty() {
        return Err(Error::Demux("avcC record carries no parameter sets".into()));
    }
    Ok(out)
}

/// Copy `count` length-prefixed parameter sets starting at `pos`, each with an
/// Annex B start code, returning the offset just past the last one.
fn copy_parameter_sets(
    record: &[u8],
    mut pos: usize,
    count: usize,
    out: &mut Vec<u8>,
) -> Result<usize, Error> {
    for _ in 0..count {
        let len = record
            .get(pos..pos + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
            .ok_or_else(|| Error::Demux("avcC parameter set length is truncated".into()))?;
        pos += 2;
        let nal = record
            .get(pos..pos + len)
            .ok_or_else(|| Error::Demux("avcC parameter set overruns the record".into()))?;
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
        pos += len;
    }
    Ok(pos)
}

/// One AVCC sample's NALs with each `length_size`-byte length prefix replaced
/// by a four-byte Annex B start code.
///
/// Parameter sets are *not* reinserted: they live in the `avcC` record, and
/// [`annexb_parameter_sets`] is what puts them back in band. `length_size`
/// comes from [`nal_length_size`].
pub fn access_unit_to_annexb(avcc_unit: &[u8], length_size: usize) -> Result<Vec<u8>, Error> {
    if !matches!(length_size, 1 | 2 | 4) {
        return Err(Error::Demux(format!(
            "unsupported avcC NAL length size {length_size}"
        )));
    }
    let mut out = Vec::with_capacity(avcc_unit.len() + 16);
    let mut pos = 0;
    while pos < avcc_unit.len() {
        let len = avcc_unit
            .get(pos..pos + length_size)
            .map(|b| b.iter().fold(0usize, |acc, byte| (acc << 8) | *byte as usize))
            .ok_or_else(|| Error::Demux("AVCC sample ends inside a NAL length".into()))?;
        pos += length_size;
        // A zero-length NAL is not a NAL; emitting a bare start code for one
        // would desynchronise every Annex B scanner downstream.
        if len == 0 {
            return Err(Error::Demux("AVCC sample has a zero-length NAL".into()));
        }
        let nal = avcc_unit
            .get(pos..pos + len)
            .ok_or_else(|| Error::Demux("AVCC NAL length overruns the sample".into()))?;
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
        pos += len;
    }
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

    /// The fixture's Annex B parameter sets, as an `avcC` record.
    fn fixture_record() -> Vec<u8> {
        parameter_set_record(&fixture_parameter_sets()).unwrap()
    }

    fn fixture_parameter_sets() -> Vec<u8> {
        crate::demux::AnnexBDemuxer::new(FIXTURE, 22).unwrap().tracks()[0]
            .extra_data
            .clone()
    }

    #[test]
    fn parameter_sets_survive_a_round_trip_through_avcc() {
        let annexb = fixture_parameter_sets();
        let back = annexb_parameter_sets(&parameter_set_record(&annexb).unwrap()).unwrap();
        // The demuxer emits SPS then PPS with four-byte start codes, which is
        // exactly the shape `annexb_parameter_sets` rebuilds.
        assert_eq!(back, annexb);
    }

    #[test]
    fn access_units_survive_a_round_trip_through_avcc() {
        let au = super::super::annexb::access_units(FIXTURE)[0];
        let back = access_unit_to_annexb(&access_unit_to_avcc(au), 4).unwrap();

        // Not byte-identical to `au`: the trip through AVCC drops parameter
        // sets and AUDs by design, so compare against the NALs it keeps.
        let mut expected = Vec::new();
        for nal in super::super::annexb::nal_units(au) {
            if matches!(nal[0] & 0x1f, 1 | 5 | 6) {
                expected.extend_from_slice(&START_CODE);
                expected.extend_from_slice(nal);
            }
        }
        assert_eq!(back, expected);
    }

    #[test]
    fn avcc_records_are_told_apart_from_annexb_extra_data() {
        assert!(is_avcc_record(&fixture_record()));
        assert!(!is_avcc_record(&fixture_parameter_sets()));
        assert!(!is_avcc_record(&[]));
        assert!(!is_avcc_record(&[1, 2, 3]));
    }

    #[test]
    fn nal_length_size_comes_from_the_record() {
        assert_eq!(nal_length_size(&fixture_record()).unwrap(), 4);
        let mut two_byte = fixture_record();
        two_byte[4] = 0xFD; // lengthSizeMinusOne = 1
        assert_eq!(nal_length_size(&two_byte).unwrap(), 2);
        assert!(nal_length_size(&[0, 0, 0, 1, 0x67]).is_err());
    }

    #[test]
    fn truncated_avcc_input_is_an_error_not_a_panic() {
        let avcc = access_unit_to_avcc(super::super::annexb::access_units(FIXTURE)[0]);
        assert!(access_unit_to_annexb(&avcc[..avcc.len() - 1], 4).is_err());
        assert!(access_unit_to_annexb(&avcc[..2], 4).is_err());
        assert!(access_unit_to_annexb(&[0, 0, 0, 0], 4).is_err());
        assert!(access_unit_to_annexb(&avcc, 3).is_err());

        let record = fixture_record();
        assert!(annexb_parameter_sets(&record[..record.len() - 1]).is_err());
    }
}
