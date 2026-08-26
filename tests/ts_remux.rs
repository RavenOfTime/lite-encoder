//! MPEG-TS read: packetize the camera fixture into a synthetic single-program
//! transport stream, demux it back, and prove `-c copy` can carry it into MKV.
//!
//! There is no `TsMuxer` (TS write is out of scope, see `todo.md`), so unlike
//! `tests/mkv_remux.rs`/`tests/mp4_remux.rs` this test cannot round-trip
//! through the crate's own muxer. Instead it packetizes the fixture into TS
//! bytes itself, following ISO/IEC 13818-1 closely enough for `TsDemuxer` to
//! read it back: PAT -> PMT -> one PES per access unit, split across 188-byte
//! packets.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lite_encoder::codec::h264::annexb::access_units;
use lite_encoder::demux::{AnnexBDemuxer, Demuxer, TsDemuxer};
use lite_encoder::media::{Codec, TrackKind};
use lite_encoder::mux::MkvMuxer;
use lite_encoder::remux::copy_remux;

const FIXTURE: &[u8] = include_bytes!("fixtures/tapo-1080p-cabac-8x8.h264");
const FRAME_RATE: u32 = 22;
const PAT_PID: u16 = 0x0000;
const PMT_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x0100;
const H264_STREAM_TYPE: u8 = 0x1b;

/// A short final chunk is padded with an adaptation-field stuffing field
/// (like a real encoder would), not trailing payload zero bytes, so
/// reassembled PES/PSI bytes come back byte-exact.
fn ts_packet(pid: u16, pusi: bool, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= 184);
    let mut p = vec![0u8; 188];
    p[0] = 0x47;
    p[1] = (if pusi { 0x40 } else { 0x00 }) | ((pid >> 8) as u8 & 0x1f);
    p[2] = pid as u8;
    if payload.len() == 184 {
        p[3] = 0x10; // payload only
        p[4..].copy_from_slice(payload);
    } else {
        p[3] = 0x30; // adaptation field + payload
        let af_len = 183 - payload.len();
        p[4] = af_len as u8;
        p[5 + af_len..].copy_from_slice(payload);
    }
    p
}

fn packetize(pid: u16, payload: &[u8], out: &mut Vec<u8>) {
    let mut chunks = payload.chunks(184);
    if let Some(first) = chunks.next() {
        out.extend(ts_packet(pid, true, first));
    }
    for chunk in chunks {
        out.extend(ts_packet(pid, false, chunk));
    }
}

/// Prepend a zero `pointer_field`, turning a bare PSI section into a packet
/// payload that starts a new section.
fn psi_payload(section: &[u8]) -> Vec<u8> {
    let mut p = vec![0x00];
    p.extend_from_slice(section);
    p
}

fn pat_section() -> Vec<u8> {
    vec![
        0x00,
        0xb0,
        0x0d,
        0x00,
        0x01,
        0xc1,
        0x00,
        0x00, // table header
        0x00,
        0x01, // program_number = 1
        0xe0 | (PMT_PID >> 8) as u8,
        PMT_PID as u8,
        0,
        0,
        0,
        0, // CRC32, unchecked by TsDemuxer
    ]
}

fn pmt_section() -> Vec<u8> {
    vec![
        0x02,
        0xb0,
        0x12,
        0x00,
        0x01,
        0xc1,
        0x00,
        0x00, // table header
        0xe0 | (VIDEO_PID >> 8) as u8,
        VIDEO_PID as u8, // PCR_PID (reused as the video PID)
        0xf0,
        0x00, // program_info_length = 0
        H264_STREAM_TYPE,
        0xe0 | (VIDEO_PID >> 8) as u8,
        VIDEO_PID as u8,
        0xf0,
        0x00, // ES_info_length = 0
        0,
        0,
        0,
        0, // CRC32, unchecked by TsDemuxer
    ]
}

/// 5-byte PES PTS field per ISO/IEC 13818-1 2.4.3.6, PTS-only ('0010' prefix).
fn pts_field(ticks: u64) -> [u8; 5] {
    [
        0x20 | (((ticks >> 30) as u8 & 0x07) << 1) | 1,
        (ticks >> 22) as u8,
        (((ticks >> 15) as u8 & 0x7f) << 1) | 1,
        (ticks >> 7) as u8,
        ((ticks as u8 & 0x7f) << 1) | 1,
    ]
}

