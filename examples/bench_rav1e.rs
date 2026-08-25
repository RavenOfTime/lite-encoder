//! Measure whether rav1e can encode the camera's substream in real time.
//!
//! The recording job is supervised and long-running, so the encoder has to keep
//! up with the camera indefinitely: 640x360 at 25 fps means one frame every
//! 40 ms. This example encodes synthetic frames of that shape across a range of
//! speed presets and reports the achieved frame rate, so the transcode design
//! can be gated on a measurement instead of an assumption.
//!
//! Run with:  cargo run --release --features av1 --example bench_rav1e

use std::time::Instant;

use rav1e::config::SpeedSettings;
use rav1e::prelude::*;

const WIDTH: usize = 640;
const HEIGHT: usize = 360;
const FPS: u64 = 25;
const FRAMES: usize = 150; // 6 seconds of video
const BITRATE: i32 = 60_000; // bits per second, the AV1 target from the storage math

/// Synthetic 4:2:0 content with motion, so the encoder does real work rather
/// than collapsing a static image into near-empty frames.
fn fill_frame(frame: &mut Frame<u8>, n: usize) {
    let t = n as i32;
    let y = &mut frame.planes[0];
    let (stride, rows) = (y.cfg.stride, y.cfg.height);
    for row in 0..rows {
        let line = &mut y.data_origin_mut()[row * stride..][..stride.min(WIDTH)];
        for (col, px) in line.iter_mut().enumerate() {
            // Diagonal gradient plus a moving band: compressible, but not static.
            let base = ((col as i32 + row as i32 + t * 3) & 0xff) as u8;
            let band = if (col as i32 - t * 5).rem_euclid(160) < 24 {
                90
            } else {
                0
            };
            *px = base.saturating_add(band);
        }
    }
    for plane in &mut frame.planes[1..] {
        let (stride, rows) = (plane.cfg.stride, plane.cfg.height);
        for row in 0..rows {
            let line = &mut plane.data_origin_mut()[row * stride..][..stride.min(WIDTH / 2)];
            for (col, px) in line.iter_mut().enumerate() {
                *px = ((col as i32 * 2 - t) & 0xff) as u8;
            }
        }
    }
}

fn bench(
    speed: u8,
    threads: usize,
    tiles: usize,
    low_latency: bool,
) -> Result<(f64, usize), Box<dyn std::error::Error>> {
    let enc = EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        bit_depth: 8,
        chroma_sampling: ChromaSampling::Cs420,
        time_base: Rational::new(1, FPS),
        speed_settings: SpeedSettings::from_preset(speed),
        bitrate: BITRATE,
        // A supervised recorder wants bounded latency and regular seek
        // points, not the multi-second lookahead a file encoder would use.
        low_latency,
        tiles,
        min_key_frame_interval: FPS * 2,
        max_key_frame_interval: FPS * 2,
        ..Default::default()
    };

    let cfg = Config::new().with_encoder_config(enc).with_threads(threads);
    let mut ctx: Context<u8> = cfg.new_context()?;

    let mut frames = Vec::with_capacity(FRAMES);
    for n in 0..FRAMES {
        let mut f = ctx.new_frame();
        fill_frame(&mut f, n);
        frames.push(f);
    }

    let mut bytes = 0usize;
    let start = Instant::now();
    for f in frames {
        ctx.send_frame(f)?;
        loop {
            match ctx.receive_packet() {
                Ok(pkt) => bytes += pkt.data.len(),
                Err(EncoderStatus::Encoded) => continue,
                Err(EncoderStatus::NeedMoreData) => break,
                Err(e) => return Err(format!("{e:?}").into()),
            }
        }
    }
    ctx.flush();
    loop {
        match ctx.receive_packet() {
            Ok(pkt) => bytes += pkt.data.len(),
            Err(EncoderStatus::Encoded) => continue,
            Err(EncoderStatus::LimitReached) => break,
            Err(EncoderStatus::NeedMoreData) => break,
            Err(e) => return Err(format!("{e:?}").into()),
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    Ok((FRAMES as f64 / elapsed, bytes))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    println!(
        "{WIDTH}x{HEIGHT}, {FRAMES} frames, target {FPS} fps, {} bit/s, {cores} cores available\n",
        BITRATE
    );
    println!(
        "{:>5}  {:>7}  {:>5}  {:>9}  {:>9}  {:>7}  {:>10}",
        "speed", "threads", "tiles", "latency", "fps", "xrt", "kbit/s"
    );
    for &(threads, tiles, low_latency) in &[
        (1usize, 1usize, true),
        (4, 4, true),
        (8, 8, true),
        (8, 1, false),
    ] {
        for speed in [6u8, 8, 9, 10] {
            let (fps, bytes) = bench(speed, threads, tiles, low_latency)?;
            let kbits = (bytes as f64 * 8.0) / (FRAMES as f64 / FPS as f64) / 1000.0;
            println!(
                "{speed:>5}  {threads:>7}  {tiles:>5}  {:>9}  {fps:>9.1}  {:>7.2}  {kbits:>10.1}",
                if low_latency { "low" } else { "normal" },
                fps / FPS as f64
            );
        }
    }
    Ok(())
}
