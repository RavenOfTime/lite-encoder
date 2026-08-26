//! MP4 (ISOBMFF) demux: H.264 video only, mirroring [`super::super::mux::Mp4Muxer`].
//!
//! Samples are addressed by absolute file offset (`stco`), so `mdat` never
//! needs to be located explicitly — every sample slices straight out of the
//! file regardless of which box notionally contains it. `co64` (64-bit chunk
//! offsets, for files over 4 GiB) and `ctts` (composition offsets, i.e.
//! B-frames) are rejected outright rather than silently mishandled: this
//! decoder doesn't support B-frames either, so a `ctts` box is a real signal
//! the file needs reordering we cannot do.

use crate::media::{Codec, Packet, Track, TrackId, TrackKind};
use crate::Error;

use super::Demuxer;

pub struct Mp4Demuxer {
    tracks: Vec<Track>,
    packets: std::vec::IntoIter<Packet>,
}

impl Mp4Demuxer {
    pub fn new(data: &[u8]) -> Result<Self, Error> {
        let (_, moov_body, moov_end) = find_box(data, 0, data.len(), b"moov")
            .ok_or_else(|| Error::Demux("no moov box".into()))?;

        let mut tracks = Vec::new();
        let mut packets = Vec::new();
        for (_, trak_body, trak_end) in all_boxes(data, moov_body, moov_end, b"trak") {
            let (track, track_packets) = read_trak(data, trak_body, trak_end)?;
            tracks.push(track);
            packets.extend(track_packets);
        }
        if tracks.is_empty() {
            return Err(Error::Demux("moov has no trak".into()));
        }
        packets.sort_by_key(|p: &Packet| p.pts);

        Ok(Mp4Demuxer {
            tracks,
            packets: packets.into_iter(),
        })
    }
}

impl Demuxer for Mp4Demuxer {
    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn read_packet(&mut self) -> Result<Option<Packet>, Error> {
        Ok(self.packets.next())
    }
}

fn read_trak(
    data: &[u8],
    trak_body: usize,
    trak_end: usize,
) -> Result<(Track, Vec<Packet>), Error> {
    let (_, tkhd_body, _) = find_box(data, trak_body, trak_end, b"tkhd")
        .ok_or_else(|| Error::Demux("trak missing tkhd".into()))?;
    let track_id = u32_at(data, tkhd_body + 12)?;

    let (_, mdia_body, mdia_end) = find_box(data, trak_body, trak_end, b"mdia")
        .ok_or_else(|| Error::Demux("trak missing mdia".into()))?;

    let (_, hdlr_body, _) = find_box(data, mdia_body, mdia_end, b"hdlr")
        .ok_or_else(|| Error::Demux("mdia missing hdlr".into()))?;
    let handler = &data[hdlr_body + 8..hdlr_body + 12];
    if handler != b"vide" {
        return Err(Error::Demux(format!(
            "handler {:?} is not supported (video only)",
            std::str::from_utf8(handler).unwrap_or("?")
        )));
    }

    let (_, mdhd_body, _) = find_box(data, mdia_body, mdia_end, b"mdhd")
        .ok_or_else(|| Error::Demux("mdia missing mdhd".into()))?;
    let timescale = u32_at(data, mdhd_body + 12)? as u64;
    if timescale == 0 {
        return Err(Error::Demux("mdhd timescale is zero".into()));
    }

    let (_, minf_body, minf_end) = find_box(data, mdia_body, mdia_end, b"minf")
        .ok_or_else(|| Error::Demux("mdia missing minf".into()))?;
    let (_, stbl_body, stbl_end) = find_box(data, minf_body, minf_end, b"stbl")
        .ok_or_else(|| Error::Demux("minf missing stbl".into()))?;

    if find_box(data, stbl_body, stbl_end, b"ctts").is_some() {
        return Err(Error::Demux(
            "composition time offsets (B-frames) are not supported".into(),
        ));
    }
    if find_box(data, stbl_body, stbl_end, b"co64").is_some() {
        return Err(Error::Demux(
            "64-bit chunk offsets (co64) are not supported".into(),
        ));
    }

    let (codec, kind, extra_data) = read_stsd(data, stbl_body, stbl_end)?;
    let deltas = read_stts(data, stbl_body, stbl_end)?;
    let sizes = read_stsz(data, stbl_body, stbl_end)?;
    if deltas.len() != sizes.len() {
        return Err(Error::Demux(format!(
            "stts covers {} samples, stsz declares {}",
            deltas.len(),
            sizes.len()
        )));
    }
    let offsets = read_sample_offsets(data, stbl_body, stbl_end, sizes.len())?;
    let sync = read_stss(data, stbl_body, stbl_end, sizes.len())?;

    let mut ticks = 0u64;
    let mut packets = Vec::with_capacity(sizes.len());
    for i in 0..sizes.len() {
        let pts = duration_from_ticks(ticks, timescale)?;
        let (offset, size) = (offsets[i], sizes[i]);
        let end = offset
            .checked_add(size as usize)
            .ok_or_else(|| Error::Demux("sample offset+size overflows".into()))?;
        let bytes = data
            .get(offset..end)
            .ok_or_else(|| Error::Demux(format!("sample {i} at 0x{offset:x} overruns the file")))?;
        packets.push(Packet {
            track: TrackId(track_id),
            pts,
            keyframe: sync[i],
            data: bytes::Bytes::copy_from_slice(bytes),
        });
        ticks += deltas[i];
    }

    Ok((
        Track {
            id: TrackId(track_id),
            codec,
            kind,
            extra_data,
        },
        packets,
    ))
}

