//! Segment-parallel AV1 encode benchmark at the shipping settings.
//!
//! Decodes once, then splits frames on the existing 2-second keyframe
//! boundary (`fps * 2` = 60 frames at 30 fps). Each chunk is a self-contained
//! encode that already starts on a keyframe, so parallelising across chunks
//! adds no forced keyframes and no compression penalty versus a single
//! sequential encode of the same settings.
//!
//! For N = 1..4 this example runs up to N rav1e contexts concurrently, reports
//! aggregate fps / bytes / PSNR, and prints the PSNR delta versus N = 1 so
//! quality is measured rather than asserted.
//!
//! Run with:
//! `cargo run --release --features av1 --example bench_av1_parallel -- INPUT.h264`

use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use lite_encoder::codec::av1::Av1Settings;
use lite_encoder::codec::h264::{annexb, decoder::Frontend};
use lite_encoder::media::Frame;

use rav1e::prelude::*;

/// Assumed camera frame rate: sets the keyframe interval and bitrate window.
const FPS: u32 = 30;

/// Same 1080p surveillance bitrate as `bench_av1` / `quality_av1`.
const BITRATE_1080P: i32 = 1_000_000;

/// Shipping keyframe interval in frames (`fps * 2`). Segments cut here so
/// each parallel encoder's first frame is already a keyframe.
const SEGMENT_FRAMES: usize = (FPS as usize) * 2;

/// Shipping configuration spelled out so a default drift cannot silently
/// change what this bench measures.
fn shipping_settings() -> Av1Settings {
    Av1Settings {
        speed: 9,
        tiles: 32,
        low_latency: true,
        threads: 0,
    }
}

struct SegmentResult {
    bytes: usize,
    sse: [f64; 3],
    counts: [usize; 3],
    /// Wall time spent accumulating PSNR, excluded from encode fps.
    quality_time: Duration,
}

struct RunResult {
    workers: usize,
    fps: f64,
    bytes: usize,
    psnr: [f64; 3],
    wall: Duration,
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

/// Encodes one keyframe-aligned segment with a fresh rav1e context.
fn encode_segment(
    frames: &[Frame],
    size: (u32, u32),
    settings: Av1Settings,
) -> Result<SegmentResult, Box<dyn std::error::Error + Send + Sync>> {
    let enc = settings.encoder_config(size.0, size.1, FPS, BITRATE_1080P);
    let cfg = Config::new()
        .with_encoder_config(enc)
        .with_threads(settings.threads);
    let mut ctx: Context<u8> = cfg.new_context()?;

    let (cw, ch) = ((size.0 as usize).div_ceil(2), (size.1 as usize).div_ceil(2));
    let mut bytes = 0usize;
    let mut sse = [0f64; 3];
    let mut counts = [0usize; 3];
    let mut quality_time = Duration::ZERO;

    for frame in frames {
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
                    bytes += packet.data.len();
                    let quality_start = Instant::now();
                    accumulate(&packet, &mut sse, &mut counts);
                    quality_time += quality_start.elapsed();
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
                let quality_start = Instant::now();
                accumulate(&packet, &mut sse, &mut counts);
                quality_time += quality_start.elapsed();
            }
            Err(EncoderStatus::Encoded) => continue,
            Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => break,
            Err(e) => return Err(format!("rav1e: {e:?}").into()),
        }
    }

    Ok(SegmentResult {
        bytes,
        sse,
        counts,
        quality_time,
    })
}

fn segment_ranges(frame_count: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    while start < frame_count {
        let end = (start + SEGMENT_FRAMES).min(frame_count);
        ranges.push((start, end));
        start = end;
    }
    ranges
}

