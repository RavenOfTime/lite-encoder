use std::time::Duration;

use lite_encoder::codec::h264::{annexb, decoder::Frontend};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = std::env::args()
        .nth(1)
        .ok_or("usage: decode_h264 INPUT.h264 OUTPUT.pgm")?;
    let output = std::env::args()
        .nth(2)
        .ok_or("usage: decode_h264 INPUT.h264 OUTPUT.pgm")?;
    let data = std::fs::read(input)?;
    let mut decoder = Frontend::new();
    for (index, access_unit) in annexb::access_units(&data).into_iter().enumerate() {
        let frames =
            decoder.decode_access_unit(access_unit, Duration::from_millis(index as u64 * 40))?;
        if let Some(frame) = frames.into_iter().next() {
            let mut pgm = format!("P5\n{} {}\n255\n", frame.width, frame.height).into_bytes();
            pgm.extend_from_slice(&frame.planes[0]);
            std::fs::write(output, pgm)?;
            println!("wrote {}x{} luma frame", frame.width, frame.height);
            return Ok(());
        }
    }
    Err("stream produced no picture".into())
}