fn duration_from_ticks(ticks: u64, timescale: u64) -> Result<std::time::Duration, Error> {
    let ns = (ticks as u128)
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_div(timescale as u128))
        .ok_or_else(|| Error::Demux("timestamp overflow".into()))?;
    Ok(std::time::Duration::from_nanos(ns as u64))
}

fn read_stsd(
    data: &[u8],
    stbl_body: usize,
    stbl_end: usize,
) -> Result<(Codec, TrackKind, Vec<u8>), Error> {
    let (_, stsd_body, stsd_end) = find_box(data, stbl_body, stbl_end, b"stsd")
        .ok_or_else(|| Error::Demux("stbl missing stsd".into()))?;
    // version+flags(4) + entry_count(4) precede the sample entries.
    let (_, avc1_body, avc1_end) =
        find_box(data, stsd_body + 8, stsd_end, b"avc1").ok_or_else(|| {
            Error::Demux("stsd has no avc1 sample entry (video codec unsupported)".into())
        })?;

    let width = u16_at(data, avc1_body + 24)? as u32;
    let height = u16_at(data, avc1_body + 26)? as u32;

    let (_, avcc_body, avcc_end) = find_box(data, avc1_body + 78, avc1_end, b"avcC")
        .ok_or_else(|| Error::Demux("avc1 sample entry has no avcC box".into()))?;
    let extra_data = data[avcc_body..avcc_end].to_vec();

    Ok((Codec::H264, TrackKind::Video { width, height }, extra_data))
}

/// Per-sample decode-time deltas (in track ticks), expanded from `stts` runs.
fn read_stts(data: &[u8], stbl_body: usize, stbl_end: usize) -> Result<Vec<u64>, Error> {
    let (_, body, end) = find_box(data, stbl_body, stbl_end, b"stts")
        .ok_or_else(|| Error::Demux("stbl missing stts".into()))?;
    let entry_count = u32_at(data, body + 4)?;
    let mut out = Vec::new();
    for i in 0..entry_count {
        let off = body + 8 + i as usize * 8;
        if off + 8 > end {
            return Err(Error::Demux("stts entry overruns its box".into()));
        }
        let count = u32_at(data, off)?;
        let delta = u32_at(data, off + 4)? as u64;
        out.extend(std::iter::repeat_n(delta, count as usize));
    }
    Ok(out)
}

fn read_stsz(data: &[u8], stbl_body: usize, stbl_end: usize) -> Result<Vec<u32>, Error> {
    let (_, body, end) = find_box(data, stbl_body, stbl_end, b"stsz")
        .ok_or_else(|| Error::Demux("stbl missing stsz".into()))?;
    let uniform_size = u32_at(data, body + 4)?;
    let sample_count = u32_at(data, body + 8)? as usize;
    if uniform_size != 0 {
        return Ok(vec![uniform_size; sample_count]);
    }
    let mut out = Vec::with_capacity(sample_count);
    for i in 0..sample_count {
        let off = body + 12 + i * 4;
        if off + 4 > end {
            return Err(Error::Demux("stsz entry overruns its box".into()));
        }
        out.push(u32_at(data, off)?);
    }
    Ok(out)
}

