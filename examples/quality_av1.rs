//! Sweeps AV1 encoder settings for speed *and* compression quality.
//!
//! `bench_av1` answers "is it fast enough". This answers the follow-up the
//! product actually cares about: whether a faster configuration pays for its
//! speed in picture quality. Rate control holds the bitrate roughly constant,
//! so a configuration that codes worse does not produce a bigger file, it
//! produces a worse picture at the same size. That is invisible unless quality
//! is measured, so this example reads rav1e's own reconstruction
//! (`Packet::rec`) against its input (`Packet::source`) and reports PSNR per
//! configuration.
//!
//! Run with:
//! `cargo run --release --features av1 --example quality_av1 -- INPUT.h264`

use std::time::{Duration, Instant};

use lite_encoder::codec::av1::Av1Settings;
use lite_encoder::codec::h264::{annexb, decoder::Frontend};
use lite_encoder::media::Frame;

use rav1e::prelude::*;

/// Assumed camera frame rate: sets the keyframe interval and the media
/// duration the bitrate is computed against.
const FPS: u32 = 30;

/// Same 1080p surveillance bitrate `bench_av1` uses, so the two tables are
/// directly comparable.
const BITRATE_1080P: i32 = 6_000_000;

/// Minimum acceptable encode fps at 1080p (1.0x real time at 30 fps).
const MIN_ENCODE_FPS_1080P: f64 = 30.0;

struct Measurement {
    label: String,
    settings: Av1Settings,
    fps: f64,
    kbit_s: f64,
    psnr: [f64; 3],
    /// Frames sent before the first packet came back: encode latency in
    /// frames, which is what `low_latency: false` trades away.
    latency_frames: usize,
}

fn decode_frames(data: &[u8]) -> Vec<Frame> {
    let mut decoder = Frontend::new();
    let mut frames = Vec::new();
    for (index, access_unit) in annexb::access_units(data).into_iter().enumerate() {
        let pts = Duration::from_millis(index as u64 * 1000 / u64::from(FPS));
        match decoder.decode_access_unit(access_unit, pts) {
            Ok(decoded) => frames.extend(decoded),
            Err(error) => {
                eprintln!("decode stopped at access unit {index}: {error}");
                break;
            }
        }
    }
    frames
}

/// Sum of squared error and sample count between two planes of the same size.
fn plane_sse(rec: &Plane<u8>, source: &Plane<u8>) -> (f64, usize) {
    let mut sse = 0f64;
    let mut count = 0usize;
    for (rec_row, src_row) in rec.rows_iter().zip(source.rows_iter()) {
        let width = rec_row.len().min(src_row.len());
        for (r, s) in rec_row[..width].iter().zip(&src_row[..width]) {
            let diff = f64::from(*r) - f64::from(*s);
            sse += diff * diff;
        }
        count += width;
    }
    (sse, count)
}

fn psnr(sse: f64, count: usize) -> f64 {
    if count == 0 {
        return f64::NAN;
    }
    let mse = sse / count as f64;
    if mse <= 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0 * 255.0 / mse).log10()
}

fn copy_plane(dst: &mut Plane<u8>, src: &[u8], src_stride: usize, width: usize, height: usize) {
    for (row, dst_row) in dst.rows_iter_mut().take(height).enumerate() {
        let start = row * src_stride;
        dst_row[..width].copy_from_slice(&src[start..start + width]);
    }
}

fn accumulate(packet: &Packet<u8>, sse: &mut [f64; 3], counts: &mut [usize; 3]) {
    let (Some(rec), Some(source)) = (packet.rec.as_ref(), packet.source.as_ref()) else {
        return;
    };
    for plane in 0..3 {
        let (plane_sse_value, count) = plane_sse(&rec.planes[plane], &source.planes[plane]);
        sse[plane] += plane_sse_value;
        counts[plane] += count;
    }
}

