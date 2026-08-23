//! Emit a structurally complete WebM file for external validation.
//!
//! Payload bytes are synthetic, so no decoder will render this; the point is
//! that the container itself parses. Run with:
//!     cargo run --example write_webm -- out.webm

use lite_encoder::media::{Codec, Packet, Track, TrackId, TrackKind};
use lite_encoder::mux::WebmMuxer;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "out.webm".into());

    let tracks = vec![
        Track {
            id: TrackId(1),
            codec: Codec::Av1,
            kind: TrackKind::Video {
                width: 1280,
                height: 720,
            },
            extra_data: vec![],
        },
        Track {
            id: TrackId(2),
            codec: Codec::Opus,
            kind: TrackKind::Audio {
                sample_rate: 48000,
                channels: 2,
            },
            // Minimal OpusHead: magic, version, channels, pre-skip, rate, gain, mapping.
            extra_data: b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec(),
        },
    ];

    let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut mux = WebmMuxer::new(file, tracks)?;

    // 10 seconds at 25 fps with a 2-second GOP, plus 20 ms audio frames.
    for i in 0..250u64 {
        let pts = Duration::from_millis(i * 40);
        mux.write_packet(&Packet {
            track: TrackId(1),
            pts,
            keyframe: i % 50 == 0,
            data: bytes::Bytes::from(vec![0xA5; 512]),
        })?;
        for j in 0..2 {
            mux.write_packet(&Packet {
                track: TrackId(2),
                pts: pts + Duration::from_millis(j * 20),
                keyframe: true,
                data: bytes::Bytes::from(vec![0x5A; 80]),
            })?;
        }
    }

    let duration = mux.media_duration();
    let bytes = mux.finalize()?;
    println!("wrote {path}: {bytes} bytes, {duration:?} of media");
    Ok(())
}
