//! Format registry: what `(container, codec)` this build can read or write.
//!
//! A queryable mirror of the README format matrix. Update both when a cell
//! flips from planned to shipped — this table is what the future CLI will
//! consult to reject an unsupported combination before doing any work.

use crate::media::Codec;
use crate::probe::Container;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Support {
    pub read: bool,
    pub write: bool,
}

/// What this build can do with `codec` inside `container`.
pub fn support(container: Container, codec: Codec) -> Support {
    match container {
        // Elementary H.264 read only; nothing else lives in an Annex B file.
        Container::AnnexB => Support {
            read: codec == Codec::H264,
            write: false,
        },
        // WebM is a Matroska subset restricted to a handful of codecs, and we
        // only ever produce it, never read it back.
        Container::WebM => Support {
            read: false,
            write: codec.webm_legal(),
        },
        // Full Matroska: any codec with a Matroska CodecID, both directions.
        Container::Matroska => Support {
            read: codec.matroska_id().is_some(),
            write: codec.matroska_id().is_some(),
        },
        // H.264 video only, both directions; see `mux::Mp4Muxer` / `demux::Mp4Demuxer`.
        Container::Mp4 => Support {
            read: codec == Codec::H264,
            write: codec == Codec::H264,
        },
        // H.264 read only; see `demux::TsDemuxer`. TS write is not planned.
        Container::MpegTs => Support {
            read: codec == Codec::H264,
            write: false,
        },
    }
}

pub fn can_read(container: Container, codec: Codec) -> bool {
    support(container, codec).read
}

pub fn can_write(container: Container, codec: Codec) -> bool {
    support(container, codec).write
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annexb_reads_h264_only() {
        assert!(can_read(Container::AnnexB, Codec::H264));
        assert!(!can_read(Container::AnnexB, Codec::Av1));
        assert!(!can_write(Container::AnnexB, Codec::H264));
    }

    #[test]
    fn webm_writes_only_webm_legal_codecs() {
        assert!(can_write(Container::WebM, Codec::Av1));
        assert!(can_write(Container::WebM, Codec::Opus));
        assert!(!can_write(Container::WebM, Codec::H264));
        assert!(!can_read(Container::WebM, Codec::Av1));
    }

    #[test]
    fn matroska_reads_and_writes_any_codec_with_a_codec_id() {
        for codec in [Codec::H264, Codec::Av1, Codec::Aac, Codec::Opus] {
            assert!(can_read(Container::Matroska, codec));
            assert!(can_write(Container::Matroska, codec));
        }
        // G.711 has no plain Matroska CodecID; see `Codec::matroska_id`.
        assert!(!can_read(Container::Matroska, Codec::Pcmu));
    }

    #[test]
    fn mp4_reads_and_writes_h264_only() {
        assert!(can_read(Container::Mp4, Codec::H264));
        assert!(can_write(Container::Mp4, Codec::H264));
        assert!(!can_read(Container::Mp4, Codec::Av1));
        assert!(!can_write(Container::Mp4, Codec::Aac));
    }

    #[test]
    fn mpeg_ts_reads_h264_only() {
        assert!(can_read(Container::MpegTs, Codec::H264));
        assert!(!can_read(Container::MpegTs, Codec::Av1));
        assert!(!can_write(Container::MpegTs, Codec::H264));
    }
}
