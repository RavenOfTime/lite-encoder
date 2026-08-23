//! Job model: what the processor was asked to do, and what it is doing now.

pub mod report;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobId(pub uuid::Uuid);

impl JobId {
    pub fn new() -> Self {
        JobId(uuid::Uuid::new_v4())
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where media comes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Input {
    Rtsp {
        url: String,
        #[serde(default)]
        username: Option<String>,
        #[serde(default)]
        password: Option<String>,
        /// TCP interleaved is the default: UDP loses packets on busy networks
        /// and most cameras handle interleaved fine.
        #[serde(default)]
        transport: RtspTransport,
    },
    File {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RtspTransport {
    #[default]
    Tcp,
    Udp,
}

/// How compressed data reaches the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Treatment {
    /// Copy the camera's bitstream through untouched. Costs almost nothing,
    /// but cannot target WebM, because no camera emits a WebM-legal codec.
    Passthrough,
    /// Decode and re-encode. Required for WebM output.
    Transcode {
        video: crate::media::Codec,
        audio: Option<crate::media::Codec>,
    },
}

/// Roll to a new file on whichever limit is hit first.
///
/// Long recordings are stored as many bounded segments, never one growing
/// file: it keeps seeking cheap, makes retention deletion trivial, and caps
/// how much a crash can cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentPolicy {
    pub max_duration: Option<Duration>,
    pub max_bytes: Option<u64>,
}

impl Default for SegmentPolicy {
    fn default() -> Self {
        SegmentPolicy {
            max_duration: Some(Duration::from_secs(300)),
            max_bytes: Some(512 * 1024 * 1024),
        }
    }
}

/// What a camera drop should do. A recorder that exits when the network
/// blips is not a recorder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// `None` means retry forever, which is the right default for a fixed
    /// camera that is expected to come back.
    pub max_attempts: Option<u32>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            max_attempts: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub input: Input,
    /// Directory that segments are written into.
    pub output_dir: PathBuf,
    pub treatment: Treatment,
    #[serde(default)]
    pub segment: SegmentPolicy,
    #[serde(default)]
    pub retry: RetryPolicy,
    /// Stop after this much media. `None` records until told to stop.
    #[serde(default)]
    pub duration: Option<Duration>,
}

impl JobSpec {
    /// Reject specs that cannot work, before any I/O happens.
    ///
    /// The important one: passthrough into WebM is impossible for every codec
    /// a camera can send, so we refuse it here rather than producing files
    /// that no browser will play.
    pub fn validate(&self) -> Result<(), crate::Error> {
        if let Treatment::Transcode { video, audio } = self.treatment {
            if !video.webm_legal() {
                return Err(crate::Error::Spec(format!(
                    "{video:?} is not a WebM-legal video codec; use Av1, Vp9 or Vp8"
                )));
            }
            if let Some(a) = audio {
                if !a.webm_legal() {
                    return Err(crate::Error::Spec(format!(
                        "{a:?} is not a WebM-legal audio codec; use Opus or Vorbis"
                    )));
                }
            }
        }
        if let Some(d) = self.segment.max_duration {
            if d.is_zero() {
                return Err(crate::Error::Spec(
                    "segment max_duration must be non-zero".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Pending,
    Connecting,
    Running,
    /// Lost the source, waiting to retry. Still an active job.
    Reconnecting,
    Stopping,
    Completed,
    Failed,
}

impl JobState {
    pub fn terminal(self) -> bool {
        matches!(self, JobState::Completed | JobState::Failed)
    }
}
