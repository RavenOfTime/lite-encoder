//! WebM muxer.
//!
//! Written for recording rather than for file conversion, so the priorities
//! are: the file on disk is always playable, and a crash costs at most one
//! cluster. The Segment is left unknown-sized while open and clusters are
//! buffered then written whole, so a truncated file stays valid up to its
//! last complete cluster.

use super::ebml::{self, id};
use crate::media::time::{self, webm_block_offset, webm_ticks, MAX_CLUSTER_SPAN};
use crate::media::{Codec, Track, TrackKind};
use crate::Error;
use std::io::Write;
use std::time::Duration;

pub struct WebmMuxer<W: Write> {
    out: W,
    tracks: Vec<Track>,
    cluster: Vec<u8>,
    cluster_base: Option<Duration>,
    bytes_written: u64,
    last_pts: Duration,
    finalized: bool,
}

impl<W: Write> WebmMuxer<W> {
    /// Write the EBML header and track headers.
    ///
    /// Fails on any track WebM cannot legally carry, so an invalid file is
    /// never created in the first place.
    pub fn new(mut out: W, tracks: Vec<Track>) -> Result<Self, Error> {
        if tracks.is_empty() {
            return Err(Error::Mux("a WebM file needs at least one track".into()));
        }

        let mut head = Vec::new();
        Self::write_ebml_header(&mut head);
        ebml::write_id(&mut head, id::SEGMENT);
        head.extend_from_slice(&ebml::UNKNOWN_SIZE);
        Self::write_info(&mut head);
        Self::write_tracks(&mut head, &tracks)?;

        out.write_all(&head)?;
        let bytes_written = head.len() as u64;

        Ok(WebmMuxer {
            out,
            tracks,
            cluster: Vec::with_capacity(256 * 1024),
            cluster_base: None,
            bytes_written,
            last_pts: Duration::ZERO,
            finalized: false,
        })
    }

    fn write_ebml_header(out: &mut Vec<u8>) {
        let mut b = Vec::new();
        ebml::write_uint(&mut b, id::EBML_VERSION, 1);
        ebml::write_uint(&mut b, id::EBML_READ_VERSION, 1);
        ebml::write_uint(&mut b, id::EBML_MAX_ID_LENGTH, 4);
        ebml::write_uint(&mut b, id::EBML_MAX_SIZE_LENGTH, 8);
        ebml::write_string(&mut b, id::DOC_TYPE, "webm");
        ebml::write_uint(&mut b, id::DOC_TYPE_VERSION, 2);
        ebml::write_uint(&mut b, id::DOC_TYPE_READ_VERSION, 2);
        ebml::write_master(out, id::EBML, &b);
    }

    fn write_info(out: &mut Vec<u8>) {
        let mut b = Vec::new();
        ebml::write_uint(&mut b, id::TIMESTAMP_SCALE, time::WEBM_TIMESTAMP_SCALE_NS);
        ebml::write_string(&mut b, id::MUXING_APP, "liteenc");
        ebml::write_string(&mut b, id::WRITING_APP, "liteenc");
        // Duration is deliberately omitted: it is unknown while recording,
        // and players treat its absence as "live".
        ebml::write_master(out, id::INFO, &b);
    }

