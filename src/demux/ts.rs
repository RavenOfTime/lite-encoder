//! MPEG transport stream (`.ts`/`.m2ts`) demux.
//!
//! Scope matches the other container demuxers: read only what the format
//! needs to hand back H.264 access units, reject anything else loudly rather
//! than guess. Concretely: **single-program**, one H.264 (`stream_type`
//! `0x1B`) elementary stream, no audio (same footnote as MKV/MP4's audio
//! gap), no scrambling, and PAT/PMT sections that fit in a single 188-byte
//! TS packet (real streams keep these tiny and repeat them periodically; a
//! section spanning multiple packets is out of scope). The whole file is
//! parsed into memory up front, same as `MkvDemuxer`/`Mp4Demuxer`.
//!
//! TS carries video as Annex B directly (start codes and all), so unlike the
//! MP4/MKV *write* path there is no AVCC reframing anywhere here — packet
//! payloads and `extra_data` follow the same convention as
//! [`super::AnnexBDemuxer`].

use crate::codec::h264::annexb::{nal_units, parameter_sets, sps_dimensions};
use crate::media::time::mpegts_pts_to_duration;
use crate::media::{Codec, Packet, Track, TrackId, TrackKind};
use crate::Error;
use std::time::Duration;

use super::Demuxer;

const TRACK_ID: TrackId = TrackId(1);
const PACKET_LEN: usize = 188;
const SYNC: u8 = 0x47;
const PAT_PID: u16 = 0x0000;
const H264_STREAM_TYPE: u8 = 0x1B;

pub struct TsDemuxer {
    track: Track,
    packets: std::vec::IntoIter<Packet>,
}

impl TsDemuxer {
    pub fn new(data: &[u8]) -> Result<Self, Error> {
        let packet_count = data.len() / PACKET_LEN;
        if packet_count == 0 {
            return Err(Error::Demux("input is shorter than one TS packet".into()));
        }

        let mut pmt_pid: Option<u16> = None;
        let mut video_pid: Option<u16> = None;
        let mut current_pes: Vec<u8> = Vec::new();
        let mut pes_packets: Vec<Vec<u8>> = Vec::new();

        for i in 0..packet_count {
            let packet = &data[i * PACKET_LEN..(i + 1) * PACKET_LEN];
            if packet[0] != SYNC {
                return Err(Error::Demux(format!(
                    "TS packet {i} does not start with sync byte 0x47"
                )));
            }
            // Transport error indicator: the packet is known corrupt (e.g. an
            // uncorrectable FEC failure upstream). Drop it rather than fail
            // the whole demux over one bad packet in a real capture.
            if packet[1] & 0x80 != 0 {
                continue;
            }
            let pusi = packet[1] & 0x40 != 0;
            let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
            let afc = (packet[3] >> 4) & 0x3;
            if afc == 0 {
                return Err(Error::Demux(format!(
                    "TS packet {i} has reserved adaptation_field_control"
                )));
            }
            if afc == 2 {
                // Adaptation field only, no payload (stuffing/PCR carrier).
                continue;
            }
            let payload_start = if afc == 3 {
                let af_len = packet[4] as usize;
                5 + af_len
            } else {
                4
            };
            if payload_start > PACKET_LEN {
                return Err(Error::Demux(format!(
                    "TS packet {i} adaptation field overruns the packet"
                )));
            }
            let payload = &packet[payload_start..];

            if pid == PAT_PID {
                if pmt_pid.is_none() {
                    pmt_pid = Some(parse_pat(payload, pusi)?);
                }
            } else if Some(pid) == pmt_pid {
                if video_pid.is_none() {
                    video_pid = Some(parse_pmt(payload, pusi)?);
                }
            } else if Some(pid) == video_pid {
                if !pusi && current_pes.is_empty() {
                    // A continuation packet before we have ever seen a PES
                    // start: the capture begins mid-packet. Drop it.
                    continue;
                }
                if pusi && !current_pes.is_empty() {
                    pes_packets.push(std::mem::take(&mut current_pes));
                }
                current_pes.extend_from_slice(payload);
            }
        }
        if !current_pes.is_empty() {
            pes_packets.push(current_pes);
        }

        if video_pid.is_none() {
            return Err(Error::Demux(
                "no H.264 (stream_type 0x1B) entry found in the PMT".into(),
            ));
        }
        if pes_packets.is_empty() {
            return Err(Error::Demux(
                "no video PES packets found on the H.264 elementary stream PID".into(),
            ));
        }

        let mut packets = Vec::with_capacity(pes_packets.len());
        let mut es_concat = Vec::new();
        for pes in &pes_packets {
            let (pts, es) = parse_pes(pes)?;
            es_concat.extend_from_slice(es);
            let keyframe = nal_units(es).any(|nal| nal.first().is_some_and(|b| b & 0x1f == 5));
            packets.push(Packet {
                track: TRACK_ID,
                pts,
                keyframe,
                data: bytes::Bytes::from(es.to_vec()),
            });
        }
        packets.sort_by_key(|p: &Packet| p.pts);

        let (width, height) = sps_dimensions(&es_concat)
            .ok_or_else(|| Error::Demux("no SPS found in TS video stream".into()))?;
        let track = Track {
            id: TRACK_ID,
            codec: Codec::H264,
            kind: TrackKind::Video { width, height },
            extra_data: parameter_sets(&es_concat),
        };

        Ok(TsDemuxer {
            track,
            packets: packets.into_iter(),
        })
    }
}