/// Encodes every segment with at most `workers` concurrent rav1e contexts.
/// Wall-clock covers the parallel encode phase only; decode is outside.
fn run_parallel(
    frames: &[Frame],
    size: (u32, u32),
    workers: usize,
) -> Result<RunResult, Box<dyn std::error::Error>> {
    let settings = shipping_settings();
    let ranges = segment_ranges(frames.len());
    let workers = workers.max(1).min(ranges.len());

    // Each worker pulls the next unfinished segment. That keeps all N contexts
    // busy for as long as there is work, instead of statically partitioning
    // uneven tail segments onto one thread.
    let next = Mutex::new(0usize);
    let mut thread_quality = vec![Duration::ZERO; workers];
    let quality_slots: Vec<Mutex<Duration>> =
        (0..workers).map(|_| Mutex::new(Duration::ZERO)).collect();

    let wall_start = Instant::now();
    let aggregated: Result<Vec<SegmentResult>, Box<dyn std::error::Error + Send + Sync>> =
        thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for slot in 0..workers {
                let next = &next;
                let ranges = &ranges;
                let quality_slot = &quality_slots[slot];
                handles.push(scope.spawn(move || {
                    let mut local = Vec::new();
                    let mut local_quality = Duration::ZERO;
                    loop {
                        let index = {
                            let mut guard = next.lock().unwrap();
                            let index = *guard;
                            if index >= ranges.len() {
                                break;
                            }
                            *guard += 1;
                            index
                        };
                        let (start, end) = ranges[index];
                        let result = encode_segment(&frames[start..end], size, settings)?;
                        local_quality += result.quality_time;
                        local.push(result);
                    }
                    *quality_slot.lock().unwrap() = local_quality;
                    Ok::<Vec<SegmentResult>, Box<dyn std::error::Error + Send + Sync>>(local)
                }));
            }

            let mut all = Vec::new();
            for handle in handles {
                all.extend(handle.join().unwrap()?);
            }
            Ok(all)
        });
    let wall = wall_start.elapsed();
    let segments = aggregated.map_err(|e| -> Box<dyn std::error::Error> { e })?;

    for (slot, quality_slot) in quality_slots.iter().enumerate() {
        thread_quality[slot] = *quality_slot.lock().unwrap();
    }
    // PSNR accumulation is measurement overhead. On the parallel critical path
    // it can extend wall-clock by at most the busiest worker's quality time.
    let quality_on_critical_path = thread_quality.into_iter().max().unwrap_or(Duration::ZERO);
    let encode_wall = wall.saturating_sub(quality_on_critical_path);

    let mut bytes = 0usize;
    let mut sse = [0f64; 3];
    let mut counts = [0usize; 3];
    for segment in &segments {
        bytes += segment.bytes;
        for plane in 0..3 {
            sse[plane] += segment.sse[plane];
            counts[plane] += segment.counts[plane];
        }
    }

    Ok(RunResult {
        workers,
        fps: frames.len() as f64 / encode_wall.as_secs_f64(),
        bytes,
        psnr: [
            psnr(sse[0], counts[0]),
            psnr(sse[1], counts[1]),
            psnr(sse[2], counts[2]),
        ],
        wall: encode_wall,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = std::env::args()
        .nth(1)
        .ok_or("usage: bench_av1_parallel INPUT.h264 [passes]")?;
    let passes: usize = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    let data = std::fs::read(&input)?;
    println!("decoding (excluded from encode timing)...");
    let decode_start = Instant::now();
    let frames = decode_frames(&data);
    let decode_elapsed = decode_start.elapsed();
    if frames.is_empty() {
        return Err("no frames decoded".into());
    }
    let size = (frames[0].width, frames[0].height);
    let ranges = segment_ranges(frames.len());
    let settings = shipping_settings();

    println!(
        "{} frames at {}x{} in {:?} ({:.1} decode fps)",
        frames.len(),
        size.0,
        size.1,
        decode_elapsed,
        frames.len() as f64 / decode_elapsed.as_secs_f64()
    );
    println!(
        "shipping: speed {}, tiles {}, low_latency {}, bitrate {} bit/s",
        settings.speed, settings.tiles, settings.low_latency, BITRATE_1080P
    );
    println!(
        "segment length {} frames (keyframe interval); {} segments: {:?}",
        SEGMENT_FRAMES,
        ranges.len(),
        ranges
            .iter()
            .map(|(start, end)| format!("{start}..{end}"))
            .collect::<Vec<_>>()
    );
    println!();
    println!(
        "{:>7}  {:>7}  {:>10}  {:>8}  {:>8}  {:>8}  {:>8}",
        "workers", "fps", "bytes", "Y dB", "U dB", "V dB", "wall"
    );

    let mut baseline_psnr = None;
    for workers in 1..=4 {
        let mut best: Option<RunResult> = None;
        for _ in 0..passes {
            let run = run_parallel(&frames, size, workers)?;
            best = Some(match best {
                Some(prev) if prev.fps >= run.fps => prev,
                _ => run,
            });
        }
        let run = best.expect("at least one pass");
        if baseline_psnr.is_none() {
            baseline_psnr = Some(run.psnr);
        }
        let baseline = baseline_psnr.unwrap();
        println!(
            "{:>7}  {:>7.1}  {:>10}  {:>8.2}  {:>8.2}  {:>8.2}  {:>8.2?}",
            run.workers, run.fps, run.bytes, run.psnr[0], run.psnr[1], run.psnr[2], run.wall
        );
        println!(
            "         vs N=1 PSNR: Y {:+.3} dB  U {:+.3} dB  V {:+.3} dB",
            run.psnr[0] - baseline[0],
            run.psnr[1] - baseline[1],
            run.psnr[2] - baseline[2],
        );
    }

    println!(
        "\nNote: this partitions a finished decode. A live recorder cannot wait\n\
         for 60-frame segments before starting encode; see the accompanying report."
    );
    Ok(())
}
