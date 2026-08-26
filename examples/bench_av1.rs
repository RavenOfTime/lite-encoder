//! Times AV1 encode over decoded H.264 camera frames.
//!
//! Drives the shipping [`Av1Encoder`] over a sweep of speed/tile settings on
//! frames produced by the pure-Rust H.264 decoder. Every row spells its
//! settings out, and the bitrate matches `quality_av1`, so the two tables
//! measure the same encoder. Throughput is a product property:
//! `bench_h264` reserves half the wall-clock at 30 fps for decode; this
//! example measures whether encode fits in the other half.
//!
//! # Acceptance gate
//!
//! Continuous 1080p recording at 22 fps needs encode to sustain **1.0× real
//! time** — **22 fps** — on a release build over a long (≥200 picture)
//! capture. The checked-in four-picture fixture is too short for the gate:
//! keyframe and warm-up cost dominate.
//!
//! Run with:
//! `cargo run --release --features av1 --example bench_av1 -- INPUT.h264`

use std::time::{Duration, Instant};

use lite_encoder::codec::av1::{Av1Encoder, Av1Settings};
use lite_encoder::codec::h264::{annexb, decoder::Frontend};
use lite_encoder::media::{Encoder, Frame, TrackId};

/// Minimum acceptable encode fps at 1080p (1.0× real time at 22 fps).
const MIN_ENCODE_FPS_1080P: f64 = 22.0;

/// Product frame rate for gate reporting, PTS spacing, and keyframe interval.
const FPS: u32 = 22;

/// Default AV1 bitrate for 1080p surveillance (bits per second).
const BITRATE_1080P: i32 = 1_000_000;

type BenchResult = Result<(Vec<Frame>, (u32, u32)), Box<dyn std::error::Error>>;

fn decode_all(data: &[u8]) -> BenchResult {
    let access_units = annexb::access_units(data);
    let mut decoder = Frontend::new();
    let mut frames = Vec::new();
    let mut size = (0, 0);
    let frame_period = Duration::from_millis(1_000 / u64::from(FPS));
    for (i, au) in access_units.iter().enumerate() {
        for frame in decoder.decode_access_unit(au, frame_period * i as u32)? {
            size = (frame.width, frame.height);
            frames.push(frame);
        }
    }
    Ok((frames, size))
}

fn encode_all(
    frames: &[Frame],
    size: (u32, u32),
    settings: Av1Settings,
) -> Result<(Duration, usize, usize, usize), Box<dyn std::error::Error>> {
    let mut enc =
        Av1Encoder::with_settings(TrackId(0), size.0, size.1, FPS, BITRATE_1080P, settings)?;
    let start = Instant::now();
    let mut packets = 0usize;
    let mut bytes = 0usize;
    let mut first_packet_after = None;
    for (index, frame) in frames.iter().enumerate() {
        let output = enc.encode(frame)?;
        if first_packet_after.is_none() && !output.is_empty() {
            first_packet_after = Some(index + 1);
        }
        packets += output.len();
        bytes += output.iter().map(|packet| packet.data.len()).sum::<usize>();
    }
    let output = enc.flush()?;
    packets += output.len();
    bytes += output.iter().map(|packet| packet.data.len()).sum::<usize>();
    Ok((
        start.elapsed(),
        packets,
        bytes,
        first_packet_after.unwrap_or(frames.len()),
    ))
}

/// Every field is spelled out on purpose, exactly as in `quality_av1`.
/// `..Av1Settings::default()` would make each row track whatever the shipping
/// default happens to be, so a row read as "speed 8" would silently become
/// speed 9 the moment the default moved. Earlier sweeps were invalidated that
/// way.
fn settings(speed: u8, tiles: usize, low_latency: bool) -> Av1Settings {
    Av1Settings {
        speed,
        tiles,
        low_latency,
        threads: 0,
    }
}