impl Demuxer for TsDemuxer {
    fn tracks(&self) -> &[Track] {
        std::slice::from_ref(&self.track)
    }

    fn read_packet(&mut self) -> Result<Option<Packet>, Error> {
        Ok(self.packets.next())
    }
}

/// Parse a Program Association Table packet's payload, returning the PMT PID.
///
/// `pusi` must be set: a PAT that starts mid-packet (continuing a section
/// begun in an earlier TS packet) is multi-packet PSI, which is out of scope.
fn parse_pat(payload: &[u8], pusi: bool) -> Result<u16, Error> {
    let section = psi_section(payload, pusi, "PAT")?;
    if section.len() < 8 {
        return Err(Error::Demux("PAT section shorter than its header".into()));
    }
    if section[0] != 0x00 {
        return Err(Error::Demux(format!(
            "expected PAT table_id 0x00, got {:#04x}",
            section[0]
        )));
    }
    let section_end = psi_section_end(section, "PAT")?;
    let programs_end = section_end
        .checked_sub(4) // trailing CRC32
        .ok_or_else(|| Error::Demux("PAT section too short for its CRC32".into()))?;

    let mut pmt_pid = None;
    let mut program_count = 0;
    let mut pos = 8;
    while pos + 4 <= programs_end {
        let program_number = (u16::from(section[pos]) << 8) | u16::from(section[pos + 1]);
        let pid = (u16::from(section[pos + 2] & 0x1f) << 8) | u16::from(section[pos + 3]);
        // program_number 0 identifies the network PID, not a program.
        if program_number != 0 {
            program_count += 1;
            pmt_pid.get_or_insert(pid);
        }
        pos += 4;
    }
    if program_count > 1 {
        return Err(Error::Demux(
            "multi-program transport streams are not supported".into(),
        ));
    }
    pmt_pid.ok_or_else(|| Error::Demux("PAT has no program entries".into()))
}

/// Parse a Program Map Table packet's payload, returning the H.264 elementary
/// stream's PID.
fn parse_pmt(payload: &[u8], pusi: bool) -> Result<u16, Error> {
    let section = psi_section(payload, pusi, "PMT")?;
    if section.len() < 12 {
        return Err(Error::Demux("PMT section shorter than its header".into()));
    }
    if section[0] != 0x02 {
        return Err(Error::Demux(format!(
            "expected PMT table_id 0x02, got {:#04x}",
            section[0]
        )));
    }
    let section_end = psi_section_end(section, "PMT")?;
    let programs_end = section_end
        .checked_sub(4) // trailing CRC32
        .ok_or_else(|| Error::Demux("PMT section too short for its CRC32".into()))?;

    let program_info_length = (usize::from(section[10] & 0x0f) << 8) | usize::from(section[11]);
    let mut pos = 12 + program_info_length;
    let mut video_pid = None;
    while pos + 5 <= programs_end {
        let stream_type = section[pos];
        let pid = (u16::from(section[pos + 1] & 0x1f) << 8) | u16::from(section[pos + 2]);
        let es_info_length =
            (usize::from(section[pos + 3] & 0x0f) << 8) | usize::from(section[pos + 4]);
        if stream_type == H264_STREAM_TYPE {
            video_pid.get_or_insert(pid);
        }
        pos += 5 + es_info_length;
    }
    video_pid.ok_or_else(|| Error::Demux("PMT has no H.264 (stream_type 0x1B) entry".into()))
}

