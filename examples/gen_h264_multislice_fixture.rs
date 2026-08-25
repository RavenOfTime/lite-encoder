use std::path::PathBuf;

use lite_encoder::codec::h264::reference::synthesize_with_slices;

fn main() {
    let output = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: gen_h264_multislice_fixture <output.h264>");
    let stream = synthesize_with_slices(256, 128, 3, 3, 2).expect("encode fixture");
    std::fs::write(&output, stream.annexb).expect("write fixture");
    println!(
        "wrote {}x{} fixture to {}",
        stream.width,
        stream.height,
        output.display()
    );
}
