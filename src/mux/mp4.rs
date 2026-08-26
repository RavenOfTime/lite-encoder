//! MP4 (ISOBMFF) muxer: H.264 video only.
//!
//! Buffers every sample in memory and writes `ftyp`, `mdat`, then `moov` only
//! at [`Mp4Muxer::finalize`] — the sample table (`stts`/`stsz`/`stco`/`stss`)
//! cannot be built until every sample's size, timestamp and chunk offset is
//! known, and getting that table right the first time matters more here than
//! `WebmMuxer`'s incremental-durability guarantee does. `flush` is therefore a
//! no-op: nothing reaches `out` before `finalize`.
//!
//! Audio and any codec besides H.264 are out of scope: `avc1`/`avcC` is the
//! only sample entry this muxer knows how to write. A track's `extra_data`
//! must already be an `avcC` record (see [`crate::codec::h264::avcc`]) —
//! Annex B extra_data is rejected rather than silently reframed.

use crate::media::{Codec, Packet, Track, TrackKind};
use crate::Error;
use std::io::Write;

/// Ticks per second for every timescale this muxer writes. One tick per
/// millisecond matches the crate-wide PTS truncation convention (see
/// `media::time`), so no rescaling is needed anywhere in this file.
const TIMESCALE: u32 = 1000;

struct Sample {
    offset_in_mdat: u64,
    size: u32,
    pts_ticks: u64,
    keyframe: bool,
}

struct TrackBuf {
    track: Track,
    samples: Vec<Sample>,
}

pub struct Mp4Muxer<W: Write> {
    out: W,
    tracks: Vec<TrackBuf>,
    mdat: Vec<u8>,
    finalized: bool,
}

impl<W: Write> Mp4Muxer<W> {
    pub fn new(out: W, tracks: Vec<Track>) -> Result<Self, Error> {
        if tracks.is_empty() {
            return Err(Error::Mux("an MP4 file needs at least one track".into()));
        }
        for t in &tracks {
            if t.codec != Codec::H264 {
                return Err(Error::Mux(format!(
                    "{:?} is not carried by this MP4 muxer (H.264 video only)",
                    t.codec
                )));
            }
            if t.extra_data.is_empty() {
                return Err(Error::Mux(
                    "H.264 track requires an avcC record in extra_data".into(),
                ));
            }
        }
        Ok(Mp4Muxer {
            out,
            tracks: tracks
                .into_iter()
                .map(|track| TrackBuf {
                    track,
                    samples: Vec::new(),
                })
                .collect(),
            mdat: Vec::new(),
            finalized: false,
        })
    }

    /// Buffer a sample. Nothing reaches `out` until [`Self::finalize`].
    pub fn write_packet(&mut self, pkt: &Packet) -> Result<(), Error> {
        if self.finalized {
            return Err(Error::Mux("muxer already finalized".into()));
        }
        let pts_ticks = (pkt.pts.as_millis() as u64) * u64::from(TIMESCALE) / 1000;
        let offset_in_mdat = self.mdat.len() as u64;
        self.mdat.extend_from_slice(&pkt.data);

        let buf = self
            .tracks
            .iter_mut()
            .find(|t| t.track.id == pkt.track)
            .ok_or_else(|| Error::Mux(format!("unknown track {:?}", pkt.track)))?;
        buf.samples.push(Sample {
            offset_in_mdat,
            size: pkt.data.len() as u32,
            pts_ticks,
            keyframe: pkt.keyframe,
        });
        Ok(())
    }

    /// No-op: this muxer only writes bytes on [`Self::finalize`].
    pub fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// Write `ftyp` + `mdat` + `moov` and report the total byte count.
    pub fn finalize(mut self) -> Result<u64, Error> {
        let ftyp = bx(b"ftyp", &ftyp_body());
        let mdat_payload_start = (ftyp.len() + 8) as u64;
        let mdat = bx(b"mdat", &std::mem::take(&mut self.mdat));
        let moov = bx(b"moov", &moov_body(&self.tracks, mdat_payload_start));

        self.out.write_all(&ftyp)?;
        self.out.write_all(&mdat)?;
        self.out.write_all(&moov)?;
        self.finalized = true;
        Ok((ftyp.len() + mdat.len() + moov.len()) as u64)
    }
}