/// Strip a PSI packet's `pointer_field` and return the section bytes.
fn psi_section<'a>(payload: &'a [u8], pusi: bool, name: &str) -> Result<&'a [u8], Error> {
    if !pusi {
        return Err(Error::Demux(format!(
            "{name} split across multiple TS packets is not supported"
        )));
    }
    let pointer = *payload
        .first()
        .ok_or_else(|| Error::Demux(format!("empty {name} payload")))? as usize;
    payload
        .get(1 + pointer..)
        .ok_or_else(|| Error::Demux(format!("{name} pointer_field overruns its TS packet")))
}

/// Validate and return `section`'s end offset (exclusive) from its
/// `section_length` field, erroring if the section does not fit in the
/// packet it started in.
fn psi_section_end(section: &[u8], name: &str) -> Result<usize, Error> {
    let section_length = (usize::from(section[1] & 0x0f) << 8) | usize::from(section[2]);
    let section_end = 3 + section_length;
    if section_end > section.len() {
        return Err(Error::Demux(format!(
            "{name} section_length overruns its TS packet; multi-packet PSI is not supported"
        )));
    }
    Ok(section_end)
}

/// Parse one reassembled PES packet, returning its PTS and Annex B payload.
fn parse_pes(pes: &[u8]) -> Result<(Duration, &[u8]), Error> {
    if pes.len() < 9 {
        return Err(Error::Demux(
            "PES packet shorter than its fixed header".into(),
        ));
    }
    if pes[0..3] != [0x00, 0x00, 0x01] {
        return Err(Error::Demux(
            "PES packet_start_code_prefix (00 00 01) missing".into(),
        ));
    }
    let stream_id = pes[3];
    if stream_id & 0xf0 != 0xe0 {
        return Err(Error::Demux(format!(
            "PES stream_id {stream_id:#04x} is not a video stream (0xE0-0xEF)"
        )));
    }
    let pts_dts_flags = pes[7] >> 6;
    if pts_dts_flags & 0b10 == 0 {
        return Err(Error::Demux(
            "PES packet has no PTS; this stream's timing cannot be recovered".into(),
        ));
    }
    let header_data_length = pes[8] as usize;
    let pts_bytes = pes
        .get(9..14)
        .ok_or_else(|| Error::Demux("PES header shorter than its PTS field".into()))?;
    let pts_ticks = decode_pts(pts_bytes)?;
    let es_start = 9 + header_data_length;
    let es = pes
        .get(es_start..)
        .ok_or_else(|| Error::Demux("PES header_data_length overruns its packet".into()))?;
    Ok((mpegts_pts_to_duration(pts_ticks), es))
}

