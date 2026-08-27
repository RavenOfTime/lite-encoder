//! Media primitives shared by sources, codecs and muxers.
//!
//! Deliberately small. We support few formats on purpose, so these types
//! describe what we actually carry rather than a universal media model.

pub mod time;

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

    /// Every codec this crate has a name for, in matrix order.
    ///
    /// Exists so `liteenc formats` and the registry tests can enumerate the
    /// support matrix instead of restating it.
    pub const ALL: &'static [Codec] = &[
        Codec::H264,
        Codec::H265,
        Codec::Av1,
        Codec::Vp9,
        Codec::Vp8,
        Codec::Aac,
        Codec::Pcmu,
        Codec::Pcma,
        Codec::Opus,
        Codec::Vorbis,
    ];

    /// The CLI's name for this codec. ffmpeg's spelling wherever one exists,
    /// so `-c:v h264` and `-c:a opus` mean what a user already expects.
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "hevc",
            Codec::Av1 => "av1",
            Codec::Vp9 => "vp9",
            Codec::Vp8 => "vp8",
            Codec::Aac => "aac",
            Codec::Pcmu => "pcm_mulaw",
            Codec::Pcma => "pcm_alaw",
            Codec::Opus => "opus",
            Codec::Vorbis => "vorbis",
        }
    }

    /// Parse a CLI codec name, accepting the common aliases too.
    pub fn from_name(name: &str) -> Option<Codec> {
        let lower = name.to_ascii_lowercase();
        if let Some(c) = Codec::ALL.iter().find(|c| c.name() == lower) {
            return Some(*c);
        }
        Some(match lower.as_str() {
            "avc" | "avc1" | "x264" => Codec::H264,
            "h265" | "hvc1" => Codec::H265,
            "libaom-av1" | "librav1e" => Codec::Av1,
            "libopus" => Codec::Opus,
            "libvorbis" => Codec::Vorbis,
            "mulaw" | "g711u" => Codec::Pcmu,
            "alaw" | "g711a" => Codec::Pcma,
            _ => return None,
        })
    }

    /// The Matroska `CodecID` string, for codecs we can mux into Matroska.
    ///
    /// A strict superset of what WebM accepts: [`Codec::webm_legal`] narrows
    /// this further for [`crate::mux::WebmMuxer`].
    pub fn matroska_id(self) -> Option<&'static str> {
        Some(match self {
            Codec::H264 => "V_MPEG4/ISO/AVC",
            Codec::H265 => "V_MPEGH/ISO/HEVC",
            Codec::Av1 => "V_AV1",
            Codec::Vp9 => "V_VP9",
            Codec::Vp8 => "V_VP8",
            Codec::Aac => "A_AAC",
            Codec::Opus => "A_OPUS",
            Codec::Vorbis => "A_VORBIS",
            // No plain Matroska CodecID for G.711; it rides inside an ACM
            // wrapper we do not build. Out of scope until something needs it.
            Codec::Pcmu | Codec::Pcma => return None,
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
/// `pts` is on the job timeline in nanoseconds, normalised by
/// [`crate::source::Timeline`] at ingest. See [`time`] for how it maps into
/// WebM. Encoders must copy the originating frame's PTS onto every packet they
/// emit for that frame.
#[derive(Debug, Clone)]
pub struct Packet {
    pub track: TrackId,
    pub pts: Duration,
    pub keyframe: bool,
    pub data: bytes::Bytes,
}

/// A decoded picture. Only what the encoders we target actually consume.
///
/// `pts` is copied from the compressed packet that produced this picture and
/// must be passed unchanged to the encoder. See [`time`].
#[derive(Debug, Clone)]
pub struct Frame {
    pub pts: Duration,
    pub width: u32,
    pub height: u32,
    /// Planar YUV 4:2:0, 8-bit: Y, U, V.
    pub planes: [Vec<u8>; 3],
    pub strides: [usize; 3],
    /// Macroblocks no slice claimed before concealment. Zero for a clean
    /// picture; non-zero means mid-grey was painted into holes (packet loss,
    /// incomplete slice coverage, etc.).
    pub concealed_macroblocks: u32,
}

/// Decode compressed packets to frames.
///
/// The shipping implementation is the pure-Rust decoder. The trait also lets
/// tests plug in OpenH264 as a validation oracle so both can be driven over
/// the same stream and compared sample-by-sample — that is a test concern,
/// not an alternate production backend.
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
    /// Encode this frame as a random-access point. Segment rollover uses this
    /// so the next WebM cluster and segment can begin independently playable.
    fn encode_keyframe(&mut self, frame: &Frame) -> Result<Vec<Packet>, crate::Error> {
        let _ = frame;
        Err(crate::Error::Encode(
            "this encoder cannot force keyframes".into(),
        ))
    }
    fn flush(&mut self) -> Result<Vec<Packet>, crate::Error>;
}
