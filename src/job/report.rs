//! Progress reporting.
//!
//! The reason this project exists rather than shelling out to a CLI: a
//! long-running recording has to be *observable*. Every event below is
//! something an operator or supervising service needs to see without
//! scraping stderr.

use super::{JobId, JobState};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    StateChanged {
        from: JobState,
        to: JobState,
    },
    /// Source connected and tracks are known.
    Connected {
        tracks: Vec<TrackSummary>,
    },
    /// A segment file was opened.
    SegmentStarted {
        index: u64,
        path: PathBuf,
    },
    /// A segment was closed and fully flushed. This is the durability
    /// signal: everything up to `media_duration` is safely on disk.
    SegmentFinished {
        index: u64,
        path: PathBuf,
        bytes: u64,
        media_duration: Duration,
    },
    /// Periodic heartbeat while running.
    Progress(Progress),
    /// Source dropped; we intend to retry.
    SourceLost {
        error: String,
        attempt: u32,
        retry_in: Duration,
    },
    /// Input timestamps jumped. Cameras do this on NTP correction and it
    /// silently corrupts recordings that trust the wire clock.
    TimestampDiscontinuity {
        expected: Duration,
        got: Duration,
    },
    /// A decoded picture had macroblocks concealed with mid-grey because no
    /// slice claimed them. The frame is still emitted so recording can
    /// continue, but operators need to know the picture is damaged.
    ConcealedPicture {
        pts: Duration,
        concealed_macroblocks: u32,
    },
    /// Encoder or writer could not keep up and media was dropped. Never
    /// hide this: a recording with silent gaps is worse than a failed one.
    Dropped {
        packets: u64,
        reason: String,
    },
    Failed {
        error: String,
    },
    Completed {
        segments: u64,
        media_duration: Duration,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackSummary {
    pub id: u32,
    pub codec: String,
    pub detail: String,
}

/// A point-in-time snapshot of a running job.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    /// Media timeline position, i.e. how much footage has been recorded.
    pub media_duration: Duration,
    /// Wallclock time since the job started, including reconnect gaps.
    pub elapsed: Duration,
    pub bytes_written: u64,
    pub packets: u64,
    pub dropped_packets: u64,
    pub segments: u64,
    /// Encoded media seconds per wallclock second. Below 1.0 on a live
    /// source means falling behind and eventually dropping.
    pub speed: f32,
}

/// A timestamped event bound to its job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEvent {
    pub job: JobId,
    /// Unix milliseconds.
    pub at_ms: u64,
    pub event: Event,
}

/// Where events go.
///
/// Kept as a trait so the core library has no opinion on transport; a gRPC
/// or HTTP surface is a `Reporter` implementation, not a rewrite.
pub trait Reporter: Send + Sync {
    fn report(&self, ev: JobEvent);
}

/// Writes newline-delimited JSON. Useful on its own and as the reference
/// implementation for any richer transport.
pub struct JsonReporter<W>(pub std::sync::Mutex<W>);

impl<W: std::io::Write + Send> Reporter for JsonReporter<W> {
    fn report(&self, ev: JobEvent) {
        if let Ok(mut w) = self.0.lock() {
            if serde_json::to_writer(&mut *w, &ev).is_ok() {
                let _ = writeln!(&mut *w);
                let _ = w.flush();
            }
        }
    }
}

/// Drops everything. For tests and embedded use.
pub struct NullReporter;

impl Reporter for NullReporter {
    fn report(&self, _ev: JobEvent) {}
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
