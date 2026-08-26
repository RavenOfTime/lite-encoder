//! Demux: read compressed [`Packet`]s and their [`Track`] layout from an input.
//!
//! One concrete implementation exists today, [`annexb::AnnexBDemuxer`], for
//! elementary H.264. Container demuxers (MKV, MP4, TS) are P2 work; this
//! trait is the seam they will land behind.

pub mod annexb;
pub mod mkv;
pub mod mp4;

pub use annexb::AnnexBDemuxer;
pub use mkv::MkvDemuxer;
pub use mp4::Mp4Demuxer;

use crate::media::{Packet, Track};
use crate::Error;

/// Read compressed packets out of a container or elementary stream.
///
/// `tracks` is fixed once the demuxer is constructed: every format we target
/// declares its tracks up front (SPS/PPS, a Matroska Tracks element, an MP4
/// `moov`), so there is no "tracks changed mid-stream" case to model.
pub trait Demuxer {
    fn tracks(&self) -> &[Track];

    /// Next packet in the input, or `None` at end of stream.
    fn read_packet(&mut self) -> Result<Option<Packet>, Error>;
}
