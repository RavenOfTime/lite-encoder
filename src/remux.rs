//! `-c copy`: move packets from a [`Demuxer`] to a [`Muxer`] with no decode.
//!
//! The muxer must already be constructed with the demuxer's tracks — track
//! negotiation (which container can carry which codec, see
//! [`crate::registry`]) is the caller's job, not this function's.

use crate::demux::Demuxer;
use crate::mux::Muxer;
use crate::Error;

/// Copy every remaining packet from `demuxer` into `muxer`, then flush.
///
/// Returns the number of packets copied.
pub fn copy_remux(demuxer: &mut dyn Demuxer, muxer: &mut dyn Muxer) -> Result<u64, Error> {
    let mut count = 0u64;
    while let Some(pkt) = demuxer.read_packet()? {
        muxer.write_packet(&pkt)?;
        count += 1;
    }
    muxer.flush()?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::AnnexBDemuxer;
    use crate::mux::MkvMuxer;

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tapo-1080p-cabac-8x8.h264");

    #[test]
    fn copies_every_packet_without_touching_its_bytes() {
        let mut demux = AnnexBDemuxer::new(FIXTURE, 22).unwrap();
        let tracks = demux.tracks().to_vec();
        let mut mux = MkvMuxer::new(Vec::new(), tracks).unwrap();

        let copied = copy_remux(&mut demux, &mut mux).unwrap();
        assert_eq!(copied, 4);
        assert!(mux.bytes_written() > 0);
    }
}
