#[cfg(not(feature = "reference-decoder"))]
fn main() {
    eprintln!("run with --features reference-decoder");
}

#[cfg(feature = "reference-decoder")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use lite_encoder::codec::h264::{decoder::Frontend, differential::compare};

    let input = std::env::args().nth(1).ok_or("usage: diff_h264 INPUT.h264")?;
    let data = std::fs::read(input)?;
    let mut subject = Frontend::new();
    let report = compare(&data, &mut subject)?;
    println!("{report}");
    println!("frames: reference {} subject {}", report.reference_frames, report.subject_frames);
    if let Some(d) = report.divergence {
        println!("divergence: {d}");
    } else {
        println!("divergence: none");
    }
    Ok(())
}
