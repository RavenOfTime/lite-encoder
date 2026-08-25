//! Timestamp contract for the transcode path.
//!
//! # One timeline end to end
//!
//! Every stage carries presentation time as [`Duration`] on the **job
//! timeline**: monotonic, starting at zero for the recording, *not* the
//! camera's RTP clock. [`crate::source::Timeline`] normalises ingest packets;
//! everything downstream copies that value unchanged.
//!
//! ```text
//! RTSP/RTP  →  Timeline::map  →  Packet.pts  →  Decoder  →  Frame.pts
//!                                                    ↓
//!                                              Encoder  →  Packet.pts
//!                                                    ↓
//!                                           WebmMuxer  →  EBML ticks
//! ```
//!
//! Decoders and encoders must not invent, drop, or reorder timestamps. The
//! H.264 front end takes PTS per access unit; [`crate::codec::av1::Av1Encoder`]
//! tracks rav1e's `input_frameno` so a lagging packet still gets the frame's
//! original PTS. A cache miss or pending entry after `flush` is an encode
//! error, not a silent zero.
//!
//! # WebM time base
//!
//! Matroska timestamps are integer ticks multiplied by `TimestampScale`
//! nanoseconds. We fix the scale at **one millisecond** so cluster timestamps
//! and SimpleBlock offsets stay inside their signed 16-bit relative field for
//! several seconds of media per cluster. Sub-millisecond PTS values are
//! truncated on write; ingest should not rely on finer resolution than 1 ms.
//!
//! rav1e's `time_base` / frame rate is **only** for rate control and keyframe
//! spacing inside the encoder. It does not define mux timestamps.

use std::time::Duration;

use crate::Error;

/// Matroska `TimestampScale` in nanoseconds: one tick is one millisecond.
pub const WEBM_TIMESTAMP_SCALE_NS: u64 = 1_000_000;

/// Maximum span of one WebM cluster. SimpleBlock timestamps are signed 16-bit
/// ticks relative to the cluster timecode, so clusters must be rolled before
/// offsets overflow. Five seconds leaves headroom below ±32 s.
pub const MAX_CLUSTER_SPAN: Duration = Duration::from_secs(5);

/// Convert a job-timeline PTS to WebM/Matroska timestamp ticks.
///
/// Truncates sub-millisecond fractions; see module docs.
pub fn webm_ticks(pts: Duration) -> u64 {
    (pts.as_nanos() / u128::from(WEBM_TIMESTAMP_SCALE_NS)) as u64
}

/// Convert WebM/Matroska timestamp ticks back to a job-timeline PTS.
pub fn from_webm_ticks(ticks: u64) -> Duration {
    Duration::from_nanos(ticks * WEBM_TIMESTAMP_SCALE_NS)
}

/// Signed tick offset from `cluster_base` to `pts` for a SimpleBlock.
pub fn webm_block_offset(pts: Duration, cluster_base: Duration) -> Result<i16, Error> {
    let rel = webm_ticks(pts) as i128 - webm_ticks(cluster_base) as i128;
    i16::try_from(rel).map_err(|_| {
        Error::Mux(format!(
            "packet {pts:?} is {rel} ms from cluster base {cluster_base:?}; \
             SimpleBlock offset does not fit i16"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webm_ticks_round_trip_milliseconds() {
        for ms in [0, 1, 40, 1_000, 86_400_000] {
            let pts = Duration::from_millis(ms);
            assert_eq!(from_webm_ticks(webm_ticks(pts)), pts);
        }
    }

    #[test]
    fn webm_ticks_truncates_sub_millisecond() {
        let pts = Duration::from_nanos(1_500_000);
        assert_eq!(webm_ticks(pts), 1);
        assert_eq!(from_webm_ticks(webm_ticks(pts)), Duration::from_millis(1));
    }

    #[test]
    fn block_offset_fits_inside_cluster_span() {
        let base = Duration::from_secs(10);
        let last = base + MAX_CLUSTER_SPAN - Duration::from_millis(1);
        assert!(webm_block_offset(last, base).is_ok());
    }

    #[test]
    fn block_offset_rejects_overflow() {
        let base = Duration::ZERO;
        let far = base + Duration::from_secs(40);
        assert!(webm_block_offset(far, base).is_err());
    }
}
