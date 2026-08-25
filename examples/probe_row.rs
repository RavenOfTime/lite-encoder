//! Scratch: compare one row of one frame against the reference.
#[cfg(not(feature = "reference-decoder"))]
fn main() {}

#[cfg(feature = "reference-decoder")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use lite_encoder::codec::h264::{
        annexb,
        decoder::Frontend,
        reference::{packet, ReferenceDecoder},
    };
    use lite_encoder::media::Decoder;
    use std::time::Duration;

    let a: Vec<String> = std::env::args().collect();
    if a.len() < 6 {
        return Err("usage: probe_row IN.h264 FRAME ROW X0 X1".into());
    }
    let (want, row, x0, x1): (usize, usize, usize, usize) =
        (a[2].parse()?, a[3].parse()?, a[4].parse()?, a[5].parse()?);
    let data = std::fs::read(&a[1])?;

    let mut ours = Frontend::new();
    let mut theirs = ReferenceDecoder::new()?;
    let (mut o, mut r) = (Vec::new(), Vec::new());
    for (i, au) in annexb::access_units(&data).into_iter().enumerate() {
        o.extend(ours.decode_access_unit(au, Duration::from_millis(i as u64 * 40))?);
        r.extend(theirs.decode(&packet(au, i))?);
    }
    let (o, r) = (&o[want], &r[want]);
    let rows = std::env::args().nth(6).and_then(|v| v.parse().ok()).unwrap_or(1);
    println!("frame {want} rows {row}..{}: ours / reference", row + rows);
    for row in row..row + rows {
        for x in x0..x1.min(o.width as usize) {
            let (a, b) = (
                o.planes[0][row * o.strides[0] + x],
                r.planes[0][row * r.strides[0] + x],
            );
            println!(
                "r{row:4} x{x:4}: {a:3} {b:3} {}",
                if a == b { "" } else { "<-- DIFF" }
            );
        }
    }
    Ok(())
}
