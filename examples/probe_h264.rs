//! Scratch: dump macroblock state and edge samples around a divergence.
#[cfg(not(feature = "reference-decoder"))]
fn main() {
    eprintln!("run with --features reference-decoder");
}

#[cfg(feature = "reference-decoder")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use lite_encoder::codec::h264::{
        annexb,
        decoder::Frontend,
        picture_decode::PictureDecoder,
        picture::Dpb,
        reference::{packet, ReferenceDecoder},
        slice,
    };
    use lite_encoder::media::Decoder;

    let input = std::env::args().nth(1).ok_or("usage: probe_h264 INPUT.h264")?;
    let data = std::fs::read(input)?;
    let au = annexb::access_units(&data)[0];

    let mut frontend = Frontend::new();
    let slices = frontend.parse_access_unit(au)?;
    let first = slices.first().ok_or("no slices")?;
    println!("slices={} deblocking={:?}", slices.len(), first.info.deblocking);
    println!(
        "qp={} chroma_qp_offset={:?} transform8x8={} constrained_intra={}",
        first.info.slice_qp,
        first.info.chroma_qp_offset,
        first.info.transform_8x8_enabled,
        first.info.constrained_intra
    );

    let config = first.info.picture;
    let mut picture = PictureDecoder::with_dpb(config, Dpb::new(config.max_refs, config.max_frame_num));
    for (id, s) in slices.iter().enumerate() {
        picture.decode_slice(s, id as u32)?;
    }
    for addr in 0..9 {
        let mb = picture.state.get(addr);
        println!(
            "mb {addr}: type={:?} qp={} t8x8={} cbp_luma={} cbp_chroma={} cbf_luma={:016b}",
            mb.mb_type, mb.qp, mb.transform_8x8, mb.cbp_luma, mb.cbp_chroma, mb.cbf_luma
        );
    }
    let unfiltered: Vec<u8> = picture.picture.planes[0][..128].to_vec();
    let (decoded, _) = picture.finish();

    let mut reference = ReferenceDecoder::new()?;
    let refframe = reference.decode(&packet(au, 0))?.remove(0);

    println!("\ncol: unfiltered / ours / reference   (row 0)");
    for x in 96..128 {
        let (o, r) = (decoded.planes[0][x], refframe.planes[0][x]);
        println!(
            "{x:3}: {:3} {:3} {:3} {}",
            unfiltered[x],
            o,
            r,
            if o == r { "" } else { "<-- DIFF" }
        );
    }
    Ok(())
}