fn measure(
    label: &str,
    settings: Av1Settings,
    frames: &[Frame],
    size: (u32, u32),
    measure_quality: bool,
) -> Result<Measurement, Box<dyn std::error::Error>> {
    let enc = settings.encoder_config(size.0, size.1, FPS, BITRATE_1080P);
    let cfg = Config::new()
        .with_encoder_config(enc)
        .with_threads(settings.threads);
    let mut ctx: Context<u8> = cfg.new_context()?;

    let (cw, ch) = ((size.0 as usize).div_ceil(2), (size.1 as usize).div_ceil(2));
    let mut bytes = 0usize;
    let mut sse = [0f64; 3];
    let mut counts = [0usize; 3];
    let mut latency_frames = 0usize;
    let mut first_packet_seen = false;
    // PSNR accumulation is measurement overhead, not encode work: 3.1M
    // samples per 1080p frame is tens of milliseconds against a ~100 ms
    // encode. Time it separately and subtract, or the fps column measures
    // this example instead of the encoder.
    let mut quality_time = Duration::ZERO;

    let start = Instant::now();
    for (index, frame) in frames.iter().enumerate() {
        let mut rav1e_frame = ctx.new_frame();
        copy_plane(
            &mut rav1e_frame.planes[0],
            &frame.planes[0],
            frame.strides[0],
            size.0 as usize,
            size.1 as usize,
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
        ctx.send_frame(rav1e_frame)?;

        loop {
            match ctx.receive_packet() {
                Ok(packet) => {
                    if !first_packet_seen {
                        first_packet_seen = true;
                        latency_frames = index + 1;
                    }
                    bytes += packet.data.len();
                    if measure_quality {
                        let quality_start = Instant::now();
                        accumulate(&packet, &mut sse, &mut counts);
                        quality_time += quality_start.elapsed();
                    }
                }
                Err(EncoderStatus::Encoded) => continue,
                Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => break,
                Err(e) => return Err(format!("rav1e: {e:?}").into()),
            }
        }
    }

    ctx.flush();
    loop {
        match ctx.receive_packet() {
            Ok(packet) => {
                bytes += packet.data.len();
                if measure_quality {
                    let quality_start = Instant::now();
                    accumulate(&packet, &mut sse, &mut counts);
                    quality_time += quality_start.elapsed();
                }
            }
            Err(EncoderStatus::Encoded) => continue,
            Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => break,
            Err(e) => return Err(format!("rav1e: {e:?}").into()),
        }
    }
    let elapsed = start.elapsed().saturating_sub(quality_time);

    let media_seconds = frames.len() as f64 / f64::from(FPS);
    Ok(Measurement {
        label: label.to_string(),
        settings,
        fps: frames.len() as f64 / elapsed.as_secs_f64(),
        kbit_s: bytes as f64 * 8.0 / media_seconds / 1000.0,
        psnr: [
            psnr(sse[0], counts[0]),
            psnr(sse[1], counts[1]),
            psnr(sse[2], counts[2]),
        ],
        latency_frames: if first_packet_seen {
            latency_frames
        } else {
            frames.len()
        },
    })
}

/// Every field is spelled out on purpose. `..Default::default()` would make
/// each row depend on whatever the shipping default happens to be, and a
/// sweep whose baseline moves under it measures nothing.
fn settings(speed: u8, tiles: usize, low_latency: bool) -> Av1Settings {
    Av1Settings {
        speed,
        tiles,
        low_latency,
        threads: 0,
    }
}

fn candidates() -> Vec<(&'static str, Av1Settings)> {
    vec![
        ("s8 t4 low-latency (was shipping)", settings(8, 4, true)),
        ("s8 t8 low-latency", settings(8, 8, true)),
        ("s8 t16 low-latency", settings(8, 16, true)),
        ("s8 t32 low-latency", settings(8, 32, true)),
        ("s8 t64 low-latency", settings(8, 64, true)),
        ("s9 t16 low-latency", settings(9, 16, true)),
        ("s9 t32 low-latency", settings(9, 32, true)),
        ("s9 t64 low-latency", settings(9, 64, true)),
        ("s10 t16 low-latency", settings(10, 16, true)),
        ("s10 t32 low-latency", settings(10, 32, true)),
        ("s8 t16 pipelined", settings(8, 16, false)),
        ("s9 t32 pipelined", settings(9, 32, false)),
    ]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = std::env::args()
        .nth(1)
        .ok_or("usage: quality_av1 INPUT.h264")?;
    // Holding rav1e's `rec`/`source` frames alive to compute PSNR costs time
    // beyond the accumulation itself, so absolute fps here reads low against
    // `bench_av1`. Pass `speed` as the second argument to skip quality
    // measurement entirely and get comparable throughput numbers.
    let measure_quality = std::env::args().nth(2).as_deref() != Some("speed");
    let data = std::fs::read(&input)?;
    let frames = decode_frames(&data);
    if frames.is_empty() {
        return Err("no frames decoded".into());
    }
    let size = (frames[0].width, frames[0].height);
    println!(
        "{} frames at {}x{}, target {} kbit/s\n",
        frames.len(),
        size.0,
        size.1,
        BITRATE_1080P / 1000
    );

    let mut results = Vec::new();
    for (label, settings) in candidates() {
        let measurement = measure(label, settings, &frames, size, measure_quality)?;
        println!(
            "{:<28} {:>6.1} fps  {:>8.1} kbit/s  Y {:>6.2} dB  U {:>6.2} dB  V {:>6.2} dB  latency {:>3} frames",
            measurement.label,
            measurement.fps,
            measurement.kbit_s,
            measurement.psnr[0],
            measurement.psnr[1],
            measurement.psnr[2],
            measurement.latency_frames,
        );
        results.push(measurement);
    }

    let baseline = &results[0];
    println!("\nversus baseline ({}):", baseline.label);
    for measurement in &results[1..] {
        println!(
            "{:<28} {:>6.2}x speed  {:>+7.2} dB Y  {:>+8.1} kbit/s",
            measurement.label,
            measurement.fps / baseline.fps,
            measurement.psnr[0] - baseline.psnr[0],
            measurement.kbit_s - baseline.kbit_s,
        );
    }

    println!("\nconfigurations reaching the {MIN_ENCODE_FPS_1080P:.0} fps gate at 1080p:");
    let mut any = false;
    for measurement in &results {
        if size == (1920, 1080) && measurement.fps >= MIN_ENCODE_FPS_1080P {
            any = true;
            println!(
                "  {:<28} {:>6.1} fps, Y PSNR {:>+6.2} dB vs baseline, tiles {}, speed {}, {}",
                measurement.label,
                measurement.fps,
                measurement.psnr[0] - baseline.psnr[0],
                measurement.settings.tiles,
                measurement.settings.speed,
                if measurement.settings.low_latency {
                    "low latency"
                } else {
                    "pipelined"
                },
            );
        }
    }
    if !any {
        println!("  none");
    }
    Ok(())
}