/// Absolute file offset of every sample, walking `stsc` chunk groups against
/// `stco` chunk offsets and `stsz` sizes (samples within a chunk are packed
/// back-to-back with no gap, per spec).
fn read_sample_offsets(
    data: &[u8],
    stbl_body: usize,
    stbl_end: usize,
    sample_count: usize,
) -> Result<Vec<usize>, Error> {
    let (_, stco_body, stco_end) = find_box(data, stbl_body, stbl_end, b"stco")
        .ok_or_else(|| Error::Demux("stbl missing stco".into()))?;
    let chunk_count = u32_at(data, stco_body + 4)? as usize;
    let mut chunk_offsets = Vec::with_capacity(chunk_count);
    for i in 0..chunk_count {
        let off = stco_body + 8 + i * 4;
        if off + 4 > stco_end {
            return Err(Error::Demux("stco entry overruns its box".into()));
        }
        chunk_offsets.push(u32_at(data, off)? as usize);
    }

    let (_, stsc_body, stsc_end) = find_box(data, stbl_body, stbl_end, b"stsc")
        .ok_or_else(|| Error::Demux("stbl missing stsc".into()))?;
    let stsc_entries = u32_at(data, stsc_body + 4)?;
    // (first_chunk, samples_per_chunk), 1-based chunk numbers, spec-sorted.
    let mut runs = Vec::with_capacity(stsc_entries as usize);
    for i in 0..stsc_entries {
        let off = stsc_body + 8 + i as usize * 12;
        if off + 12 > stsc_end {
            return Err(Error::Demux("stsc entry overruns its box".into()));
        }
        runs.push((u32_at(data, off)? as usize, u32_at(data, off + 4)? as usize));
    }

    let sizes = read_stsz(data, stbl_body, stbl_end)?;
    let mut offsets = Vec::with_capacity(sample_count);
    let mut sample = 0usize;
    for (run_idx, &(first_chunk, samples_per_chunk)) in runs.iter().enumerate() {
        let last_chunk = runs
            .get(run_idx + 1)
            .map(|&(next, _)| next - 1)
            .unwrap_or(chunk_count);
        for chunk in first_chunk..=last_chunk {
            let mut pos = *chunk_offsets
                .get(chunk - 1)
                .ok_or_else(|| Error::Demux(format!("stsc references chunk {chunk} past stco")))?;
            for _ in 0..samples_per_chunk {
                if sample >= sample_count {
                    return Err(Error::Demux("stsc describes more samples than stsz".into()));
                }
                offsets.push(pos);
                pos += sizes[sample] as usize;
                sample += 1;
            }
        }
    }
    if sample != sample_count {
        return Err(Error::Demux(format!(
            "stsc covers {sample} samples, stsz declares {sample_count}"
        )));
    }
    Ok(offsets)
}

/// Keyframe flag per sample. Absent `stss` means every sample is a sync
/// sample, per spec.
fn read_stss(
    data: &[u8],
    stbl_body: usize,
    stbl_end: usize,
    sample_count: usize,
) -> Result<Vec<bool>, Error> {
    let Some((_, body, end)) = find_box(data, stbl_body, stbl_end, b"stss") else {
        return Ok(vec![true; sample_count]);
    };
    let entry_count = u32_at(data, body + 4)?;
    let mut sync = vec![false; sample_count];
    for i in 0..entry_count {
        let off = body + 8 + i as usize * 4;
        if off + 4 > end {
            return Err(Error::Demux("stss entry overruns its box".into()));
        }
        let sample_number = u32_at(data, off)? as usize;
        let index = sample_number
            .checked_sub(1)
            .ok_or_else(|| Error::Demux("stss sample_number is 0".into()))?;
        *sync
            .get_mut(index)
            .ok_or_else(|| Error::Demux("stss references a sample past stsz".into()))? = true;
    }
    Ok(sync)
}

fn u32_at(data: &[u8], pos: usize) -> Result<u32, Error> {
    data.get(pos..pos + 4)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
        .ok_or_else(|| Error::Demux(format!("truncated u32 at offset {pos}")))
}

