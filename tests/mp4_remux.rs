//! Annex B → AVCC bitstream reframe → MP4 → back, on the camera fixture.
//!
//! Unlike `tests/mkv_remux.rs`, this is not a byte-identical copy: MP4
//! requires length-prefixed NALs and an `avcC` record instead of Annex B
//! start codes, so `codec::h264::avcc` reframes each packet on the way in.
//! The test proves that reframing is lossless and deterministic by
//! recomputing the expected AVCC bytes independently and comparing.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lite_encoder::codec::h264::avcc;
use lite_encoder::demux::{AnnexBDemuxer, Demuxer, Mp4Demuxer};
use lite_encoder::media::{Codec, Packet, Track, TrackKind};
use lite_encoder::mux::Mp4Muxer;

const FIXTURE: &[u8] = include_bytes!("fixtures/tapo-1080p-cabac-8x8.h264");

/// `Mp4Muxer::finalize` only reports a byte count and consumes the muxer, so
/// there is no writer to retrieve afterward. A shared buffer lets this test
/// read the bytes anyway, without adding a writer-accessor to the muxer.
#[derive(Clone)]
struct SharedBuf(Rc<RefCell<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn fixture_round_trips_through_mp4_via_the_avcc_bitstream_filter() {
    let source = AnnexBDemuxer::new(FIXTURE, 22).unwrap();
    let tracks: Vec<Track> = source
        .tracks()
        .iter()
        .cloned()
        .map(|mut t| {
            t.extra_data = avcc::parameter_set_record(&t.extra_data).unwrap();
            t
        })
        .collect();

    let buf = SharedBuf(Rc::new(RefCell::new(Vec::new())));
    let mut mux = Mp4Muxer::new(buf.clone(), tracks).unwrap();

    let mut expected_avcc = Vec::new();
    let mut original = AnnexBDemuxer::new(FIXTURE, 22).unwrap();
    while let Some(pkt) = original.read_packet().unwrap() {
        let avcc_data = avcc::access_unit_to_avcc(&pkt.data);
        expected_avcc.push((pkt.pts, pkt.keyframe, avcc_data.clone()));
        mux.write_packet(&Packet {
            data: bytes::Bytes::from(avcc_data),
            ..pkt
        })
        .unwrap();
    }
    assert_eq!(expected_avcc.len(), 4, "fixture has 4 access units");

    let bytes_written = mux.finalize().unwrap();
    let out = buf.0.borrow().clone();
    assert_eq!(out.len() as u64, bytes_written);

    let mut roundtrip = Mp4Demuxer::new(&out).unwrap();
    assert_eq!(roundtrip.tracks().len(), 1);
    assert_eq!(roundtrip.tracks()[0].codec, Codec::H264);
    assert!(matches!(
        roundtrip.tracks()[0].kind,
        TrackKind::Video {
            width: 1920,
            height: 1080
        }
    ));

    let mut count = 0;
    for (pts, keyframe, avcc_data) in expected_avcc {
        let pkt = roundtrip
            .read_packet()
            .unwrap()
            .expect("round trip must not lose packets");
        assert_eq!(pkt.pts, pts);
        assert_eq!(pkt.keyframe, keyframe);
        assert_eq!(
            &pkt.data[..],
            &avcc_data[..],
            "AVCC bytes must survive untouched"
        );
        count += 1;
    }
    assert_eq!(count, 4);
    assert!(roundtrip.read_packet().unwrap().is_none());
}