    fn write_tracks(out: &mut Vec<u8>, tracks: &[Track]) -> Result<(), Error> {
        let mut all = Vec::new();
        for t in tracks {
            let codec_id = t
                .codec
                .matroska_id()
                .ok_or_else(|| Error::Mux(format!("{:?} cannot be stored in WebM", t.codec)))?;
            if !t.codec.webm_legal() {
                return Err(Error::Mux(format!("{:?} is not WebM-legal", t.codec)));
            }

            let mut e = Vec::new();
            ebml::write_uint(&mut e, id::TRACK_NUMBER, t.id.0 as u64);
            ebml::write_uint(&mut e, id::TRACK_UID, t.id.0 as u64);
            ebml::write_uint(&mut e, id::FLAG_LACING, 0);
            ebml::write_string(&mut e, id::CODEC_ID, codec_id);

            match &t.kind {
                TrackKind::Video { width, height } => {
                    ebml::write_uint(&mut e, id::TRACK_TYPE, 1);
                    let mut v = Vec::new();
                    ebml::write_uint(&mut v, id::PIXEL_WIDTH, *width as u64);
                    ebml::write_uint(&mut v, id::PIXEL_HEIGHT, *height as u64);
                    ebml::write_master(&mut e, id::VIDEO, &v);
                }
                TrackKind::Audio {
                    sample_rate,
                    channels,
                } => {
                    ebml::write_uint(&mut e, id::TRACK_TYPE, 2);
                    let mut a = Vec::new();
                    ebml::write_float(&mut a, id::SAMPLING_FREQUENCY, *sample_rate as f64);
                    ebml::write_uint(&mut a, id::CHANNELS, *channels as u64);
                    ebml::write_master(&mut e, id::AUDIO, &a);
                }
            }

            // Opus in particular is undecodable without its OpusHead.
            if !t.extra_data.is_empty() {
                ebml::write_bytes(&mut e, id::CODEC_PRIVATE, &t.extra_data);
            } else if t.codec == Codec::Opus {
                return Err(Error::Mux(
                    "Opus track requires OpusHead in extra_data".into(),
                ));
            }

            ebml::write_master(&mut all, id::TRACK_ENTRY, &e);
        }
        ebml::write_master(out, id::TRACKS, &all);
        Ok(())
    }

    /// Append a packet.
    ///
    /// Starts a new cluster on a video keyframe, or when the current cluster
    /// has run long, so every cluster begins at a seekable point.
    pub fn write_packet(&mut self, pkt: &crate::media::Packet) -> Result<(), Error> {
        if self.finalized {
            return Err(Error::Mux("muxer already finalized".into()));
        }
        let track = self
            .tracks
            .iter()
            .find(|t| t.id == pkt.track)
            .ok_or_else(|| Error::Mux(format!("unknown track {:?}", pkt.track)))?;
        let is_video = track.codec.is_video();
        let track_num = track.id.0 as u64;

        let base = match self.cluster_base {
            None => {
                self.cluster_base = Some(pkt.pts);
                pkt.pts
            }
            Some(base) => {
                let long = pkt.pts.saturating_sub(base) >= MAX_CLUSTER_SPAN;
                if (is_video && pkt.keyframe) || long {
                    self.flush_cluster()?;
                    self.cluster_base = Some(pkt.pts);
                    pkt.pts
                } else {
                    base
                }
            }
        };

        let rel_ms = webm_block_offset(pkt.pts, base)?;

        ebml::write_simple_block(
            &mut self.cluster,
            track_num,
            rel_ms,
            pkt.keyframe,
            &pkt.data,
        );
        self.last_pts = self.last_pts.max(pkt.pts);
        Ok(())
    }

    fn flush_cluster(&mut self) -> Result<(), Error> {
        let Some(base) = self.cluster_base else {
            return Ok(());
        };
        if self.cluster.is_empty() {
            return Ok(());
        }

        let mut body = Vec::with_capacity(self.cluster.len() + 16);
        ebml::write_uint(&mut body, id::TIMESTAMP, webm_ticks(base));
        body.extend_from_slice(&self.cluster);

        let mut framed = Vec::with_capacity(body.len() + 16);
        ebml::write_master(&mut framed, id::CLUSTER, &body);

        self.out.write_all(&framed)?;
        self.bytes_written += framed.len() as u64;
        self.cluster.clear();
        Ok(())
    }

    /// Flush the open cluster so everything written so far is durable.
    pub fn flush(&mut self) -> Result<(), Error> {
        self.flush_cluster()?;
        self.cluster_base = None;
        self.out.flush()?;
        Ok(())
    }

    /// Close the file and report its size.
    pub fn finalize(mut self) -> Result<u64, Error> {
        self.flush()?;
        self.finalized = true;
        Ok(self.bytes_written)
    }