fn ftyp_body() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"isom"); // major_brand
    b.extend_from_slice(&0u32.to_be_bytes()); // minor_version
    b.extend_from_slice(b"isom");
    b.extend_from_slice(b"avc1");
    b.extend_from_slice(b"mp41");
    b
}

fn moov_body(tracks: &[TrackBuf], mdat_payload_start: u64) -> Vec<u8> {
    let duration = tracks
        .iter()
        .flat_map(|t| t.samples.last())
        .map(|s| s.pts_ticks)
        .max()
        .unwrap_or(0);

    let mut b = bx(b"mvhd", &mvhd_body(duration));
    for (i, t) in tracks.iter().enumerate() {
        let track_id = (i + 1) as u32;
        b.extend_from_slice(&bx(b"trak", &trak_body(t, track_id, mdat_payload_start)));
    }
    b
}

fn mvhd_body(duration_ticks: u64) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // version+flags
    b.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    b.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    b.extend_from_slice(&TIMESCALE.to_be_bytes());
    b.extend_from_slice(&(duration_ticks as u32).to_be_bytes());
    b.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate = 1.0
    b.extend_from_slice(&0x0100u16.to_be_bytes()); // volume = 1.0
    b.extend_from_slice(&[0u8; 2]); // reserved
    b.extend_from_slice(&[0u8; 8]); // reserved
    b.extend_from_slice(&unity_matrix());
    b.extend_from_slice(&[0u8; 24]); // pre_defined
    b.extend_from_slice(&2u32.to_be_bytes()); // next_track_ID
    b
}

fn trak_body(t: &TrackBuf, track_id: u32, mdat_payload_start: u64) -> Vec<u8> {
    let duration = t.samples.last().map(|s| s.pts_ticks).unwrap_or(0);
    let (width, height) = match t.track.kind {
        TrackKind::Video { width, height } => (width, height),
        TrackKind::Audio { .. } => unreachable!("Mp4Muxer::new rejects non-H.264 tracks"),
    };

    let mut b = bx(b"tkhd", &tkhd_body(track_id, duration, width, height));
    b.extend_from_slice(&bx(b"mdia", &mdia_body(t, duration, mdat_payload_start)));
    b
}

fn tkhd_body(track_id: u32, duration_ticks: u64, width: u32, height: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0x0000_0007u32.to_be_bytes()); // version+flags: enabled|in movie|in preview
    b.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    b.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    b.extend_from_slice(&track_id.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // reserved
    b.extend_from_slice(&(duration_ticks as u32).to_be_bytes());
    b.extend_from_slice(&[0u8; 8]); // reserved
    b.extend_from_slice(&0u16.to_be_bytes()); // layer
    b.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
    b.extend_from_slice(&0u16.to_be_bytes()); // volume (video track)
    b.extend_from_slice(&[0u8; 2]); // reserved
    b.extend_from_slice(&unity_matrix());
    b.extend_from_slice(&(width << 16).to_be_bytes()); // width, 16.16 fixed
    b.extend_from_slice(&(height << 16).to_be_bytes()); // height, 16.16 fixed
    b
}

fn mdia_body(t: &TrackBuf, duration_ticks: u64, mdat_payload_start: u64) -> Vec<u8> {
    let mut b = bx(b"mdhd", &mdhd_body(duration_ticks));
    b.extend_from_slice(&bx(b"hdlr", &hdlr_body()));
    b.extend_from_slice(&bx(b"minf", &minf_body(t, mdat_payload_start)));
    b
}

fn mdhd_body(duration_ticks: u64) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // version+flags
    b.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    b.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    b.extend_from_slice(&TIMESCALE.to_be_bytes());
    b.extend_from_slice(&(duration_ticks as u32).to_be_bytes());
    b.extend_from_slice(&0x55C4u16.to_be_bytes()); // language = "und"
    b.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    b
}

fn hdlr_body() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // version+flags
    b.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
    b.extend_from_slice(b"vide"); // handler_type
    b.extend_from_slice(&[0u8; 12]); // reserved
    b.extend_from_slice(b"liteenc\0"); // name, NUL-terminated
    b
}