fn pes_packet(pts_ticks: u64, es: &[u8]) -> Vec<u8> {
    let mut pes = vec![0x00, 0x00, 0x01, 0xe0, 0x00, 0x00, 0x80, 0x80, 0x05];
    pes.extend_from_slice(&pts_field(pts_ticks));
    pes.extend_from_slice(es);
    pes
}

/// Packetize `access_units` into a single-program TS: PAT, PMT, then one PES
/// per access unit. PTS mirrors `AnnexBDemuxer`'s synthesized
/// `index * 1000 / frame_rate` milliseconds, in the PES's 90 kHz clock.
fn build_ts(access_units: &[&[u8]], frame_rate: u32) -> Vec<u8> {
    let mut out = Vec::new();
    packetize(PAT_PID, &psi_payload(&pat_section()), &mut out);
    packetize(PMT_PID, &psi_payload(&pmt_section()), &mut out);
    for (index, au) in access_units.iter().enumerate() {
        let ms = index as u64 * 1000 / frame_rate as u64;
        packetize(VIDEO_PID, &pes_packet(ms * 90, au), &mut out);
    }
    out
}

/// `MkvMuxer::finalize` only reports a byte count; a shared buffer lets this
/// test read the bytes anyway. Same helper as `tests/mkv_remux.rs`.
#[derive(Clone)]
struct SharedBuf(Rc<RefCell<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

#[test]
fn ts_demuxer_recovers_every_access_unit_with_correct_timing() {
    let aus = access_units(FIXTURE);
    let ts_bytes = build_ts(&aus, FRAME_RATE);

    let mut from_ts = TsDemuxer::new(&ts_bytes).unwrap();
    assert_eq!(from_ts.tracks().len(), 1);
    assert_eq!(from_ts.tracks()[0].codec, Codec::H264);
    assert!(matches!(
        from_ts.tracks()[0].kind,
        TrackKind::Video {
            width: 1920,
            height: 1080
        }
    ));
    assert!(!from_ts.tracks()[0].extra_data.is_empty());

    let mut from_annexb = AnnexBDemuxer::new(FIXTURE, FRAME_RATE).unwrap();
    let mut count = 0;
    while let Some(expected) = from_annexb.read_packet().unwrap() {
        let got = from_ts
            .read_packet()
            .unwrap()
            .expect("TsDemuxer must not lose access units");
        assert_eq!(got.pts, expected.pts);
        assert_eq!(got.keyframe, expected.keyframe);
        assert_eq!(&got.data[..], &expected.data[..]);
        count += 1;
    }
    assert_eq!(count, 4, "fixture has 4 access units");
    assert!(from_ts.read_packet().unwrap().is_none());
}

#[test]
fn ts_source_copy_remuxes_into_mkv() {
    let aus = access_units(FIXTURE);
    let ts_bytes = build_ts(&aus, FRAME_RATE);

    let mut source = TsDemuxer::new(&ts_bytes).unwrap();
    let tracks = source.tracks().to_vec();

    let buf = SharedBuf(Rc::new(RefCell::new(Vec::new())));
    let mut mux = MkvMuxer::new(buf.clone(), tracks).unwrap();
    let copied = copy_remux(&mut source, &mut mux).unwrap();
    assert_eq!(copied, 4);
    mux.finalize().unwrap();

    let out = buf.0.borrow().clone();
    let mut roundtrip = lite_encoder::demux::MkvDemuxer::new(&out).unwrap();
    let mut original = AnnexBDemuxer::new(FIXTURE, FRAME_RATE).unwrap();
    let mut count = 0;
    while let Some(orig_pkt) = original.read_packet().unwrap() {
        let rt_pkt = roundtrip.read_packet().unwrap().unwrap();
        assert_eq!(rt_pkt.pts, orig_pkt.pts);
        assert_eq!(rt_pkt.data, orig_pkt.data);
        count += 1;
    }
    assert_eq!(count, 4);
}
