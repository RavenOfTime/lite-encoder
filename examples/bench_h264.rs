//! Times the pure-Rust H.264 decoder over an Annex B file.
//!
//! Throughput is a correctness-adjacent property here: this decoder exists to
//! feed a long-running recorder, so a stream it decodes accurately but slower
//! than real time is still a stream it cannot record.

use std::time::{Duration, Instant};

use lite_encoder::codec::h264::{annexb, decoder::Frontend};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = std::env::args()
        .nth(1)
        .ok_or("usage: bench_h264 INPUT.h264 [passes]")?;
    let passes: usize = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);

    let data = std::fs::read(input)?;
    let access_units = annexb::access_units(&data);
    println!("{} access units, {} bytes", access_units.len(), data.len());

    let mut best = Duration::MAX;
    let mut frames = 0;
    let mut size = (0, 0);
    for pass in 0..passes {
        let mut decoder = Frontend::new();
        let start = Instant::now();
        frames = 0;
        for (i, au) in access_units.iter().enumerate() {
            for frame in decoder.decode_access_unit(au, Duration::from_millis(i as u64 * 40))? {
                size = (frame.width, frame.height);
                frames += 1;
            }
        }
        let elapsed = start.elapsed();
        println!(
            "pass {pass}: {frames} frames in {:?} ({:.1} fps)",
            elapsed,
            frames as f64 / elapsed.as_secs_f64()
        );
        best = best.min(elapsed);
    }

    let fps = frames as f64 / best.as_secs_f64();
    println!(
        "\nbest: {:.1} fps at {}x{} ({:.2} ms/frame)",
        fps,
        size.0,
        size.1,
        best.as_secs_f64() * 1000.0 / frames as f64
    );
    for target in [25.0, 30.0] {
        println!("  {:.2}x real time at {target} fps", fps / target);
    }

    // The oracle's throughput is the only calibration available: it says
    // whether this number is respectable for the work involved or an order of
    // magnitude off what the same picture costs a mature implementation.
    #[cfg(feature = "reference-decoder")]
    {
        use lite_encoder::codec::h264::reference::{packet, ReferenceDecoder};
        use lite_encoder::media::Decoder;

        let mut best = Duration::MAX;
        for _ in 0..passes {
            let mut decoder = ReferenceDecoder::new()?;
            let start = Instant::now();
            for (i, au) in access_units.iter().enumerate() {
                decoder.decode(&packet(au, i))?;
            }
            best = best.min(start.elapsed());
        }
        println!(
            "\nopenh264: {:.1} fps ({:.2} ms/frame)",
            frames as f64 / best.as_secs_f64(),
            best.as_secs_f64() * 1000.0 / frames as f64
        );
    }
    Ok(())
}
