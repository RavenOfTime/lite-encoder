//! Matroska (`.mkv`) and WebM demux.
//!
//! Reads what our own muxers write: `SimpleBlock`s (any number of tracks)
//! inside known-size `Cluster`s, under an unknown-size `Segment` (our muxers
//! never know the final size up front, so the reader has to accept that
//! shape too). `BlockGroup`/`Block` (used for lacing or per-block durations)
//! and non-default `TimestampScale` are out of scope until a fixture needs
//! them — the whole file is parsed into memory rather than streamed, which
//! matches the "cheap remux" scope this reader exists for.

use crate::media::time::{from_webm_ticks, WEBM_TIMESTAMP_SCALE_NS};
use crate::media::{Codec, Packet, Track, TrackId, TrackKind};
use crate::mux::ebml::id;
use crate::Error;

use super::Demuxer;

pub struct MkvDemuxer {
    tracks: Vec<Track>,
    packets: std::vec::IntoIter<Packet>,
}

impl MkvDemuxer {
    pub fn new(data: &[u8]) -> Result<Self, Error> {
        let (header_id, header_size, header_body_start) = read_element(data, 0)?;
        if header_id != id::EBML {
            return Err(Error::Demux("not an EBML file".into()));
        }
        // DocType isn't load-bearing: both webm and matroska parse the same.
        let after_header = header_body_start
            + header_size.ok_or_else(|| Error::Demux("EBML header has unknown size".into()))?;

        let (seg_id, seg_size, seg_body_start) = read_element(data, after_header)?;
        if seg_id != id::SEGMENT {
            return Err(Error::Demux("expected a Segment element".into()));
        }
        let seg_end = match seg_size {
            Some(size) => seg_body_start + size,
            // Unknown size: our own muxers only use this for Segment, and
            // Segment is the last top-level element, so it runs to EOF.
            None => data.len(),
        };
        if seg_end > data.len() {
            return Err(Error::Demux("Segment size overruns the file".into()));
        }

        let mut tracks = Vec::new();
        let mut scale_ns = WEBM_TIMESTAMP_SCALE_NS;
        let mut packets = Vec::new();

        let mut pos = seg_body_start;
        while pos < seg_end {
            let (eid, size, body_start) = read_element(data, pos)?;
            let size =
                size.ok_or_else(|| Error::Demux("only Segment may have an unknown size".into()))?;
            let body_end = body_start + size;
            if body_end > seg_end {
                return Err(Error::Demux("child element overruns Segment".into()));
            }

            match eid {
                id::INFO => scale_ns = read_timestamp_scale(&data[body_start..body_end])?,
                id::TRACKS => tracks = read_tracks(&data[body_start..body_end])?,
                id::CLUSTER => read_cluster(&data[body_start..body_end], scale_ns, &mut packets)?,
                // Cues, Attachments, Tags, Chapters, SeekHead: not needed to
                // recover packets, so skipped rather than rejected.
                _ => {}
            }

            pos = body_end;
        }

        if tracks.is_empty() {
            return Err(Error::Demux("no Tracks element found".into()));
        }
        packets.sort_by_key(|p: &Packet| p.pts);

        Ok(MkvDemuxer {
            tracks,
            packets: packets.into_iter(),
        })
    }
}

impl Demuxer for MkvDemuxer {
    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn read_packet(&mut self) -> Result<Option<Packet>, Error> {
        Ok(self.packets.next())
    }
}

fn read_timestamp_scale(info: &[u8]) -> Result<u64, Error> {
    let mut pos = 0;
    while pos < info.len() {
        let (eid, size, body_start) = read_element(info, pos)?;
        let size = size.ok_or_else(|| Error::Demux("Info element has unknown size".into()))?;
        let body_end = body_start + size;
        if eid == id::TIMESTAMP_SCALE {
            return Ok(read_uint(&info[body_start..body_end]));
        }
        pos = body_end;
    }
    Ok(WEBM_TIMESTAMP_SCALE_NS)
}

fn read_tracks(tracks_body: &[u8]) -> Result<Vec<Track>, Error> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < tracks_body.len() {
        let (eid, size, body_start) = read_element(tracks_body, pos)?;
        let size = size.ok_or_else(|| Error::Demux("TrackEntry has unknown size".into()))?;
        let body_end = body_start + size;
        if eid == id::TRACK_ENTRY {
            out.push(read_track_entry(&tracks_body[body_start..body_end])?);
        }
        pos = body_end;
    }
    Ok(out)
}

