//! PTS is preserved across H.264 decode and AV1 encode on the job timeline.

#![cfg(feature = "av1")]

use std::time::Duration;

use lite_encoder::codec::av1::Av1Encoder;
use lite_encoder::codec::h264::decoder::Frontend;
use lite_encoder::media::{Encoder, TrackId};

const FIXTURE: &[u8] = include_bytes!("fixtures/tapo-1080p-cabac-8x8.h264");

#[test]
fn decode_then_encode_preserves_pts_on_the_job_timeline() {
    let mut decoder = Frontend::new();
    let mut frames = Vec::new();
    for (i, au) in lite_encoder::codec::h264::annexb::access_units(FIXTURE)
        .into_iter()
        .enumerate()
    {
        let pts = Duration::from_millis(i as u64 * 40);
        let decoded = decoder.decode_access_unit(au, pts).expect("decode");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].pts, pts);
        frames.push(decoded.into_iter().next().unwrap());
    }

    let mut encoder = Av1Encoder::new(TrackId(1), 1920, 1080, 30, 1_000_000).unwrap();
    for frame in &frames {
        encoder.encode(frame).expect("encode");
    }
    let packets = encoder.flush().expect("flush");
    assert!(!packets.is_empty());

    for frame in &frames {
        assert!(
            packets.iter().any(|pkt| pkt.pts == frame.pts),
            "missing packet for frame at {:?}",
            frame.pts
        );
    }
}

#[test]
fn packet_pts_maps_to_webm_ticks_without_loss_at_millisecond_resolution() {
    use lite_encoder::media::time::{from_webm_ticks, webm_ticks};
    use lite_encoder::media::{Codec, Packet, Track, TrackKind};
    use lite_encoder::mux::WebmMuxer;

    let pts = Duration::from_millis(1_240);
    let mut mux = WebmMuxer::new(
        Vec::new(),
        vec![Track {
            id: TrackId(1),
            codec: Codec::Av1,
            kind: TrackKind::Video {
                width: 16,
                height: 16,
            },
            extra_data: vec![],
        }],
    )
    .unwrap();
    mux.write_packet(&Packet {
        track: TrackId(1),
        pts,
        keyframe: true,
        data: bytes::Bytes::from_static(&[0x01]),
    })
    .unwrap();
    assert_eq!(mux.media_duration(), pts);
    assert_eq!(from_webm_ticks(webm_ticks(pts)), pts);
}
