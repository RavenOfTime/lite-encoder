//! Scratch: write a synthetic CABAC fixture to a file.
#[cfg(not(feature = "reference-decoder"))]
fn main() {
    eprintln!("run with --features reference-decoder");
}

#[cfg(feature = "reference-decoder")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 6 {
        return Err("usage: gen_h264 OUT.h264 W H FRAMES GOP".into());
    }
    let stream = lite_encoder::codec::h264::reference::synthesize(
        a[2].parse()?,
        a[3].parse()?,
        a[4].parse()?,
        a[5].parse()?,
    )?;
    std::fs::write(&a[1], &stream.annexb)?;
    println!("wrote {} bytes", stream.annexb.len());
    Ok(())
}
