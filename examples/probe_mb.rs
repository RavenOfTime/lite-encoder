//! Scratch: difference map for one macroblock.
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
    if a.len() < 5 {
        return Err("usage: probe_mb IN.h264 FRAME MBX MBY [count]".into());
    }
    let (want, mbx, mby): (usize, usize, usize) = (a[2].parse()?, a[3].parse()?, a[4].parse()?);
    let count: usize = a.get(5).and_then(|v| v.parse().ok()).unwrap_or(1);
    let data = std::fs::read(&a[1])?;

    let mut ours = Frontend::new();
    let mut theirs = ReferenceDecoder::new()?;
    let (mut o, mut r) = (Vec::new(), Vec::new());
    for (i, au) in annexb::access_units(&data).into_iter().enumerate() {
        o.extend(ours.decode_access_unit(au, Duration::from_millis(i as u64 * 40))?);
        r.extend(theirs.decode(&packet(au, i))?);
    }
    let (o, r) = (&o[want], &r[want]);

    for mb in mbx..mbx + count {
        println!("frame {want} macroblock ({mb}, {mby}) — . same, digit = |delta|");
        for y in 0..16 {
            let mut line = String::new();
            for x in 0..16 {
                let i = (mby * 16 + y) * o.strides[0] + mb * 16 + x;
                let d = o.planes[0][i].abs_diff(r.planes[0][i]);
                line.push(match d {
                    0 => '.',
                    1..=9 => (b'0' + d) as char,
                    _ => '#',
                });
                if x % 4 == 3 {
                    line.push(' ');
                }
            }
            println!("  {line}");
            if y % 4 == 3 {
                println!();
            }
        }
    }
    Ok(())
}
