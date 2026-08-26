pub mod ebml;
mod matroska;
pub mod mkv;
pub mod mp4;
pub mod webm;

pub use mkv::MkvMuxer;
pub use mp4::Mp4Muxer;
pub use webm::WebmMuxer;

use crate::media::Packet;
use crate::Error;

/// Write compressed packets to a container.
///
/// `WebmMuxer` and `MkvMuxer` share their cluster-writing core
/// (`matroska::MatroskaMuxer`); `Mp4Muxer` writes ISOBMFF instead. `finalize`
/// stays an inherent method per muxer rather than part of this trait: it
/// consumes the muxer to report a final byte count, which is not
/// object-safe.
pub trait Muxer {
    fn write_packet(&mut self, pkt: &Packet) -> Result<(), Error>;
    fn flush(&mut self) -> Result<(), Error>;
}

impl<W: std::io::Write> Muxer for WebmMuxer<W> {
    fn write_packet(&mut self, pkt: &Packet) -> Result<(), Error> {
        WebmMuxer::write_packet(self, pkt)
    }

    fn flush(&mut self) -> Result<(), Error> {
        WebmMuxer::flush(self)
    }
}

impl<W: std::io::Write> Muxer for MkvMuxer<W> {
    fn write_packet(&mut self, pkt: &Packet) -> Result<(), Error> {
        MkvMuxer::write_packet(self, pkt)
    }

    fn flush(&mut self) -> Result<(), Error> {
        MkvMuxer::flush(self)
    }
}

impl<W: std::io::Write> Muxer for Mp4Muxer<W> {
    fn write_packet(&mut self, pkt: &Packet) -> Result<(), Error> {
        Mp4Muxer::write_packet(self, pkt)
    }

    fn flush(&mut self) -> Result<(), Error> {
        Mp4Muxer::flush(self)
    }
}
