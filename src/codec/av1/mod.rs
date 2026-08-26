//! AV1 encoding via rav1e, gated behind the `av1` feature.
//!
//! The shipping configuration is speed 10, 16 tiles, low latency. It measured
//! 30.1 fps over the 224-picture 1080p camera capture; speed 9 reached only
//! 18.0 fps. See `examples/bench_av1.rs` for the reproducible sweep.

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
    force_keyframe: bool,
}

/// Performance-sensitive rav1e settings exposed for benchmark-driven product
/// selection. Defaults are the current shipping configuration.
#[derive(Debug, Clone, Copy)]
pub struct Av1Settings {
    pub speed: u8,
    pub tiles: usize,
    pub low_latency: bool,
    /// Zero lets rav1e use its global Rayon pool.
    pub threads: usize,
}

impl Default for Av1Settings {
    fn default() -> Self {
        Self {
            speed: 10,
            tiles: 16,
            low_latency: true,
            threads: 0,
        }
    }
}

impl Av1Settings {
    /// Builds the rav1e configuration these settings describe.
    ///
    /// Exposed so benchmarks measure the shipping configuration itself rather
    /// than a copy that can drift from it. `bitrate` is in bits per second.
    pub fn encoder_config(&self, width: u32, height: u32, fps: u32, bitrate: i32) -> EncoderConfig {
        let fps = u64::from(fps.max(1));
        EncoderConfig {
            width: width as usize,
            height: height as usize,
            bit_depth: 8,
            chroma_sampling: ChromaSampling::Cs420,
            time_base: Rational::new(1, fps),
            speed_settings: SpeedSettings::from_preset(self.speed),
            bitrate,
            low_latency: self.low_latency,
            tiles: self.tiles,
            min_key_frame_interval: fps * 2,
            max_key_frame_interval: fps * 2,
            ..Default::default()
        }
    }
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
        Self::with_settings(track, width, height, fps, bitrate, Av1Settings::default())
    }

    pub fn with_settings(
        track: TrackId,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: i32,
        settings: Av1Settings,
    ) -> Result<Self, Error> {
        let enc = settings.encoder_config(width, height, fps, bitrate);

        let cfg = Config::new()
            .with_encoder_config(enc)
            .with_threads(settings.threads);
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
            force_keyframe: false,
        })
    }

    fn packet_from(&mut self, pkt: rav1e::prelude::Packet<u8>) -> Result<Packet, Error> {
        let pts = self.pending.remove(&pkt.input_frameno).ok_or_else(|| {
            Error::Encode(format!(
                "rav1e returned unknown input frame {}",
                pkt.input_frameno
            ))
        })?;
        Ok(Packet {
            track: self.track,
            pts,
            keyframe: pkt.frame_type == FrameType::KEY,
            data: pkt.data.into(),
        })
    }

    /// Drains every packet currently available without blocking for more
    /// input, which is what both `encode` (after one `send_frame`) and
    /// `flush` (after signalling end of stream) need.
    fn drain(&mut self, out: &mut Vec<Packet>) -> Result<(), Error> {
        loop {
            match self.ctx.receive_packet() {
                Ok(pkt) => {
                    let packet = self.packet_from(pkt)?;
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
        validate_plane(
            "Y",
            &frame.planes[0],
            frame.strides[0],
            frame.width as usize,
            frame.height as usize,
        )?;
        let (cw, ch) = (
            (frame.width as usize).div_ceil(2),
            (frame.height as usize).div_ceil(2),
        );
        validate_plane("U", &frame.planes[1], frame.strides[1], cw, ch)?;
        validate_plane("V", &frame.planes[2], frame.strides[2], cw, ch)?;
        let mut rav1e_frame = self.ctx.new_frame();
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
        let send_result = if self.force_keyframe {
            self.ctx.send_frame((
                rav1e_frame,
                FrameParameters {
                    frame_type_override: FrameTypeOverride::Key,
                    ..Default::default()
                },
            ))
        } else {
            self.ctx.send_frame(rav1e_frame)
        };
        send_result.map_err(|e| Error::Encode(format!("rav1e: {e:?}")))?;

        let mut out = Vec::new();
        self.drain(&mut out)?;
        Ok(out)
    }

    fn encode_keyframe(&mut self, frame: &Frame) -> Result<Vec<Packet>, Error> {
        self.force_keyframe = true;
        let result = self.encode(frame);
        self.force_keyframe = false;
        result
    }

    fn flush(&mut self) -> Result<Vec<Packet>, Error> {
        self.ctx.flush();
        let mut out = Vec::new();
        loop {
            match self.ctx.receive_packet() {
                Ok(pkt) => {
                    let packet = self.packet_from(pkt)?;
                    out.push(packet);
                }
                Err(EncoderStatus::Encoded) => continue,
                Err(EncoderStatus::LimitReached | EncoderStatus::NeedMoreData) => break,
                Err(e) => return Err(Error::Encode(format!("rav1e: {e:?}"))),
            }
        }
        if !self.pending.is_empty() {
            return Err(Error::Encode(format!(
                "rav1e flush left {} frame timestamp(s) pending",
                self.pending.len()
            )));
        }
        Ok(out)
    }
}

fn validate_plane(
    name: &str,
    data: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> Result<(), Error> {
    if stride < width {
        return Err(Error::Encode(format!(
            "{name} plane stride {stride} is smaller than width {width}"
        )));
    }
    let required = if height == 0 {
        0
    } else {
        (height - 1)
            .checked_mul(stride)
            .and_then(|offset| offset.checked_add(width))
            .ok_or_else(|| Error::Encode(format!("{name} plane layout overflows usize")))?
    };
    if data.len() < required {
        return Err(Error::Encode(format!(
            "{name} plane has {} bytes; {required} required for {width}x{height} at stride {stride}",
            data.len()
        )));
    }
    Ok(())
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
            concealed_macroblocks: 0,
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
    fn requested_keyframe_is_exposed_on_the_output_packet() {
        let mut enc = Av1Encoder::new(TrackId(0), 16, 16, 25, 60_000).unwrap();
        let first = synthetic_frame(16, 16, Duration::ZERO, 10);
        let forced = synthetic_frame(16, 16, Duration::from_millis(40), 20);
        let mut packets = enc.encode(&first).unwrap();
        packets.extend(enc.encode_keyframe(&forced).unwrap());
        packets.extend(enc.flush().unwrap());

        let packet = packets
            .iter()
            .find(|packet| packet.pts == forced.pts)
            .expect("forced frame packet");
        assert!(packet.keyframe);
    }

    #[test]
    fn mismatched_frame_size_is_rejected() {
        let mut enc = Av1Encoder::new(TrackId(0), 16, 16, 25, 60_000).unwrap();
        let frame = synthetic_frame(32, 32, Duration::ZERO, 0);
        assert!(enc.encode(&frame).is_err());
    }

    #[test]
    fn undersized_plane_stride_is_rejected() {
        let mut enc = Av1Encoder::new(TrackId(0), 16, 16, 25, 60_000).unwrap();
        let mut frame = synthetic_frame(16, 16, Duration::ZERO, 0);
        frame.strides[0] = 15;
        let error = enc.encode(&frame).unwrap_err();
        assert!(error.to_string().contains("Y plane stride 15"));
    }

    #[test]
    fn truncated_plane_is_rejected() {
        let mut enc = Av1Encoder::new(TrackId(0), 16, 16, 25, 60_000).unwrap();
        let mut frame = synthetic_frame(16, 16, Duration::ZERO, 0);
        frame.planes[1].pop();
        let error = enc.encode(&frame).unwrap_err();
        assert!(error
            .to_string()
            .contains("U plane has 63 bytes; 64 required"));
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
        assert!(enc.pending.is_empty());
    }

    #[test]
    fn odd_width_encodes_and_flushes() {
        let mut enc = Av1Encoder::new(TrackId(0), 17, 16, 25, 60_000).unwrap();
        let frame = synthetic_frame(17, 16, Duration::from_millis(40), 42);
        enc.encode(&frame).unwrap();
        let packets = enc.flush().unwrap();
        assert!(!packets.is_empty());
        assert_eq!(packets[0].pts, Duration::from_millis(40));
    }

    #[test]
    fn odd_height_encodes_and_flushes() {
        let mut enc = Av1Encoder::new(TrackId(0), 16, 17, 25, 60_000).unwrap();
        let frame = synthetic_frame(16, 17, Duration::from_millis(40), 42);
        enc.encode(&frame).unwrap();
        let packets = enc.flush().unwrap();
        assert!(!packets.is_empty());
        assert_eq!(packets[0].pts, Duration::from_millis(40));
    }

    #[test]
    fn realistic_resolution_encodes_without_panic() {
        let mut enc = Av1Encoder::new(TrackId(0), 640, 360, 25, 60_000).unwrap();
        let frame = synthetic_frame(640, 360, Duration::from_millis(40), 7);
        enc.encode(&frame).unwrap();
        let packets = enc.flush().unwrap();
        assert!(!packets.is_empty());
        assert_eq!(packets[0].pts, Duration::from_millis(40));
    }

    #[test]
    fn packet_for_unknown_frame_number_is_rejected() {
        let mut enc = Av1Encoder::new(TrackId(0), 16, 16, 25, 60_000).unwrap();
        let packet = rav1e::prelude::Packet {
            data: vec![1, 2, 3],
            rec: None,
            source: None,
            input_frameno: 99,
            frame_type: FrameType::INTER,
            qp: 100,
            enc_stats: Default::default(),
            opaque: None,
        };
        let error = enc.packet_from(packet).unwrap_err();
        assert!(error.to_string().contains("unknown input frame 99"));
    }
}