    /// Bytes on disk plus whatever is buffered in the open cluster.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written + self.cluster.len() as u64
    }

    /// Media duration seen so far.
    pub fn media_duration(&self) -> Duration {
        self.last_pts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Packet, TrackId};

    const CLUSTER_ID: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];

    fn video_track() -> Track {
        Track {
            id: TrackId(1),
            codec: Codec::Av1,
            kind: TrackKind::Video {
                width: 1920,
                height: 1080,
            },
            extra_data: vec![],
        }
    }

    fn pkt(ms: u64, key: bool) -> Packet {
        Packet {
            track: TrackId(1),
            pts: Duration::from_millis(ms),
            keyframe: key,
            data: bytes::Bytes::from_static(&[0x01, 0x02, 0x03]),
        }
    }

    fn drain(mut m: WebmMuxer<Vec<u8>>) -> Vec<u8> {
        m.flush().unwrap();
        m.out
    }

    fn count_clusters(out: &[u8]) -> usize {
        out.windows(4).filter(|w| *w == CLUSTER_ID).count()
    }

    #[test]
    fn writes_recognisable_webm_header() {
        let out = drain(WebmMuxer::new(Vec::new(), vec![video_track()]).unwrap());
        assert_eq!(&out[..4], &[0x1A, 0x45, 0xDF, 0xA3], "EBML magic");
        // The "webm" doctype is what makes browsers accept the file.
        assert!(out.windows(4).any(|w| w == b"webm"));
    }

    #[test]
    fn segment_size_is_unknown_while_live() {
        let out = drain(WebmMuxer::new(Vec::new(), vec![video_track()]).unwrap());
        let seg = out
            .windows(4)
            .position(|w| w == [0x18, 0x53, 0x80, 0x67])
            .expect("segment element");
        assert_eq!(&out[seg + 4..seg + 12], &ebml::UNKNOWN_SIZE);
    }

    #[test]
    fn rejects_codecs_webm_cannot_carry() {
        let t = Track {
            id: TrackId(1),
            codec: Codec::H264,
            kind: TrackKind::Video {
                width: 640,
                height: 480,
            },
            extra_data: vec![],
        };
        match WebmMuxer::new(Vec::new(), vec![t]) {
            Err(Error::Mux(_)) => {}
            Err(e) => panic!("wrong error: {e:?}"),
            Ok(_) => panic!("H.264 must be rejected: WebM cannot carry it"),
        }
    }

    #[test]
    fn rejects_opus_without_opushead() {
        let t = Track {
            id: TrackId(2),
            codec: Codec::Opus,
            kind: TrackKind::Audio {
                sample_rate: 48000,
                channels: 2,
            },
            extra_data: vec![],
        };
        assert!(WebmMuxer::new(Vec::new(), vec![t]).is_err());
    }

    #[test]
    fn keyframe_opens_a_new_cluster() {
        let mut m = WebmMuxer::new(Vec::new(), vec![video_track()]).unwrap();
        m.write_packet(&pkt(0, true)).unwrap();
        m.write_packet(&pkt(40, false)).unwrap();
        m.write_packet(&pkt(80, true)).unwrap();
        assert_eq!(count_clusters(&drain(m)), 2);
    }

    #[test]
    fn long_gop_is_split_before_block_offsets_overflow() {
        let mut m = WebmMuxer::new(Vec::new(), vec![video_track()]).unwrap();
        // A camera with a 60s GOP sends no keyframe for a minute. Without the
        // duration cap this would overflow the i16 block offset.
        for i in 0..600 {
            m.write_packet(&pkt(i * 100, i == 0)).unwrap();
        }
        assert!(count_clusters(&drain(m)) >= 12, "expected periodic splits");
    }

    #[test]
    fn tracks_are_required() {
        assert!(WebmMuxer::new(Vec::new(), vec![]).is_err());
    }

    #[test]
    fn reports_media_duration_and_size() {
        let mut m = WebmMuxer::new(Vec::new(), vec![video_track()]).unwrap();
        m.write_packet(&pkt(0, true)).unwrap();
        m.write_packet(&pkt(1000, false)).unwrap();
        assert_eq!(m.media_duration(), Duration::from_millis(1000));
        assert!(m.bytes_written() > 0);
    }
}
