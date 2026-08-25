//! Scratch: decode one access unit with tracing.
#[cfg(not(feature = "reference-decoder"))]
fn main() {}

#[cfg(feature = "reference-decoder")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use lite_encoder::codec::h264::{annexb, decoder::Frontend};
    use std::time::Duration;
    let input = std::env::args().nth(1).ok_or("usage: probe_au IN.h264 N")?;
    let want: usize = std::env::args().nth(2).unwrap_or("0".into()).parse()?;
    let data = std::fs::read(input)?;
    let mut f = Frontend::new();
    for (i, au) in annexb::access_units(&data).into_iter().enumerate() {
        if i == want {
            eprintln!("--- access unit {i} ---");
        }
        let r = f.decode_access_unit(au, Duration::from_millis(i as u64 * 40));
        if let Err(e) = r {
            eprintln!("au {i} failed: {e}");
            break;
        }
        if i >= want {
            break;
        }
    }
    Ok(())
}
