//! AV1 encoding via rav1e, gated behind the `av1` feature.
//!
//! Configuration mirrors `examples/bench_rav1e.rs`: speed 8, 4 tiles, low
//! latency. That combination measured ~2x real time at 640x360, the shape
//! the transcode target actually produces, so it is the only preset wired up
//! here rather than a knob left for callers to tune.

use std::collections::HashMap;
use std::time::Duration;

use rav1e::config::SpeedSettings;
use rav1e::prelude::*;

use crate::media::{Codec, Encoder, Frame, Packet, TrackId};
use crate::Error;

/// Encodes decoded frames to AV1 with rav1e, tuned for a supervised
/// real-time recorder rather than a batch file encoder.
pub struct Av1Encoder {
    ctx: Context<u8>,
    track: TrackId,
    width: u32,
    height: u32,
    /// PTS of frames sent but not yet returned as a packet, keyed by rav1e's
    /// `input_frameno`. Low-latency mode does not reorder, but packets can
    /// still lag a `send_frame` call, so PTS has to travel with the frame
    /// number rather than being read off in send order.
    pending: HashMap<u64, Duration>,
    next_frameno: u64,
}

impl Av1Encoder {
    /// `bitrate` is in bits per second. `fps` only sets the keyframe
    /// interval (2 seconds, matching the benchmarked configuration) and the
    /// rate-control time base; frames are otherwise encoded in call order
    /// regardless of their actual cadence.
    pub fn new(
        track: TrackId,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: i32,
    ) -> Result<Self, Error> {
        let fps = u64::from(fps.max(1));
        let enc = EncoderConfig {
            width: width as usize,
            height: height as usize,
            bit_depth: 8,
            chroma_sampling: ChromaSampling::Cs420,
            time_base: Rational::new(1, fps),
            speed_settings: SpeedSettings::from_preset(8),
            bitrate,
            low_latency: true,
            tiles: 4,
            min_key_frame_interval: fps * 2,
            max_key_frame_interval: fps * 2,
            ..Default::default()
        };

        let cfg = Config::new().with_encoder_config(enc);
        let ctx = cfg
            .new_context()
            .map_err(|e| Error::Encode(format!("rav1e: {e:?}")))?;
        Ok(Self {
            ctx,
            track,
            width,
            height,
            pending: HashMap::new(),
            next_frameno: 0,
        })
    }

    fn packet_from(&mut self, pkt: rav1e::prelude::Packet<u8>) -> Packet {
        let pts = self.pending.remove(&pkt.input_frameno).unwrap_or_default();
        Packet {
            track: self.track,
            pts,
            keyframe: pkt.frame_type == FrameType::KEY,
            data: pkt.data.into(),
        }
    }

    /// Drains every packet currently available without blocking for more
    /// input, which is what both `encode` (after one `send_frame`) and
    /// `flush` (after signalling end of stream) need.
    fn drain(&mut self, out: &mut Vec<Packet>) -> Result<(), Error> {
        loop {
            match self.ctx.receive_packet() {
                Ok(pkt) => {
                    let packet = self.packet_from(pkt);
                    out.push(packet);
                }
                Err(EncoderStatus::Encoded) => continue,
                Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => return Ok(()),
                Err(e) => return Err(Error::Encode(format!("rav1e: {e:?}"))),
            }
        }
    }
}

impl Encoder for Av1Encoder {
    fn codec(&self) -> Codec {
        Codec::Av1
    }

