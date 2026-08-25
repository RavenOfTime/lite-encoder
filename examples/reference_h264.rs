#[cfg(not(feature = "reference-decoder"))]
fn main() {
    eprintln!("run with --features reference-decoder");
}

#[cfg(feature = "reference-decoder")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use lite_encoder::codec::h264::{
        annexb,
        reference::{packet, ReferenceDecoder},
    };
    use lite_encoder::media::Decoder;

    let input = std::env::args()
        .nth(1)
        .ok_or("usage: reference_h264 INPUT.h264 OUTPUT.pgm")?;
    let output = std::env::args()
        .nth(2)
        .ok_or("usage: reference_h264 INPUT.h264 OUTPUT.pgm")?;
    let data = std::fs::read(input)?;
    let mut decoder = ReferenceDecoder::new()?;
    for (index, access_unit) in annexb::access_units(&data).into_iter().enumerate() {
        if let Some(frame) = decoder
            .decode(&packet(access_unit, index))?
            .into_iter()
            .next()
        {
            let mut pgm = format!("P5\n{} {}\n255\n", frame.width, frame.height).into_bytes();
            pgm.extend_from_slice(&frame.planes[0]);
            std::fs::write(output, pgm)?;
            println!("wrote {}x{} luma frame", frame.width, frame.height);
            return Ok(());
        }
    }
    Err("stream produced no picture".into())
}