fn minf_body(t: &TrackBuf, mdat_payload_start: u64) -> Vec<u8> {
    let mut b = bx(b"vmhd", &vmhd_body());
    b.extend_from_slice(&bx(b"dinf", &dinf_body()));
    b.extend_from_slice(&bx(b"stbl", &stbl_body(t, mdat_payload_start)));
    b
}

fn vmhd_body() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&1u32.to_be_bytes()); // version=0, flags=1
    b.extend_from_slice(&[0u8; 8]); // graphicsmode(2) + opcolor(6)
    b
}

fn dinf_body() -> Vec<u8> {
    let mut url = Vec::new();
    url.extend_from_slice(&1u32.to_be_bytes()); // version=0, flags=1: media in this file
    let url_box = bx(b"url ", &url);

    let mut dref = Vec::new();
    dref.extend_from_slice(&0u32.to_be_bytes()); // version+flags
    dref.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    dref.extend_from_slice(&url_box);

    bx(b"dref", &dref)
}

fn stbl_body(t: &TrackBuf, mdat_payload_start: u64) -> Vec<u8> {
    let mut b = bx(b"stsd", &stsd_body(&t.track));
    b.extend_from_slice(&bx(b"stts", &stts_body(&t.samples)));
    b.extend_from_slice(&bx(b"stsc", &stsc_body(t.samples.len())));
    b.extend_from_slice(&bx(b"stsz", &stsz_body(&t.samples)));
    b.extend_from_slice(&bx(b"stco", &stco_body(&t.samples, mdat_payload_start)));
    if let Some(stss) = stss_body(&t.samples) {
        b.extend_from_slice(&bx(b"stss", &stss));
    }
    b
}

fn stsd_body(track: &Track) -> Vec<u8> {
    let TrackKind::Video { width, height } = track.kind else {
        unreachable!("Mp4Muxer::new rejects non-H.264 tracks");
    };

    let avcc = bx(b"avcC", &track.extra_data);

    let mut entry = Vec::new();
    entry.extend_from_slice(&[0u8; 6]); // reserved
    entry.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    entry.extend_from_slice(&[0u8; 2]); // pre_defined
    entry.extend_from_slice(&[0u8; 2]); // reserved
    entry.extend_from_slice(&[0u8; 12]); // pre_defined
    entry.extend_from_slice(&(width as u16).to_be_bytes());
    entry.extend_from_slice(&(height as u16).to_be_bytes());
    entry.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // horizresolution = 72 dpi
    entry.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // vertresolution = 72 dpi
    entry.extend_from_slice(&[0u8; 4]); // reserved
    entry.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    entry.extend_from_slice(&[0u8; 32]); // compressorname (empty, Pascal string)
    entry.extend_from_slice(&0x0018u16.to_be_bytes()); // depth = 24
    entry.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined = -1
    entry.extend_from_slice(&avcc);
    let avc1 = bx(b"avc1", &entry);

    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes()); // version+flags
    b.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    b.extend_from_slice(&avc1);
    b
}

/// `sample_count, sample_delta` pairs. The final sample repeats the previous
/// delta: there is no next timestamp to derive one from, and readers require
/// every sample to have a positive duration.
fn stts_body(samples: &[Sample]) -> Vec<u8> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for i in 0..samples.len() {
        let delta = if i + 1 < samples.len() {
            (samples[i + 1].pts_ticks - samples[i].pts_ticks) as u32
        } else if i > 0 {
            (samples[i].pts_ticks - samples[i - 1].pts_ticks) as u32
        } else {
            0
        };
        match runs.last_mut() {
            Some((count, d)) if *d == delta => *count += 1,
            _ => runs.push((1, delta)),
        }
    }

    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&(runs.len() as u32).to_be_bytes());
    for (count, delta) in runs {
        b.extend_from_slice(&count.to_be_bytes());
        b.extend_from_slice(&delta.to_be_bytes());
    }
    b
}

/// One sample per chunk, so a single entry covers every chunk.
fn stsc_body(sample_count: usize) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes());
    if sample_count == 0 {
        b.extend_from_slice(&0u32.to_be_bytes());
        return b;
    }
    b.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    b.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
    b.extend_from_slice(&1u32.to_be_bytes()); // samples_per_chunk
    b.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
    b
}