    fn extra_data(&self) -> Vec<u8> {
        self.ctx.container_sequence_header()
    }

    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>, Error> {
        if frame.width != self.width || frame.height != self.height {
            return Err(Error::Encode(format!(
                "frame is {}x{}, encoder configured for {}x{}",
                frame.width, frame.height, self.width, self.height
            )));
        }
        let mut rav1e_frame = self.ctx.new_frame();
        let (cw, ch) = (
            (frame.width as usize).div_ceil(2),
            (frame.height as usize).div_ceil(2),
        );
        copy_plane(
            &mut rav1e_frame.planes[0],
            &frame.planes[0],
            frame.strides[0],
            frame.width as usize,
            frame.height as usize,
        );
        copy_plane(
            &mut rav1e_frame.planes[1],
            &frame.planes[1],
            frame.strides[1],
            cw,
            ch,
        );
        copy_plane(
            &mut rav1e_frame.planes[2],
            &frame.planes[2],
            frame.strides[2],
            cw,
            ch,
        );

        self.pending.insert(self.next_frameno, frame.pts);
        self.next_frameno += 1;
        self.ctx
            .send_frame(rav1e_frame)
            .map_err(|e| Error::Encode(format!("rav1e: {e:?}")))?;

        let mut out = Vec::new();
        self.drain(&mut out)?;
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<Packet>, Error> {
        self.ctx.flush();
        let mut out = Vec::new();
        loop {
            match self.ctx.receive_packet() {
                Ok(pkt) => {
                    let packet = self.packet_from(pkt);
                    out.push(packet);
                }
                Err(EncoderStatus::Encoded) => continue,
                Err(EncoderStatus::LimitReached | EncoderStatus::NeedMoreData) => break,
                Err(e) => return Err(Error::Encode(format!("rav1e: {e:?}"))),
            }
        }
        Ok(out)
    }
}

/// Copies one plane's valid `width`x`height` samples, row by row, from our
/// tightly-strided `Frame` into rav1e's padded `Plane`.
fn copy_plane(dst: &mut Plane<u8>, src: &[u8], src_stride: usize, width: usize, height: usize) {
    let stride = dst.cfg.stride;
    let data = dst.data_origin_mut();
    for row in 0..height {
        let src_row = &src[row * src_stride..row * src_stride + width];
        data[row * stride..row * stride + width].copy_from_slice(src_row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_frame(width: u32, height: u32, pts: Duration, n: u8) -> Frame {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y = vec![n; w * h];
        let u = vec![128u8.wrapping_add(n); cw * ch];
        let v = vec![128u8.wrapping_sub(n); cw * ch];
        Frame {
            pts,
            width,
            height,
            planes: [y, u, v],
            strides: [w, cw, cw],
        }
    }

    #[test]
    fn first_frame_produces_a_keyframe_with_its_pts() {
        let mut enc = Av1Encoder::new(TrackId(0), 16, 16, 25, 60_000).unwrap();
        let frame = synthetic_frame(16, 16, Duration::from_millis(40), 10);
        let mut packets = enc.encode(&frame).unwrap();
        packets.extend(enc.flush().unwrap());
        assert!(!packets.is_empty());
        assert!(packets[0].keyframe);
        assert_eq!(packets[0].pts, Duration::from_millis(40));
        assert_eq!(packets[0].track, TrackId(0));
    }

    #[test]
    fn mismatched_frame_size_is_rejected() {
        let mut enc = Av1Encoder::new(TrackId(0), 16, 16, 25, 60_000).unwrap();
        let frame = synthetic_frame(32, 32, Duration::ZERO, 0);
        assert!(enc.encode(&frame).is_err());
    }

    #[test]
    fn extra_data_is_a_non_empty_av1_codec_configuration_record() {
        let enc = Av1Encoder::new(TrackId(0), 16, 16, 25, 60_000).unwrap();
        assert!(!enc.extra_data().is_empty());
    }

    #[test]
    fn flush_drains_every_frame_sent_before_it() {
        let mut enc = Av1Encoder::new(TrackId(0), 16, 16, 25, 60_000).unwrap();
        let mut sent = Vec::new();
        for n in 0..5u8 {
            let pts = Duration::from_millis(40 * u64::from(n));
            sent.push(pts);
            enc.encode(&synthetic_frame(16, 16, pts, n * 20)).unwrap();
        }
        let packets = enc.flush().unwrap();
        let mut seen: Vec<_> = packets.iter().map(|p| p.pts).collect();
        seen.sort();
        let mut expected = sent;
        expected.sort();
        assert_eq!(seen, expected);
    }
}
