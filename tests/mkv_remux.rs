//! `-c copy`: Annex B → MKV → back, on the checked-in camera fixture.
//!
//! No decode happens anywhere in this path; it exists to prove the demux →
//! copy → mux → demux loop is lossless for the bytes and timing that matter,
//! independent of the codec inside.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lite_encoder::demux::{AnnexBDemuxer, Demuxer, MkvDemuxer};
use lite_encoder::media::{Codec, TrackKind};
use lite_encoder::mux::MkvMuxer;
use lite_encoder::remux::copy_remux;

const FIXTURE: &[u8] = include_bytes!("fixtures/tapo-1080p-cabac-8x8.h264");

/// `MkvMuxer::finalize` only reports a byte count, by design (same contract
/// as `WebmMuxer`): the muxer owns the writer, and production code never
/// needs it back. A shared buffer lets this test read the bytes anyway,
/// without adding a writer-accessor to the muxer's public API.
#[derive(Clone)]
struct SharedBuf(Rc<RefCell<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

#[test]
fn fixture_round_trips_through_mkv_without_decoding() {
    let mut source = AnnexBDemuxer::new(FIXTURE, 22).unwrap();
    let tracks = source.tracks().to_vec();
    assert_eq!(tracks.len(), 1);

    let buf = SharedBuf(Rc::new(RefCell::new(Vec::new())));
    let mut mux = MkvMuxer::new(buf.clone(), tracks).unwrap();
    let copied = copy_remux(&mut source, &mut mux).unwrap();
    assert_eq!(copied, 4, "fixture has 4 access units");
    let bytes_written = mux.finalize().unwrap();
    assert!(bytes_written > 1_000);

    let out = buf.0.borrow().clone();
    assert_eq!(out.len() as u64, bytes_written);

    let mut roundtrip = MkvDemuxer::new(&out).unwrap();
    assert_eq!(roundtrip.tracks().len(), 1);
    assert_eq!(roundtrip.tracks()[0].codec, Codec::H264);
    assert!(matches!(
        roundtrip.tracks()[0].kind,
        TrackKind::Video {
            width: 1920,
            height: 1080
        }
    ));

    let mut original = AnnexBDemuxer::new(FIXTURE, 22).unwrap();
    let mut count = 0;
    while let Some(orig_pkt) = original.read_packet().unwrap() {
        let rt_pkt = roundtrip
            .read_packet()
            .unwrap()
            .expect("round trip must not lose packets");
        assert_eq!(rt_pkt.pts, orig_pkt.pts);
        assert_eq!(rt_pkt.keyframe, orig_pkt.keyframe);
        assert_eq!(
            rt_pkt.data, orig_pkt.data,
            "copy must not touch payload bytes"
        );
        count += 1;
    }
    assert_eq!(count, 4);
    assert!(roundtrip.read_packet().unwrap().is_none());
}