fn read_track_entry(entry: &[u8]) -> Result<Track, Error> {
    let mut number: Option<u32> = None;
    let mut codec_id: Option<String> = None;
    let mut extra_data = Vec::new();
    let mut video: Option<(u32, u32)> = None;
    let mut audio: Option<(u32, u8)> = None;

    let mut pos = 0;
    while pos < entry.len() {
        let (eid, size, body_start) = read_element(entry, pos)?;
        let size = size.ok_or_else(|| Error::Demux("TrackEntry child has unknown size".into()))?;
        let body_end = body_start + size;
        let body = &entry[body_start..body_end];

        match eid {
            id::TRACK_NUMBER => number = Some(read_uint(body) as u32),
            id::CODEC_ID => {
                codec_id = Some(
                    std::str::from_utf8(body)
                        .map_err(|_| Error::Demux("CodecID is not valid UTF-8".into()))?
                        .to_string(),
                )
            }
            id::CODEC_PRIVATE => extra_data = body.to_vec(),
            id::VIDEO => video = Some(read_video(body)?),
            id::AUDIO => audio = Some(read_audio(body)?),
            _ => {}
        }
        pos = body_end;
    }

    let number = number.ok_or_else(|| Error::Demux("TrackEntry missing TrackNumber".into()))?;
    let codec_id = codec_id.ok_or_else(|| Error::Demux("TrackEntry missing CodecID".into()))?;
    let codec = codec_from_matroska_id(&codec_id)?;

    let kind = if let Some((width, height)) = video {
        TrackKind::Video { width, height }
    } else if let Some((sample_rate, channels)) = audio {
        TrackKind::Audio {
            sample_rate,
            channels,
        }
    } else {
        return Err(Error::Demux(format!(
            "TrackEntry {number} has neither Video nor Audio"
        )));
    };

    Ok(Track {
        id: TrackId(number),
        codec,
        kind,
        extra_data,
    })
}

fn read_video(body: &[u8]) -> Result<(u32, u32), Error> {
    let mut width = None;
    let mut height = None;
    let mut pos = 0;
    while pos < body.len() {
        let (eid, size, body_start) = read_element(body, pos)?;
        let size = size.ok_or_else(|| Error::Demux("Video child has unknown size".into()))?;
        let body_end = body_start + size;
        match eid {
            id::PIXEL_WIDTH => width = Some(read_uint(&body[body_start..body_end]) as u32),
            id::PIXEL_HEIGHT => height = Some(read_uint(&body[body_start..body_end]) as u32),
            _ => {}
        }
        pos = body_end;
    }
    match (width, height) {
        (Some(w), Some(h)) => Ok((w, h)),
        _ => Err(Error::Demux("Video element missing dimensions".into())),
    }
}

fn read_audio(body: &[u8]) -> Result<(u32, u8), Error> {
    let mut sample_rate = None;
    let mut channels = None;
    let mut pos = 0;
    while pos < body.len() {
        let (eid, size, body_start) = read_element(body, pos)?;
        let size = size.ok_or_else(|| Error::Demux("Audio child has unknown size".into()))?;
        let body_end = body_start + size;
        match eid {
            id::SAMPLING_FREQUENCY => {
                sample_rate = Some(read_float(&body[body_start..body_end])? as u32)
            }
            id::CHANNELS => channels = Some(read_uint(&body[body_start..body_end]) as u8),
            _ => {}
        }
        pos = body_end;
    }
    match (sample_rate, channels) {
        (Some(r), Some(c)) => Ok((r, c)),
        _ => Err(Error::Demux(
            "Audio element missing sample rate or channels".into(),
        )),
    }
}

fn read_cluster(cluster: &[u8], scale_ns: u64, packets: &mut Vec<Packet>) -> Result<(), Error> {
    if scale_ns != WEBM_TIMESTAMP_SCALE_NS {
        return Err(Error::Demux(format!(
            "TimestampScale {scale_ns} ns is not supported; only {WEBM_TIMESTAMP_SCALE_NS} (1 ms) is"
        )));
    }

    let mut base_ticks: Option<u64> = None;
    let mut pos = 0;
    while pos < cluster.len() {
        let (eid, size, body_start) = read_element(cluster, pos)?;
        let size = size.ok_or_else(|| Error::Demux("Cluster child has unknown size".into()))?;
        let body_end = body_start + size;
        let body = &cluster[body_start..body_end];

        match eid {
            id::TIMESTAMP => base_ticks = Some(read_uint(body)),
            id::SIMPLE_BLOCK => {
                let base = base_ticks
                    .ok_or_else(|| Error::Demux("SimpleBlock before Cluster Timestamp".into()))?;
                packets.push(read_simple_block(body, base)?);
            }
            // BlockGroup (lacing, block-level durations) is not produced by
            // our muxers and is rejected rather than silently dropped.
            _ => {}
        }
        pos = body_end;
    }
    Ok(())
}

