//! Matroska (`.mkv`) muxer: [`MatroskaMuxer`] with the full Matroska codec
//! list rather than WebM's restricted subset — H.264 in particular, which is
//! what the `-c copy` remux path exists for.

use super::matroska::MatroskaMuxer;
use crate::media::{Codec, Packet, Track};
use crate::Error;
use std::io::Write;
use std::time::Duration;

pub struct MkvMuxer<W: Write>(MatroskaMuxer<W>);

impl<W: Write> MkvMuxer<W> {
    /// Write the EBML header and track headers.
    ///
    /// Fails on any track with no Matroska `CodecID` at all (see
    /// [`Codec::matroska_id`]); everything that has one is accepted.
    pub fn new(out: W, tracks: Vec<Track>) -> Result<Self, Error> {
        Ok(MkvMuxer(MatroskaMuxer::new(
            out,
            tracks,
            "matroska",
            &|c: Codec| c.matroska_id().is_some(),
        )?))
    }

    pub fn write_packet(&mut self, pkt: &Packet) -> Result<(), Error> {
        self.0.write_packet(pkt)
    }

    pub fn flush(&mut self) -> Result<(), Error> {
        self.0.flush()
    }

    pub fn finalize(self) -> Result<u64, Error> {
        self.0.finalize()
    }

    pub fn bytes_written(&self) -> u64 {
        self.0.bytes_written()
    }

    pub fn media_duration(&self) -> Duration {
        self.0.media_duration()
    }
}

#[cfg(test)]
impl<W: Write> MkvMuxer<W> {
    /// Unwrap the writer for `demux::mkv`'s round-trip tests. Test-only: the
    /// muxer itself never exposes the raw bytes it writes.
    pub fn into_writer_for_test(self) -> W {
        self.0.into_writer()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Packet, TrackId, TrackKind};

    fn h264_track() -> Track {
        Track {
            id: TrackId(1),
            codec: Codec::H264,
            kind: TrackKind::Video {
                width: 1920,
                height: 1080,
            },
            extra_data: vec![0x00, 0x00, 0x00, 0x01, 0x67],
        }
    }

    fn pkt(ms: u64, key: bool) -> Packet {
        Packet {
            track: TrackId(1),
            pts: Duration::from_millis(ms),
            keyframe: key,
            data: bytes::Bytes::from_static(&[0xAA, 0xBB]),
        }
    }

    fn drain(mut m: MkvMuxer<Vec<u8>>) -> Vec<u8> {
        m.flush().unwrap();
        m.0.into_writer()
    }

    #[test]
    fn writes_a_matroska_doctype() {
        let out = drain(MkvMuxer::new(Vec::new(), vec![h264_track()]).unwrap());
        assert_eq!(&out[..4], &[0x1A, 0x45, 0xDF, 0xA3], "EBML magic");
        assert!(out.windows(8).any(|w| w == b"matroska"));
    }

    #[test]
    fn accepts_h264_which_webm_rejects() {
        assert!(MkvMuxer::new(Vec::new(), vec![h264_track()]).is_ok());
    }

    #[test]
    fn round_trips_packets_through_a_cluster() {
        let mut m = MkvMuxer::new(Vec::new(), vec![h264_track()]).unwrap();
        m.write_packet(&pkt(0, true)).unwrap();
        m.write_packet(&pkt(40, false)).unwrap();
        let out = drain(m);
        assert!(out.windows(4).any(|w| w == [0x1F, 0x43, 0xB6, 0x75]));
    }

    #[test]
    fn rejects_a_codec_with_no_matroska_codec_id() {
        let t = Track {
            id: TrackId(2),
            codec: Codec::Pcmu,
            kind: TrackKind::Audio {
                sample_rate: 8000,
                channels: 1,
            },
            extra_data: vec![],
        };
        assert!(MkvMuxer::new(Vec::new(), vec![t]).is_err());
    }
}