fn u16_at(data: &[u8], pos: usize) -> Result<u16, Error> {
    data.get(pos..pos + 2)
        .map(|b| u16::from_be_bytes(b.try_into().unwrap()))
        .ok_or_else(|| Error::Demux(format!("truncated u16 at offset {pos}")))
}

/// One box at `pos`: `(fourcc, body_start, box_end)`.
fn read_box(data: &[u8], pos: usize) -> Result<([u8; 4], usize, usize), Error> {
    if pos + 8 > data.len() {
        return Err(Error::Demux(format!(
            "truncated box header at offset {pos}"
        )));
    }
    let size = u32_at(data, pos)? as usize;
    let fourcc: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();
    if size < 8 {
        return Err(Error::Demux(format!(
            "box {fourcc:?} has impossible size {size}"
        )));
    }
    let end = pos
        .checked_add(size)
        .ok_or_else(|| Error::Demux("box size overflows".into()))?;
    if end > data.len() {
        return Err(Error::Demux(format!("box {fourcc:?} overruns the buffer")));
    }
    Ok((fourcc, pos + 8, end))
}

/// The first child box named `want` inside `[start, end)`: `(fourcc,
/// body_start, box_end)`.
fn find_box(data: &[u8], start: usize, end: usize, want: &[u8; 4]) -> Option<(u32, usize, usize)> {
    all_boxes(data, start, end, want).into_iter().next()
}

/// Every child box named `want` inside `[start, end)`.
fn all_boxes(data: &[u8], start: usize, end: usize, want: &[u8; 4]) -> Vec<(u32, usize, usize)> {
    let mut pos = start;
    let mut out = Vec::new();
    while pos < end {
        let Ok((fourcc, body, box_end)) = read_box(data, pos) else {
            break;
        };
        if &fourcc == want {
            out.push((u32::from_be_bytes(fourcc), body, box_end));
        }
        pos = box_end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::h264::avcc;
    use crate::demux::AnnexBDemuxer;
    use crate::mux::Mp4Muxer;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/tapo-1080p-cabac-8x8.h264");

    /// `Mp4Muxer` only writes bytes inside `finalize`, which consumes the
    /// muxer — there is no `self` left afterward to hand a writer back from.
    /// A shared buffer lets this test read the bytes anyway.
    #[derive(Clone)]
    struct SharedBuf(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn muxed_fixture() -> Vec<u8> {
        let mut demux = AnnexBDemuxer::new(FIXTURE, 22).unwrap();
        let tracks: Vec<Track> = demux
            .tracks()
            .iter()
            .cloned()
            .map(|mut t| {
                t.extra_data = avcc::parameter_set_record(&t.extra_data).unwrap();
                t
            })
            .collect();

        let buf = SharedBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
        let mut mux = Mp4Muxer::new(buf.clone(), tracks).unwrap();
        while let Some(pkt) = demux.read_packet().unwrap() {
            mux.write_packet(&Packet {
                data: bytes::Bytes::from(avcc::access_unit_to_avcc(&pkt.data)),
                ..pkt
            })
            .unwrap();
        }
        mux.finalize().unwrap();
        let bytes = buf.0.borrow().clone();
        bytes
    }

    #[test]
    fn parses_track_and_samples_from_our_own_muxer() {
        let bytes = muxed_fixture();
        let mut demux = Mp4Demuxer::new(&bytes).unwrap();

        assert_eq!(demux.tracks().len(), 1);
        assert_eq!(demux.tracks()[0].codec, Codec::H264);
        assert!(matches!(
            demux.tracks()[0].kind,
            TrackKind::Video {
                width: 1920,
                height: 1080
            }
        ));

        let mut count = 0;
        let mut last_pts = None;
        while let Some(pkt) = demux.read_packet().unwrap() {
            if let Some(prev) = last_pts {
                assert!(pkt.pts >= prev);
            }
            last_pts = Some(pkt.pts);
            count += 1;
        }
        assert_eq!(count, 4);
    }

    #[test]
    fn rejects_a_file_with_no_moov() {
        assert!(Mp4Demuxer::new(&[0, 0, 0, 8, b'f', b't', b'y', b'p']).is_err());
    }
}
