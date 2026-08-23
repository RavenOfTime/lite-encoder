//! Minimal EBML writer: just the primitives Matroska/WebM needs.

/// EBML element IDs, stored with their length-descriptor bits already set,
/// so they are written verbatim.
pub mod id {
    pub const EBML: u32 = 0x1A45_DFA3;
    pub const EBML_VERSION: u32 = 0x4286;
    pub const EBML_READ_VERSION: u32 = 0x42F7;
    pub const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
    pub const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
    pub const DOC_TYPE: u32 = 0x4282;
    pub const DOC_TYPE_VERSION: u32 = 0x4287;
    pub const DOC_TYPE_READ_VERSION: u32 = 0x4285;

    pub const SEGMENT: u32 = 0x1853_8067;
    pub const INFO: u32 = 0x1549_A966;
    pub const TIMESTAMP_SCALE: u32 = 0x002A_D7B1;
    pub const MUXING_APP: u32 = 0x4D80;
    pub const WRITING_APP: u32 = 0x5741;
    pub const DURATION: u32 = 0x4489;

    pub const TRACKS: u32 = 0x1654_AE6B;
    pub const TRACK_ENTRY: u32 = 0xAE;
    pub const TRACK_NUMBER: u32 = 0xD7;
    pub const TRACK_UID: u32 = 0x73C5;
    pub const TRACK_TYPE: u32 = 0x83;
    pub const FLAG_LACING: u32 = 0x9C;
    pub const CODEC_ID: u32 = 0x86;
    pub const CODEC_PRIVATE: u32 = 0x63A2;
    pub const DEFAULT_DURATION: u32 = 0x0233_83E3;

    pub const VIDEO: u32 = 0xE0;
    pub const PIXEL_WIDTH: u32 = 0xB0;
    pub const PIXEL_HEIGHT: u32 = 0xBA;

    pub const AUDIO: u32 = 0xE1;
    pub const SAMPLING_FREQUENCY: u32 = 0xB5;
    pub const CHANNELS: u32 = 0x9F;

    pub const CLUSTER: u32 = 0x1F43_B675;
    pub const TIMESTAMP: u32 = 0xE7;
    pub const SIMPLE_BLOCK: u32 = 0xA3;

    pub const CUES: u32 = 0x1C53_BB6B;
    pub const CUE_POINT: u32 = 0xBB;
    pub const CUE_TIME: u32 = 0xB3;
    pub const CUE_TRACK_POSITIONS: u32 = 0xB7;
    pub const CUE_TRACK: u32 = 0xF7;
    pub const CUE_CLUSTER_POSITION: u32 = 0xF1;
}

/// An 8-byte "unknown size" VINT.
///
/// Used for the Segment element while recording: the size is not known until
/// the file is closed, and a reader that hits a truncated file can still play
/// everything up to the last complete cluster.
pub const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

/// Write an element ID, stripping the leading zero bytes it is stored with.
pub fn write_id(out: &mut Vec<u8>, id: u32) {
    let bytes = id.to_be_bytes();
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(3);
    out.extend_from_slice(&bytes[start..]);
}

/// Write a size as a variable-length integer.
///
/// Each length has one value reserved as "unknown" (all data bits set), so
/// the usable range for `n` bytes is `2^(7n) - 1`.
pub fn write_size(out: &mut Vec<u8>, size: u64) {
    let mut len = 1usize;
    while len <= 8 && size >= (1u64 << (7 * len)) - 1 {
        len += 1;
    }
    debug_assert!(len <= 8, "size {size} too large for EBML VINT");
    let marker = 1u64 << (7 * len);
    let v = size | marker;
    let bytes = v.to_be_bytes();
    out.extend_from_slice(&bytes[8 - len..]);
}

/// Element with a byte-string payload.
pub fn write_bytes(out: &mut Vec<u8>, id: u32, data: &[u8]) {
    write_id(out, id);
    write_size(out, data.len() as u64);
    out.extend_from_slice(data);
}

