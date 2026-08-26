//! WebM muxer: [`super::matroska::MatroskaMuxer`] restricted to WebM-legal
//! codecs, with the `"webm"` DocType browsers require.

use super::matroska::MatroskaMuxer;
use crate::media::{Codec, Packet, Track};
use crate::Error;
use std::io::Write;
use std::time::Duration;

pub struct WebmMuxer<W: Write>(MatroskaMuxer<W>);

impl<W: Write> WebmMuxer<W> {
    /// Write the EBML header and track headers.
    ///
    /// Fails on any track WebM cannot legally carry, so an invalid file is
    /// never created in the first place.
    pub fn new(out: W, tracks: Vec<Track>) -> Result<Self, Error> {
        Ok(WebmMuxer(MatroskaMuxer::new(
            out,
            tracks,
            "webm",
            &Codec::webm_legal,
        )?))
    }

    /// Append a packet.
    ///
    /// Starts a new cluster on a video keyframe, or when the current cluster
    /// has run long, so every cluster begins at a seekable point.
    pub fn write_packet(&mut self, pkt: &Packet) -> Result<(), Error> {
        self.0.write_packet(pkt)
    }

    /// Flush the open cluster so everything written so far is durable.
    pub fn flush(&mut self) -> Result<(), Error> {
        self.0.flush()
    }

    /// Close the file and report its size.
    pub fn finalize(self) -> Result<u64, Error> {
        self.0.finalize()
    }

    /// Bytes on disk plus whatever is buffered in the open cluster.
    pub fn bytes_written(&self) -> u64 {
        self.0.bytes_written()
    }

    /// Media duration seen so far.
    pub fn media_duration(&self) -> Duration {
        self.0.media_duration()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Packet, TrackId, TrackKind};

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
        m.0.into_writer()
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
        assert_eq!(&out[seg + 4..seg + 12], &crate::mux::ebml::UNKNOWN_SIZE);
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
