//! The openh264 reference decoder, behind the [`Decoder`] seam.
//!
//! Cisco's implementation is the oracle the pure-Rust decoder is checked
//! against. It is compiled from vendored C and so is the exact dependency
//! this project exists to remove; it is therefore gated behind the
//! `reference-decoder` feature, never enabled by default, and used only from
//! tests and [`super::differential`].
//!
//! Correctness in a video decoder is bit-exact and failures are delayed: a
//! wrong rounding mode in deblocking does not soften one frame, it feeds
//! inter prediction and rots the picture seconds later. Hand-written unit
//! tests cannot see that coming. Diffing every frame against a decoder that
//! is known-correct can.

use std::time::Duration;

use openh264::decoder::{Decoder as Openh264Decoder, DecoderConfig};
use openh264::encoder::{
    Encoder as Openh264Encoder, EncoderConfig, FrameRate, IntraFramePeriod, Profile,
    RateControlMode,
};
use openh264::formats::{YUVSlices, YUVSource};
use openh264::OpenH264API;

use crate::media::{Decoder, Frame, Packet, TrackId};
use crate::Error;

fn decode_err(e: impl std::fmt::Display) -> Error {
    Error::Decode(format!("openh264: {e}"))
}

/// A [`Decoder`] backed by Cisco's openh264.
pub struct ReferenceDecoder {
    inner: Openh264Decoder,
}

impl ReferenceDecoder {
    pub fn new() -> Result<Self, Error> {
        let config = DecoderConfig::new().debug(false);
        let inner = Openh264Decoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(decode_err)?;
        Ok(Self { inner })
    }
}

impl Decoder for ReferenceDecoder {
    /// `pkt.data` must be one Annex B access unit; see [`super::annexb`].
    ///
    /// openh264 emits at most one picture per call, so the returned vector
    /// holds zero or one frame even though the trait permits more.
    fn decode(&mut self, pkt: &Packet) -> Result<Vec<Frame>, Error> {
        let decoded = self.inner.decode(&pkt.data).map_err(decode_err)?;
        Ok(decoded
            .map(|yuv| to_frame(&yuv, pkt.pts))
            .into_iter()
            .collect())
    }

    fn flush(&mut self) -> Result<Vec<Frame>, Error> {
        // openh264 hands back the pictures still in its buffer, but without
        // usable timestamps; the harness only ever compares pixels, and the
        // live path drives this decoder one access unit at a time, so a
        // placeholder is honest here rather than a guess dressed up as data.
        let frames = self.inner.flush_remaining().map_err(decode_err)?;
        Ok(frames
            .iter()
            .map(|yuv| to_frame(yuv, Duration::ZERO))
            .collect())
    }
}

/// Copies a decoded picture out of openh264's internal buffers.
///
/// The copy is not incidental: openh264 hands out slices pointing into memory
/// it reuses on the next call, and the planes are stride-padded, which our
/// [`Frame`] permits but downstream comparison is simpler without.
fn to_frame(yuv: &impl YUVSource, pts: Duration) -> Frame {
    let (width, height) = yuv.dimensions();
    let (y_stride, u_stride, v_stride) = yuv.strides();
    Frame {
        pts,
        width: width as u32,
        height: height as u32,
        planes: [
            pack(yuv.y(), y_stride, width, height),
            pack(yuv.u(), u_stride, width.div_ceil(2), height.div_ceil(2)),
            pack(yuv.v(), v_stride, width.div_ceil(2), height.div_ceil(2)),
        ],
        strides: [width, width.div_ceil(2), width.div_ceil(2)],
    }
}

/// Drops row padding, producing a tightly packed plane.
fn pack(plane: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
    (0..height)
        .flat_map(|row| &plane[row * stride..row * stride + width])
        .copied()
        .collect()
}

/// Encodes a synthetic Annex B stream, for tests that need real bitstream
/// data without a camera on the network.
///
/// Captures from the Tapo are the streams that actually matter, but they are
/// large, and a test that only runs when someone remembered to record one is
/// a test that does not run. This produces the same coding tools — High
/// profile, CABAC, 8x8 transform — deterministically and in milliseconds.
pub struct SyntheticStream {
    pub annexb: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// Encodes `frames` pictures of a moving test pattern at `width`x`height`.
///
/// `keyframe_interval` is in frames; 1 gives an all-intra stream, which is
/// the right shape for exercising a decoder that cannot yet do inter
/// prediction.
pub fn synthesize(
    width: usize,
    height: usize,
    frames: usize,
    keyframe_interval: u32,
) -> Result<SyntheticStream, Error> {
    let config = EncoderConfig::new()
        .profile(Profile::High)
        .max_frame_rate(FrameRate::from_hz(25.0))
        .intra_frame_period(IntraFramePeriod::from_num_frames(keyframe_interval))
        // Deterministic output matters more than bitrate here: rate control
        // that adapts to wall-clock timing, or a scene-change heuristic that
        // inserts a keyframe of its own, would make the fixture differ
        // between runs and turn any regression into a coin flip. Turning rate
        // control off entirely also means we never ask openh264 to drop a
        // frame, which would break the one-access-unit-per-input-frame
        // correspondence the harness relies on.
        .rate_control_mode(RateControlMode::Off)
        .num_threads(1)
        .scene_change_detect(false)
        .adaptive_quantization(false)
        .skip_frames(false)
        .debug(false);

    let mut encoder = Openh264Encoder::with_api_config(OpenH264API::from_source(), config)
        .map_err(|e| Error::Encode(format!("openh264: {e}")))?;

    let mut annexb = Vec::new();
    for i in 0..frames {
        let (y, u, v) = test_pattern(width, height, i);
        let slices = YUVSlices::new(
            (&y, &u, &v),
            (width, height),
            (width, width.div_ceil(2), width.div_ceil(2)),
        );
        encoder
            .encode(&slices)
            .map_err(|e| Error::Encode(format!("openh264: {e}")))?
            .write_vec(&mut annexb);
    }

    Ok(SyntheticStream {
        annexb,
        width,
        height,
    })
}

/// A moving gradient with a hard-edged block sliding across it.
///
/// The gradient gives the transform something with low-frequency content to
/// work on, the edges give the deblocking filter something to smooth, and the
/// motion gives inter prediction a reason to produce non-zero vectors. Flat
/// or noisy input would exercise none of the three.
fn test_pattern(width: usize, height: usize, frame: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let shift = (frame * 3) % width;

    let mut y = vec![0u8; width * height];
    for row in 0..height {
        for col in 0..width {
            let gradient = ((row + col + frame) % 256) as u8;
            let in_block = (col + shift) % width < width / 8 && row % height < height / 2;
            y[row * width + col] = if in_block { 235 } else { gradient };
        }
    }

    let mut u = vec![0u8; cw * ch];
    let mut v = vec![0u8; cw * ch];
    for row in 0..ch {
        for col in 0..cw {
            u[row * cw + col] = (128 + (col as i32 - cw as i32 / 2).clamp(-64, 64)) as u8;
            v[row * cw + col] = (128 + (row as i32 - ch as i32 / 2).clamp(-64, 64)) as u8;
        }
    }
    (y, u, v)
}

/// Wraps one access unit as a [`Packet`] for the [`Decoder`] seam.
pub fn packet(au: &[u8], index: usize) -> Packet {
    Packet {
        track: TrackId(0),
        pts: Duration::from_millis(index as u64 * 40),
        keyframe: super::annexb::nal_units(au).any(|n| n[0] & 0x1f == 5),
        data: bytes::Bytes::copy_from_slice(au),
    }
}