/// Element with an unsigned-integer payload, minimally encoded.
pub fn write_uint(out: &mut Vec<u8>, id: u32, value: u64) {
    let bytes = value.to_be_bytes();
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(7);
    write_bytes(out, id, &bytes[start..]);
}

/// Element with a 64-bit float payload.
pub fn write_float(out: &mut Vec<u8>, id: u32, value: f64) {
    write_bytes(out, id, &value.to_be_bytes());
}

pub fn write_string(out: &mut Vec<u8>, id: u32, value: &str) {
    write_bytes(out, id, value.as_bytes());
}

/// Build a master element from `body` and append it with a known size.
pub fn write_master(out: &mut Vec<u8>, id: u32, body: &[u8]) {
    write_bytes(out, id, body);
}

/// A Matroska SimpleBlock.
///
/// `rel_ts` is signed 16-bit and relative to the enclosing cluster's
/// timestamp, which is what bounds how long a cluster may be.
pub fn write_simple_block(out: &mut Vec<u8>, track: u64, rel_ts: i16, keyframe: bool, data: &[u8]) {
    let mut body = Vec::with_capacity(data.len() + 8);
    write_size(&mut body, track);
    body.extend_from_slice(&rel_ts.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.extend_from_slice(data);
    write_bytes(out, id::SIMPLE_BLOCK, &body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_sizes_use_minimal_length() {
        let mut b = Vec::new();
        write_size(&mut b, 0);
        assert_eq!(b, vec![0x80]);

        b.clear();
        write_size(&mut b, 1);
        assert_eq!(b, vec![0x81]);

        // 127 is the reserved "unknown" value for 1 byte, so it must roll
        // over to a 2-byte encoding rather than emitting 0xFF.
        b.clear();
        write_size(&mut b, 127);
        assert_eq!(b, vec![0x40, 0x7F]);

        b.clear();
        write_size(&mut b, 126);
        assert_eq!(b, vec![0xFE]);
    }

    #[test]
    fn ids_drop_leading_zeroes() {
        let mut b = Vec::new();
        write_id(&mut b, id::EBML);
        assert_eq!(b, vec![0x1A, 0x45, 0xDF, 0xA3]);

        b.clear();
        write_id(&mut b, id::TRACK_ENTRY);
        assert_eq!(b, vec![0xAE]);

        b.clear();
        write_id(&mut b, id::TIMESTAMP_SCALE);
        assert_eq!(b, vec![0x2A, 0xD7, 0xB1]);
    }

    #[test]
    fn uints_are_minimally_encoded() {
        let mut b = Vec::new();
        write_uint(&mut b, id::TRACK_NUMBER, 1);
        assert_eq!(b, vec![0xD7, 0x81, 0x01]);

        b.clear();
        write_uint(&mut b, id::TIMESTAMP_SCALE, 1_000_000);
        assert_eq!(b, vec![0x2A, 0xD7, 0xB1, 0x83, 0x0F, 0x42, 0x40]);
    }

    #[test]
    fn zero_encodes_as_one_byte() {
        let mut b = Vec::new();
        write_uint(&mut b, id::TIMESTAMP, 0);
        assert_eq!(b, vec![0xE7, 0x81, 0x00]);
    }

    #[test]
    fn simple_block_carries_keyframe_flag() {
        let mut b = Vec::new();
        write_simple_block(&mut b, 1, 250, true, &[0xAA, 0xBB]);
        // id, size=6, track vint, ts hi/lo, flags, payload
        assert_eq!(b, vec![0xA3, 0x86, 0x81, 0x00, 0xFA, 0x80, 0xAA, 0xBB]);

        b.clear();
        write_simple_block(&mut b, 1, -2, false, &[0xAA]);
        assert_eq!(b, vec![0xA3, 0x85, 0x81, 0xFF, 0xFE, 0x00, 0xAA]);
    }
}
