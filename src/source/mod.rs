pub mod rtsp;
pub mod timeline;

pub use rtsp::RtspSource;
pub use timeline::{Timeline, TimelineEvent};

use crate::media::{Packet, Track};

/// A source of compressed packets.
///
/// Sources yield packets on a timeline that starts at zero for the job, not
/// on the camera's clock; normalisation is the source's responsibility so
/// that muxers never have to reason about wire timestamps.
#[allow(async_fn_in_trait)]
pub trait Source {
    /// Tracks discovered during setup.
    fn tracks(&self) -> &[Track];

    /// Next packet, or `None` at end of stream.
    async fn next_packet(&mut self) -> crate::Result<Option<Packet>>;
}
