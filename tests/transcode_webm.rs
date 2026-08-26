//! Decode → AV1 encode → WebM mux on the checked-in camera fixture.

#![cfg(feature = "av1")]

use std::time::Duration;

use lite_encoder::codec::av1::Av1Encoder;
use lite_encoder::codec::h264::{annexb, decoder::Frontend};
use lite_encoder::media::{Codec, Encoder, Track, TrackId, TrackKind};
use lite_encoder::mux::WebmMuxer;

const FIXTURE: &[u8] = include_bytes!("fixtures/tapo-1080p-cabac-8x8.h264");

#[test]
fn fixture_decode_encode_muxes_a_playable_webm() {
    let mut decoder = Frontend::new();
    let mut frames = Vec::new();
    for (i, au) in annexb::access_units(FIXTURE).into_iter().enumerate() {
        let pts = Duration::from_millis(i as u64 * 1000 / 22);
        let decoded = decoder.decode_access_unit(au, pts).expect("decode");
        frames.extend(decoded);
    }
    assert_eq!(frames.len(), 4);
    assert_eq!((frames[0].width, frames[0].height), (1920, 1080));

    let mut encoder = Av1Encoder::new(TrackId(1), 1920, 1080, 22, 1_000_000).unwrap();
    let track = Track {
        id: TrackId(1),
        codec: Codec::Av1,
        kind: TrackKind::Video {
            width: 1920,
            height: 1080,
        },
        extra_data: encoder.extra_data(),
    };
    assert!(!track.extra_data.is_empty(), "av1C must be present");

    let mut out = Vec::new();
    let mut mux = WebmMuxer::new(&mut out, vec![track]).unwrap();
    let mut packet_count = 0usize;
    for frame in &frames {
        for packet in encoder.encode(frame).expect("encode") {
            packet_count += 1;
            mux.write_packet(&packet).expect("mux");
        }
    }
    for packet in encoder.flush().expect("flush") {
        packet_count += 1;
        mux.write_packet(&packet).expect("mux flush");
    }
    assert!(packet_count >= frames.len());
    let duration = mux.media_duration();
    let bytes = mux.finalize().expect("finalize");
    assert_eq!(bytes, out.len() as u64);
    assert!(bytes > 1_000, "expected a non-trivial WebM, got {bytes}");
    assert!(duration >= frames.last().unwrap().pts);
    // EBML header magic
    assert_eq!(&out[..4], &[0x1A, 0x45, 0xDF, 0xA3]);
}
