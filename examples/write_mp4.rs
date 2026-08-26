//! Annex B camera fixture → MP4, for external structural validation.
//!
//! MP4 requires length-prefixed NALs and an `avcC` configuration record, not
//! Annex B start codes, so this is also the reference example for the
//! Annex B → AVCC bitstream reframing every real MP4 remux needs. Run with:
//!     cargo run --example write_mp4 -- out.mp4
//!     python tools/mp4_check.py out.mp4

use lite_encoder::codec::h264::avcc;
use lite_encoder::demux::{AnnexBDemuxer, Demuxer};
use lite_encoder::media::{Packet, Track};
use lite_encoder::mux::Mp4Muxer;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "out.mp4".into());
    let fixture: &[u8] = include_bytes!("../tests/fixtures/tapo-1080p-cabac-8x8.h264");

    let mut demux = AnnexBDemuxer::new(fixture, 22)?;
    let tracks: Vec<Track> = demux
        .tracks()
        .iter()
        .cloned()
        .map(|mut t| -> Result<Track, Box<dyn std::error::Error>> {
            t.extra_data = avcc::parameter_set_record(&t.extra_data)?;
            Ok(t)
        })
        .collect::<Result<_, _>>()?;

    let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut mux = Mp4Muxer::new(file, tracks)?;

    while let Some(pkt) = demux.read_packet()? {
        mux.write_packet(&Packet {
            data: bytes::Bytes::from(avcc::access_unit_to_avcc(&pkt.data)),
            ..pkt
        })?;
    }

    let bytes = mux.finalize()?;
    println!("wrote {path}: {bytes} bytes");
    Ok(())
}