fn read_simple_block(block: &[u8], base_ticks: u64) -> Result<Packet, Error> {
    let (track, n) = read_vint_size(block, 0)
        .ok_or_else(|| Error::Demux("SimpleBlock: truncated track number".into()))?;
    let rest = &block[n..];
    if rest.len() < 3 {
        return Err(Error::Demux("SimpleBlock shorter than its header".into()));
    }
    let rel_ts = i16::from_be_bytes([rest[0], rest[1]]);
    let flags = rest[2];
    if flags & 0x06 != 0 {
        return Err(Error::Demux("laced SimpleBlock is not supported".into()));
    }
    let data = rest[3..].to_vec();
    let ticks = (base_ticks as i64 + rel_ts as i64).max(0) as u64;

    Ok(Packet {
        track: TrackId(track as u32),
        pts: from_webm_ticks(ticks),
        keyframe: flags & 0x80 != 0,
        data: bytes::Bytes::from(data),
    })
}

fn codec_from_matroska_id(codec_id: &str) -> Result<Codec, Error> {
    Ok(match codec_id {
        "V_MPEG4/ISO/AVC" => Codec::H264,
        "V_MPEGH/ISO/HEVC" => Codec::H265,
        "V_AV1" => Codec::Av1,
        "V_VP9" => Codec::Vp9,
        "V_VP8" => Codec::Vp8,
        "A_AAC" => Codec::Aac,
        "A_OPUS" => Codec::Opus,
        "A_VORBIS" => Codec::Vorbis,
        other => return Err(Error::Demux(format!("unsupported CodecID {other:?}"))),
    })
}

fn read_uint(body: &[u8]) -> u64 {
    body.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64)
}

fn read_float(body: &[u8]) -> Result<f64, Error> {
    match body.len() {
        4 => Ok(f32::from_be_bytes(body.try_into().unwrap()) as f64),
        8 => Ok(f64::from_be_bytes(body.try_into().unwrap())),
        n => Err(Error::Demux(format!("float element has odd length {n}"))),
    }
}

/// Read one element at `pos`: `(id, size, body_start)`. `size` is `None` for
/// an EBML "unknown size" marker.
fn read_element(data: &[u8], pos: usize) -> Result<(u32, Option<usize>, usize), Error> {
    let (id, id_len) = read_vint_id(data, pos)
        .ok_or_else(|| Error::Demux(format!("truncated element ID at offset {pos}")))?;
    let (size, size_len) = read_vint_size(data, pos + id_len)
        .ok_or_else(|| Error::Demux(format!("truncated element size at offset {pos}")))?;
    let body_start = pos + id_len + size_len;
    let unknown = size == (1u64 << (7 * size_len)) - 1;
    Ok((
        id,
        if unknown { None } else { Some(size as usize) },
        body_start,
    ))
}

/// Read an EBML ID VINT: length bytes, marker bit kept as part of the value
/// (element IDs are matched against their raw on-wire form).
fn read_vint_id(data: &[u8], pos: usize) -> Option<(u32, usize)> {
    let first = *data.get(pos)?;
    if first == 0 {
        return None;
    }
    let len = (1..=4).find(|&l| first & (0x80 >> (l - 1)) != 0)?;
    let bytes = data.get(pos..pos + len)?;
    let mut value = 0u32;
    for &b in bytes {
        value = (value << 8) | b as u32;
    }
    Some((value, len))
}

