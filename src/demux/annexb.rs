//! Elementary H.264 Annex B as a [`Demuxer`].
//!
//! Elementary streams carry no timestamps, so `frame_rate` is required to
//! synthesize PTS on read — the same convention the benches and
//! `transcode_webm` test already use by hand.

use std::time::Duration;

use h264_reader::nal::sps::SeqParameterSet;
use h264_reader::nal::Nal;

use super::Demuxer;
use crate::codec::h264::annexb::{access_units, nal_units};
use crate::media::{Codec, Packet, Track, TrackId, TrackKind};
use crate::Error;

const TRACK_ID: TrackId = TrackId(1);

pub struct AnnexBDemuxer {
    track: Track,
    units: std::vec::IntoIter<Vec<u8>>,
    frame_rate: u32,
    index: u64,
}

impl AnnexBDemuxer {
    pub fn new(data: &[u8], frame_rate: u32) -> Result<Self, Error> {
        if frame_rate == 0 {
            return Err(Error::Demux("frame rate must be nonzero".into()));
        }

        let (width, height) = sps_dimensions(data)
            .ok_or_else(|| Error::Demux("no SPS found in Annex B stream".into()))?;

        let track = Track {
            id: TRACK_ID,
            codec: Codec::H264,
            kind: TrackKind::Video { width, height },
            extra_data: parameter_sets(data),
        };

        let units: Vec<Vec<u8>> = access_units(data).into_iter().map(<[u8]>::to_vec).collect();

        Ok(AnnexBDemuxer {
            track,
            units: units.into_iter(),
            frame_rate,
            index: 0,
        })
    }
}

impl Demuxer for AnnexBDemuxer {
    fn tracks(&self) -> &[Track] {
        std::slice::from_ref(&self.track)
    }

    fn read_packet(&mut self) -> Result<Option<Packet>, Error> {
        let Some(data) = self.units.next() else {
            return Ok(None);
        };
        let keyframe = nal_units(&data).any(|nal| nal.first().is_some_and(|b| b & 0x1f == 5));
        let pts = Duration::from_millis(self.index * 1000 / self.frame_rate as u64);
        self.index += 1;

        Ok(Some(Packet {
            track: TRACK_ID,
            pts,
            keyframe,
            data: bytes::Bytes::from(data),
        }))
    }
}

/// Pixel dimensions from the stream's first SPS.
fn sps_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    nal_units(data).find_map(|nal| {
        if nal.first()? & 0x1f != 7 {
            return None;
        }
        let rn = h264_reader::nal::RefNal::new(nal, &[], true);
        SeqParameterSet::from_bits(rn.rbsp_bits())
            .ok()?
            .pixel_dimensions()
            .ok()
    })
}

/// The stream's first SPS and PPS, start codes included, as `extra_data`.
fn parameter_sets(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut have_sps = false;
    let mut have_pps = false;
    for nal in nal_units(data) {
        let kind = match nal.first() {
            Some(b) => b & 0x1f,
            None => continue,
        };
        let take = (kind == 7 && !have_sps) || (kind == 8 && !have_pps);
        if !take {
            if have_sps && have_pps {
                break;
            }
            continue;
        }
        have_sps |= kind == 7;
        have_pps |= kind == 8;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/tapo-1080p-cabac-8x8.h264");

    #[test]
    fn reads_the_declared_track_and_every_access_unit() {
        let mut demux = AnnexBDemuxer::new(FIXTURE, 22).unwrap();
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

        let mut count = 0;
        let mut last_pts = None;
        while let Some(pkt) = demux.read_packet().unwrap() {
            assert_eq!(pkt.track, TRACK_ID);
            if let Some(prev) = last_pts {
                assert!(pkt.pts > prev, "pts must be strictly increasing");
            }
            last_pts = Some(pkt.pts);
            count += 1;
        }
        assert_eq!(count, 4);
        assert!(
            demux.read_packet().unwrap().is_none(),
            "stream stays exhausted"
        );
    }

    #[test]
    fn first_packet_is_a_keyframe() {
        let mut demux = AnnexBDemuxer::new(FIXTURE, 22).unwrap();
        let first = demux.read_packet().unwrap().unwrap();
        assert!(first.keyframe);
    }

    #[test]
    fn rejects_a_zero_frame_rate() {
        assert!(AnnexBDemuxer::new(FIXTURE, 0).is_err());
    }

    #[test]
    fn rejects_a_stream_without_an_sps() {
        assert!(AnnexBDemuxer::new(&[0, 0, 1, 0x41, 0x80], 30).is_err());
    }
}