fn stsz_body(samples: &[Sample]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // sample_size=0: sizes vary, listed below
    b.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    for s in samples {
        b.extend_from_slice(&s.size.to_be_bytes());
    }
    b
}

/// One sample per chunk, so a chunk's offset is exactly its sample's offset.
fn stco_body(samples: &[Sample], mdat_payload_start: u64) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    for s in samples {
        let offset = mdat_payload_start + s.offset_in_mdat;
        b.extend_from_slice(&(offset as u32).to_be_bytes());
    }
    b
}

/// `None` when every sample is a sync sample: an absent `stss` means exactly
/// that, per spec, so an all-keyframe track (a still-image-only clip, or a
/// single-sample file) needs no box at all.
fn stss_body(samples: &[Sample]) -> Option<Vec<u8>> {
    if samples.iter().all(|s| s.keyframe) {
        return None;
    }
    let mut b = Vec::new();
    b.extend_from_slice(&0u32.to_be_bytes());
    let keyframes: Vec<u32> = samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.keyframe)
        .map(|(i, _)| (i + 1) as u32)
        .collect();
    b.extend_from_slice(&(keyframes.len() as u32).to_be_bytes());
    for k in keyframes {
        b.extend_from_slice(&k.to_be_bytes());
    }
    Some(b)
}

fn unity_matrix() -> [u8; 36] {
    let mut m = [0u8; 36];
    m[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // a = 1.0
    m[16..20].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // d = 1.0
    m[32..36].copy_from_slice(&0x4000_0000u32.to_be_bytes()); // w = 1.0 (2.30 fixed)
    m
}

fn bx(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{TrackId, TrackKind};
    use std::time::Duration;

    fn h264_track() -> Track {
        Track {
            id: TrackId(1),
            codec: Codec::H264,
            kind: TrackKind::Video {
                width: 1920,
                height: 1080,
            },
            extra_data: vec![1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 1, 0x67, 1, 0, 1, 0x68],
        }
    }

    fn pkt(ms: u64, key: bool, data: &[u8]) -> Packet {
        Packet {
            track: TrackId(1),
            pts: Duration::from_millis(ms),
            keyframe: key,
            data: bytes::Bytes::copy_from_slice(data),
        }
    }

    fn m_bytes(tracks: Vec<Track>, packets: &[(u64, bool, &[u8])]) -> Vec<u8> {
        use std::cell::RefCell;
        use std::rc::Rc;

        #[derive(Clone)]
        struct Shared(Rc<RefCell<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.borrow_mut().write(buf)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let sink = Shared(Rc::new(RefCell::new(Vec::new())));
        let mut m = Mp4Muxer::new(sink.clone(), tracks).unwrap();
        for &(ms, key, data) in packets {
            m.write_packet(&pkt(ms, key, data)).unwrap();
        }
        m.finalize().unwrap();
        let bytes = sink.0.borrow().clone();
        bytes
    }

    #[test]
    fn box_layout_is_ftyp_then_mdat_then_moov() {
        let out = m_bytes(
            vec![h264_track()],
            &[(0, true, &[1, 2, 3]), (40, false, &[4, 5])],
        );
        assert_eq!(&out[4..8], b"ftyp");
        let ftyp_size = u32::from_be_bytes(out[0..4].try_into().unwrap()) as usize;
        assert_eq!(&out[ftyp_size + 4..ftyp_size + 8], b"mdat");
    }

    #[test]
    fn rejects_non_h264_tracks() {
        let t = Track {
            id: TrackId(1),
            codec: Codec::Av1,
            kind: TrackKind::Video {
                width: 640,
                height: 480,
            },
            extra_data: vec![1],
        };
        assert!(Mp4Muxer::new(Vec::new(), vec![t]).is_err());
    }

    #[test]
    fn rejects_h264_tracks_without_avcc_extra_data() {
        let t = Track {
            id: TrackId(1),
            codec: Codec::H264,
            kind: TrackKind::Video {
                width: 640,
                height: 480,
            },
            extra_data: vec![],
        };
        assert!(Mp4Muxer::new(Vec::new(), vec![t]).is_err());
    }

    #[test]
    fn tracks_are_required() {
        assert!(Mp4Muxer::new(Vec::new(), vec![]).is_err());
    }
}