/// True when a swept row is the configuration `Av1Encoder::new` would pick.
/// `Av1Settings` is deliberately not `PartialEq`-derived here so the sweep
/// keeps compiling if a field is added; the three fields that change the
/// encode are compared explicitly.
fn is_shipping_default(settings: Av1Settings) -> bool {
    let shipping = Av1Settings::default();
    settings.speed == shipping.speed
        && settings.tiles == shipping.tiles
        && settings.low_latency == shipping.low_latency
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = std::env::args()
        .nth(1)
        .ok_or("usage: bench_av1 INPUT.h264 [passes]")?;
    let passes: usize = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let speed_only = std::env::args().nth(3).as_deref() == Some("speed");

    let data = std::fs::read(&input)?;
    let access_units = annexb::access_units(&data);
    println!("{} access units, {} bytes", access_units.len(), data.len());

    println!("decoding (not gated)...");
    let decode_start = Instant::now();
    let (frames, size) = decode_all(&data)?;
    let decode_elapsed = decode_start.elapsed();
    if frames.is_empty() {
        return Err("no frames decoded".into());
    }
    println!(
        "{} frames at {}x{} in {:?} ({:.1} fps)",
        frames.len(),
        size.0,
        size.1,
        decode_elapsed,
        frames.len() as f64 / decode_elapsed.as_secs_f64()
    );

    println!("\nspeed  tiles  latency  threads      fps    kbit/s  first-packet");
    let mut best_fps = 0.0f64;
    let mut best_settings = Av1Settings::default();
    let mut shipping_fps: Option<f64> = None;
    let all_candidates = [
        settings(8, 4, true),
        settings(8, 16, true),
        settings(8, 32, true),
        settings(9, 16, true),
        settings(9, 32, true),
        settings(10, 32, true),
        settings(8, 16, false),
        settings(9, 32, false),
    ];
    let candidates: &[Av1Settings] = if speed_only {
        &all_candidates[3..6]
    } else {
        &all_candidates
    };
    for &settings in candidates {
        let mut best = Duration::MAX;
        let mut best_bytes = 0usize;
        let mut first_packet_after = 0usize;
        for _ in 0..passes {
            let (elapsed, _packets, bytes, delay) = encode_all(&frames, size, settings)?;
            if elapsed < best {
                best = elapsed;
                best_bytes = bytes;
                first_packet_after = delay;
            }
        }
        let fps = frames.len() as f64 / best.as_secs_f64();
        let media_seconds = frames.len() as f64 / f64::from(FPS);
        let kbit_s = best_bytes as f64 * 8.0 / media_seconds / 1000.0;
        println!(
            "{:>5}  {:>5}  {:>7}  {:>7}  {:>7.1}  {:>8.1}  {:>12}",
            settings.speed,
            settings.tiles,
            if settings.low_latency {
                "low"
            } else {
                "normal"
            },
            settings.threads,
            fps,
            kbit_s,
            first_packet_after
        );
        if fps > best_fps {
            best_fps = fps;
            best_settings = settings;
        }
        if is_shipping_default(settings) {
            shipping_fps = Some(fps);
        }
    }

    println!(
        "\nbest candidate: {:.1} fps at {}x{} (speed {}, tiles {}, {} latency)",
        best_fps,
        size.0,
        size.1,
        best_settings.speed,
        best_settings.tiles,
        if best_settings.low_latency {
            "low"
        } else {
            "normal"
        },
    );
    for target in [22.0, 25.0, 30.0] {
        println!("  {:.2}x real time at {target} fps", best_fps / target);
    }

    let gate_applies = size == (1920, 1080) && frames.len() >= 200;
    if gate_applies {
        // Gate the configuration the product actually ships, not the fastest
        // row in the sweep: the fastest row is routinely one the quality
        // sweep already rejected, and gating on it reports PASS for an encode
        // nobody runs.
        let (gated_fps, gated_label) = match shipping_fps {
            Some(fps) => (fps, "shipping default"),
            None => (best_fps, "best candidate (shipping default not swept)"),
        };
        let margin = gated_fps / MIN_ENCODE_FPS_1080P;
        println!(
            "\nacceptance gate: {:.0} fps min encode at 1080p (1.0× @ 22 fps), {gated_label} at {gated_fps:.1} fps",
            MIN_ENCODE_FPS_1080P
        );
        if gated_fps >= MIN_ENCODE_FPS_1080P {
            println!("  PASS ({margin:.2}× the floor)");
        } else {
            println!("  FAIL ({margin:.2}× the floor)");
            std::process::exit(1);
        }
    } else if size == (1920, 1080) {
        println!(
            "\n(skipping acceptance gate: need ≥200 pictures for a stable rate; got {})",
            frames.len()
        );
    }

    Ok(())
}