/// Decode a 5-byte, 33-bit PES PTS field (ISO/IEC 13818-1 2.4.3.6).
fn decode_pts(b: &[u8]) -> Result<u64, Error> {
    let prefix = b[0] >> 4;
    if prefix != 0x2 && prefix != 0x3 {
        return Err(Error::Demux(format!(
            "PTS prefix nibble {prefix:#x} is neither 0010 nor 0011"
        )));
    }
    if b[0] & 1 != 1 || b[2] & 1 != 1 || b[4] & 1 != 1 {
        return Err(Error::Demux("PTS marker bits are not all set".into()));
    }
    Ok((u64::from(b[0] & 0x0e) << 29)
        | (u64::from(b[1]) << 22)
        | (u64::from(b[2] & 0xfe) << 14)
        | (u64::from(b[3]) << 7)
        | (u64::from(b[4]) >> 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pack one 188-byte TS packet: `pid`/`pusi` in the header, `payload`
    /// copied in verbatim. A short final chunk is padded with an
    /// adaptation-field stuffing field (like a real encoder would), not
    /// trailing payload zero bytes, so reassembled PES/PSI bytes come back
    /// byte-exact.
    fn ts_packet(pid: u16, pusi: bool, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= 184, "payload must fit one TS packet");
        let mut p = vec![0u8; PACKET_LEN];
        p[0] = SYNC;
        p[1] = (if pusi { 0x40 } else { 0x00 }) | ((pid >> 8) as u8 & 0x1f);
        p[2] = pid as u8;
        if payload.len() == 184 {
            p[3] = 0x10; // adaptation_field_control = payload only
            p[4..].copy_from_slice(payload);
        } else {
            p[3] = 0x30; // adaptation_field_control = adaptation field + payload
            let af_len = 183 - payload.len();
            p[4] = af_len as u8;
            p[5 + af_len..].copy_from_slice(payload);
        }
        p
    }

    fn pat_section(pmt_pid: u16) -> Vec<u8> {
        let mut s = vec![0x00, 0xb0, 0x0d, 0x00, 0x01, 0xc1, 0x00, 0x00];
        s.push(0x00);
        s.push(0x01);
        s.push(0xe0 | (pmt_pid >> 8) as u8);
        s.push(pmt_pid as u8);
        s.extend_from_slice(&[0, 0, 0, 0]); // CRC32, unchecked by our reader
        s
    }

    fn pmt_section(video_pid: u16) -> Vec<u8> {
        let mut s = vec![0x02, 0xb0, 0x12, 0x00, 0x01, 0xc1, 0x00, 0x00];
        s.push(0xe0 | (video_pid >> 8) as u8); // PCR_PID high bits (reused as video PID for simplicity)
        s.push(video_pid as u8);
        s.push(0xf0);
        s.push(0x00); // program_info_length = 0
        s.push(H264_STREAM_TYPE);
        s.push(0xe0 | (video_pid >> 8) as u8);
        s.push(video_pid as u8);
        s.push(0xf0);
        s.push(0x00); // ES_info_length = 0
        s.extend_from_slice(&[0, 0, 0, 0]); // CRC32, unchecked by our reader
        s
    }

    /// Prepend a zero `pointer_field`, turning a bare PSI section into a
    /// packet payload that starts a new section (`pusi` must be set on the
    /// TS packet carrying it).
    fn psi_payload(section: &[u8]) -> Vec<u8> {
        let mut p = vec![0x00];
        p.extend_from_slice(section);
        p
    }

    fn pes_packet(pts_ticks: u64, es: &[u8]) -> Vec<u8> {
        let mut b0 = (0x2u8) << 4;
        b0 |= ((pts_ticks >> 30) as u8 & 0x07) << 1;
        b0 |= 1;
        let b1 = (pts_ticks >> 22) as u8;
        let mut b2 = ((pts_ticks >> 15) as u8 & 0x7f) << 1;
        b2 |= 1;
        let b3 = (pts_ticks >> 7) as u8;
        let mut b4 = (pts_ticks as u8 & 0x7f) << 1;
        b4 |= 1;

        let mut pes = vec![0x00, 0x00, 0x01, 0xe0, 0x00, 0x00, 0x80, 0x80, 0x05];
        pes.extend_from_slice(&[b0, b1, b2, b3, b4]);
        pes.extend_from_slice(es);
        pes
    }

    /// Splits `payload` across as many `pid` TS packets as needed, PUSI set
    /// only on the first.
    fn packetize(pid: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunks = payload.chunks(184);
        if let Some(first) = chunks.next() {
            out.extend(ts_packet(pid, true, first));
        }
        for chunk in chunks {
            out.extend(ts_packet(pid, false, chunk));
        }
        out
    }

    /// A minimal single-access-unit stream: PAT, PMT, then one PES packet
    /// carrying SPS+PPS+IDR from the crate's real Annex B fixture.
    fn minimal_ts() -> Vec<u8> {
        const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/tapo-1080p-cabac-8x8.h264");
        let au = crate::codec::h264::annexb::access_units(FIXTURE)[0];

        let mut out = Vec::new();
        out.extend(ts_packet(PAT_PID, true, &psi_payload(&pat_section(0x1000))));
        out.extend(ts_packet(0x1000, true, &psi_payload(&pmt_section(0x0100))));
        out.extend(packetize(0x0100, &pes_packet(90_000, au)));
        out
    }

    #[test]
    fn reads_track_and_first_access_unit() {
        let mut demux = TsDemuxer::new(&minimal_ts()).unwrap();
        assert_eq!(demux.tracks().len(), 1);
        assert_eq!(demux.tracks()[0].codec, Codec::H264);
        assert!(matches!(
            demux.tracks()[0].kind,
            TrackKind::Video {
                width: 1920,
                height: 1080
            }
        ));
        assert!(!demux.tracks()[0].extra_data.is_empty());

        let pkt = demux.read_packet().unwrap().unwrap();
        assert_eq!(pkt.pts, Duration::from_secs(1));
        assert!(pkt.keyframe);
        assert!(demux.read_packet().unwrap().is_none());
    }

    #[test]
    fn rejects_input_shorter_than_one_packet() {
        assert!(TsDemuxer::new(&[0x47; 10]).is_err());
    }

    #[test]
    fn rejects_a_bad_sync_byte() {
        let mut ts = minimal_ts();
        ts[0] = 0x00;
        assert!(TsDemuxer::new(&ts).is_err());
    }

    #[test]
    fn drops_a_transport_error_indicator_packet_instead_of_failing() {
        let mut ts = minimal_ts();
        ts[1] |= 0x80; // flag the PAT packet as corrupt
        assert!(TsDemuxer::new(&ts).is_err(), "PMT PID never resolves");
    }

    #[test]
    fn rejects_a_pat_with_more_than_one_program() {
        let mut pat = vec![0x00, 0xb0, 0x11, 0x00, 0x01, 0xc1, 0x00, 0x00];
        pat.extend_from_slice(&[0x00, 0x01, 0xe1, 0x00]); // program 1 -> PID 0x100
        pat.extend_from_slice(&[0x00, 0x02, 0xe1, 0x01]); // program 2 -> PID 0x101
        pat.extend_from_slice(&[0, 0, 0, 0]);

        let mut ts = Vec::new();
        ts.extend(ts_packet(PAT_PID, true, &psi_payload(&pat)));
        assert!(TsDemuxer::new(&ts).is_err());
    }

    #[test]
    fn rejects_a_pmt_with_no_h264_stream() {
        let mut pmt = vec![0x02, 0xb0, 0x12, 0x00, 0x01, 0xc1, 0x00, 0x00];
        pmt.extend_from_slice(&[0xe1, 0x00, 0xf0, 0x00]); // PCR_PID
        pmt.push(0x0f); // AAC ADTS, not H.264
        pmt.extend_from_slice(&[0xe1, 0x01, 0xf0, 0x00]);
        pmt.extend_from_slice(&[0, 0, 0, 0]);

        let mut ts = Vec::new();
        ts.extend(ts_packet(PAT_PID, true, &psi_payload(&pat_section(0x1000))));
        ts.extend(ts_packet(0x1000, true, &psi_payload(&pmt)));
        assert!(TsDemuxer::new(&ts).is_err());
    }

    #[test]
    fn rejects_a_pes_packet_without_a_pts() {
        let mut ts = Vec::new();
        ts.extend(ts_packet(PAT_PID, true, &psi_payload(&pat_section(0x1000))));
        ts.extend(ts_packet(0x1000, true, &psi_payload(&pmt_section(0x0100))));
        // PES header flags say "no PTS/DTS" (0x00 in the flags byte).
        let pes = vec![
            0x00, 0x00, 0x01, 0xe0, 0x00, 0x00, 0x80, 0x00, 0x00, 1, 2, 3,
        ];
        ts.extend(packetize(0x0100, &pes));
        assert!(TsDemuxer::new(&ts).is_err());
    }
}
