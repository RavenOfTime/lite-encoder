//! Container sniffing (ffprobe-shaped).
//!
//! Starts with extension + magic bytes, the same two signals `file(1)` and
//! ffprobe both lean on first. It grows into real parsing (actual track
//! listing) as each container gets a [`crate::demux::Demuxer`] in P2; until
//! then this only answers "what container is this", not "what is in it".

use std::path::Path;

use crate::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// Elementary H.264, Annex B start codes.
    AnnexB,
    WebM,
    Matroska,
    Mp4,
    MpegTs,
}

/// Identify `data`'s container.
///
/// `path` is an optional hint used when magic bytes are ambiguous or `data`
/// is too short to carry them (an empty or truncated capture, for instance).
pub fn probe(data: &[u8], path: Option<&Path>) -> Result<Container, Error> {
    if let Some(c) = sniff_magic(data) {
        return Ok(c);
    }
    if let Some(c) = path.and_then(sniff_extension) {
        return Ok(c);
    }
    Err(Error::Demux("could not identify container".into()))
}

fn sniff_magic(data: &[u8]) -> Option<Container> {
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        // EBML doesn't distinguish WebM from Matroska in its magic; both use
        // the same four bytes. The DocType string a few dozen bytes into the
        // header does, so look for it directly rather than parsing EBML here.
        let window = &data[..data.len().min(4096)];
        return Some(if contains(window, b"webm") {
            Container::WebM
        } else {
            Container::Matroska
        });
    }
    if data.len() >= 8 && &data[4..8] == b"ftyp" {
        return Some(Container::Mp4);
    }
    if is_mpeg_ts(data) {
        return Some(Container::MpegTs);
    }
    if starts_with_annexb_code(data) {
        return Some(Container::AnnexB);
    }
    None
}

/// MPEG-TS packets are a fixed 188 bytes, each starting with sync byte 0x47.
/// One matching byte could be coincidence; four in a row is the format.
fn is_mpeg_ts(data: &[u8]) -> bool {
    const PACKET: usize = 188;
    const CHECK: usize = 4;
    let available = data.len() / PACKET;
    if available == 0 {
        return false;
    }
    (0..available.min(CHECK)).all(|i| data[i * PACKET] == 0x47)
}

fn starts_with_annexb_code(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn sniff_extension(path: &Path) -> Option<Container> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "h264" | "264" | "avc" | "annexb" => Some(Container::AnnexB),
        "webm" => Some(Container::WebM),
        "mkv" => Some(Container::Matroska),
        "mp4" | "m4v" | "mov" => Some(Container::Mp4),
        "ts" | "m2ts" => Some(Container::MpegTs),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_annexb_by_start_code() {
        assert_eq!(probe(&[0, 0, 1, 0x67], None).unwrap(), Container::AnnexB);
        assert_eq!(probe(&[0, 0, 0, 1, 0x67], None).unwrap(), Container::AnnexB);
    }

    #[test]
    fn sniffs_webm_vs_matroska_by_doctype() {
        let mut webm = vec![0x1A, 0x45, 0xDF, 0xA3];
        webm.extend_from_slice(b"junkwebmjunk");
        assert_eq!(probe(&webm, None).unwrap(), Container::WebM);

        let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3];
        mkv.extend_from_slice(b"junkmatroskajunk");
        assert_eq!(probe(&mkv, None).unwrap(), Container::Matroska);
    }

    #[test]
    fn sniffs_mp4_by_ftyp() {
        let mut data = vec![0, 0, 0, 0x20];
        data.extend_from_slice(b"ftypisom");
        assert_eq!(probe(&data, None).unwrap(), Container::Mp4);
    }

    #[test]
    fn sniffs_mpeg_ts_by_repeated_sync_byte() {
        let mut data = vec![0u8; 188 * 4];
        for i in 0..4 {
            data[i * 188] = 0x47;
        }
        assert_eq!(probe(&data, None).unwrap(), Container::MpegTs);
    }

    #[test]
    fn falls_back_to_extension_when_magic_is_ambiguous() {
        let data = [0xffu8; 16];
        assert_eq!(
            probe(&data, Some(Path::new("clip.mkv"))).unwrap(),
            Container::Matroska
        );
    }

    #[test]
    fn errors_when_nothing_matches() {
        assert!(probe(&[0xff; 16], None).is_err());
        assert!(probe(&[0xff; 16], Some(Path::new("clip.unknown"))).is_err());
    }
}
