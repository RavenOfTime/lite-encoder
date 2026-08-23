//! Media primitives shared by sources, codecs and muxers.
//!
//! Deliberately small. We support few formats on purpose, so these types
//! describe what we actually carry rather than a universal media model.

use std::time::Duration;

/// Codecs we can carry. Narrow by design.
///
/// `Webm*` variants are the only ones a WebM muxer will accept; everything
/// else is ingest-side and must be transcoded before it can reach a WebM file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Codec {
    // Ingest-side video (what cameras actually send).
    H264,
    H265,
    // WebM-legal video.
    Av1,
    Vp9,
    Vp8,
    // Ingest-side audio.
    Aac,
    /// G.711 mu-law.
    Pcmu,
    /// G.711 A-law.
    Pcma,
    // WebM-legal audio.
    Opus,
    Vorbis,
}

impl Codec {
    /// Whether a WebM (Matroska subset) file may legally contain this codec.
    ///
    /// This is the constraint that forces transcoding on the RTSP path: no
    /// camera emits a WebM-legal video codec today.
    pub fn webm_legal(self) -> bool {
        matches!(
            self,
            Codec::Av1 | Codec::Vp9 | Codec::Vp8 | Codec::Opus | Codec::Vorbis
        )
    }

    pub fn is_video(self) -> bool {
        matches!(
            self,
            Codec::H264 | Codec::H265 | Codec::Av1 | Codec::Vp9 | Codec::Vp8
        )
    }

    /// The Matroska `CodecID` string, for codecs we can mux.
    pub fn matroska_id(self) -> Option<&'static str> {
        Some(match self {
            Codec::Av1 => "V_AV1",
            Codec::Vp9 => "V_VP9",
            Codec::Vp8 => "V_VP8",
            Codec::Opus => "A_OPUS",
            Codec::Vorbis => "A_VORBIS",
            // Legal in Matroska but *not* in WebM; we don't emit these.
            _ => return None,
        })
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct TrackId(pub u32);

#[derive(Debug, Clone)]
pub enum TrackKind {
    Video { width: u32, height: u32 },
    Audio { sample_rate: u32, channels: u8 },
}

#[derive(Debug, Clone)]
pub struct Track {
    pub id: TrackId,
    pub codec: Codec,
    pub kind: TrackKind,
    /// Codec-private setup data (AV1 sequence header, OpusHead, SPS/PPS...).
    pub extra_data: Vec<u8>,
}

/// One compressed access unit.
///
/// `pts` is normalised to a monotonic timeline that starts at zero for the
/// job, *not* the camera's clock. See `source::Timeline` for why that matters.
#[derive(Debug, Clone)]
pub struct Packet {
    pub track: TrackId,
    pub pts: Duration,
    pub keyframe: bool,
    pub data: bytes::Bytes,
}

/// A decoded picture. Only what the encoders we target actually consume.
#[derive(Debug, Clone)]
pub struct Frame {
    pub pts: Duration,
    pub width: u32,
    pub height: u32,
    /// Planar YUV 4:2:0, 8-bit: Y, U, V.
    pub planes: [Vec<u8>; 3],
    pub strides: [usize; 3],
}

/// Decode compressed packets to frames.
///
/// This trait is the seam that keeps the pure-Rust goal from becoming a
/// single-vendor bet: an unproven pure-Rust H.264 decoder and an openh264
/// FFI reference implementation are interchangeable behind it, so they can be
/// diffed against each other on real camera streams.
pub trait Decoder: Send {
    fn decode(&mut self, pkt: &Packet) -> Result<Vec<Frame>, crate::Error>;
    /// Flush buffered pictures at end of stream.
    fn flush(&mut self) -> Result<Vec<Frame>, crate::Error>;
}

/// Encode frames to WebM-legal compressed packets.
pub trait Encoder: Send {
    fn codec(&self) -> Codec;
    /// Codec-private data for the muxer's track header.
    fn extra_data(&self) -> Vec<u8>;
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>, crate::Error>;
    fn flush(&mut self) -> Result<Vec<Packet>, crate::Error>;
}