/// Read an EBML size VINT: length bytes, marker bit masked off.
fn read_vint_size(data: &[u8], pos: usize) -> Option<(u64, usize)> {
    let first = *data.get(pos)?;
    if first == 0 {
        return None;
    }
    let len = (1..=8).find(|&l| first & (0x80 >> (l - 1)) != 0)?;
    let bytes = data.get(pos..pos + len)?;
    let mut value = (first & (0xFFu16 >> len) as u8) as u64;
    for &b in &bytes[1..] {
        value = (value << 8) | b as u64;
    }
    Some((value, len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::MkvMuxer;
    use std::time::Duration;

    fn h264_track() -> Track {
        Track {
            id: TrackId(1),
            codec: Codec::H264,
            kind: TrackKind::Video {
                width: 1920,
                height: 1080,
            },
            extra_data: vec![0, 0, 0, 1, 0x67, 0x64],
        }
    }

    #[test]
    fn parses_track_and_packets_from_our_own_muxer() {
        let mut m = MkvMuxer::new(Vec::new(), vec![h264_track()]).unwrap();
        m.write_packet(&Packet {
            track: TrackId(1),
            pts: Duration::from_millis(0),
            keyframe: true,
            data: bytes::Bytes::from_static(&[1, 2, 3]),
        })
        .unwrap();
        m.write_packet(&Packet {
            track: TrackId(1),
            pts: Duration::from_millis(40),
            keyframe: false,
            data: bytes::Bytes::from_static(&[4, 5]),
        })
        .unwrap();
        m.flush().unwrap();
        let bytes = m.into_writer_for_test();

        let mut demux = MkvDemuxer::new(&bytes).unwrap();
        assert_eq!(demux.tracks().len(), 1);
        assert_eq!(demux.tracks()[0].codec, Codec::H264);
        assert_eq!(demux.tracks()[0].extra_data, h264_track().extra_data);

        let p0 = demux.read_packet().unwrap().unwrap();
        assert_eq!(p0.pts, Duration::from_millis(0));
        assert!(p0.keyframe);
        assert_eq!(&p0.data[..], &[1, 2, 3]);

        let p1 = demux.read_packet().unwrap().unwrap();
        assert_eq!(p1.pts, Duration::from_millis(40));
        assert!(!p1.keyframe);
        assert_eq!(&p1.data[..], &[4, 5]);

        assert!(demux.read_packet().unwrap().is_none());
    }

    #[test]
    fn rejects_non_ebml_input() {
        assert!(MkvDemuxer::new(&[0xff; 16]).is_err());
    }

    /// WebM differs from Matroska only in DocType and its codec list, so this
    /// reader covers both — which is what `registry::support` claims for
    /// `Container::WebM`'s read cell, and what lets `liteenc probe` open a
    /// `.webm` this crate just wrote.
    #[test]
    fn reads_back_a_webm_written_by_the_webm_muxer() {
        let track = Track {
            id: TrackId(1),
            codec: Codec::Av1,
            kind: TrackKind::Video {
                width: 640,
                height: 360,
            },
            extra_data: vec![0x81, 0x00, 0x0c, 0x00],
        };

        let mut bytes = Vec::new();
        let mut m = crate::mux::WebmMuxer::new(&mut bytes, vec![track]).unwrap();
        m.write_packet(&Packet {
            track: TrackId(1),
            pts: Duration::from_millis(0),
            keyframe: true,
            data: bytes::Bytes::from_static(&[7, 7, 7]),
        })
        .unwrap();
        m.finalize().unwrap();

        let mut demux = MkvDemuxer::new(&bytes).unwrap();
        assert_eq!(demux.tracks().len(), 1);
        assert_eq!(demux.tracks()[0].codec, Codec::Av1);
        let pkt = demux.read_packet().unwrap().unwrap();
        assert_eq!(&pkt.data[..], &[7, 7, 7]);
        assert!(demux.read_packet().unwrap().is_none());
    }

    #[test]
    fn keeps_packets_on_their_own_track_across_multiple_tracks() {
        let audio_track = Track {
            id: TrackId(2),
            codec: Codec::Aac,
            kind: TrackKind::Audio {
                sample_rate: 48_000,
                channels: 2,
            },
            extra_data: vec![0x12, 0x10],
        };

        let mut m = MkvMuxer::new(Vec::new(), vec![h264_track(), audio_track.clone()]).unwrap();
        m.write_packet(&Packet {
            track: TrackId(1),
            pts: Duration::from_millis(0),
            keyframe: true,
            data: bytes::Bytes::from_static(&[1, 2, 3]),
        })
        .unwrap();
        m.write_packet(&Packet {
            track: TrackId(2),
            pts: Duration::from_millis(10),
            keyframe: false,
            data: bytes::Bytes::from_static(&[9, 9]),
        })
        .unwrap();
        m.flush().unwrap();
        let bytes = m.into_writer_for_test();

        let mut demux = MkvDemuxer::new(&bytes).unwrap();
        assert_eq!(demux.tracks().len(), 2);
        assert!(demux.tracks().iter().any(|t| t.codec == Codec::H264));
        assert!(demux.tracks().iter().any(|t| t.codec == Codec::Aac));

        let p0 = demux.read_packet().unwrap().unwrap();
        assert_eq!(p0.track, TrackId(1));
        let p1 = demux.read_packet().unwrap().unwrap();
        assert_eq!(p1.track, TrackId(2));
        assert_eq!(&p1.data[..], &[9, 9]);
        assert!(demux.read_packet().unwrap().is_none());
    }
}
