//! Scratch: does openh264 accept an entropy-coding-mode change?
#[cfg(not(feature = "reference-decoder"))]
fn main() {
    eprintln!("run with --features reference-decoder");
}

#[cfg(feature = "reference-decoder")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use openh264::encoder::{Encoder, EncoderConfig, FrameRate, Profile, RateControlMode};
    use openh264::formats::YUVSlices;
    use openh264::OpenH264API;
    use openh264_sys2::{SEncParamExt, ENCODER_OPTION_SVC_ENCODE_PARAM_EXT};

    let (w, h) = (64usize, 48usize);
    let config = EncoderConfig::new()
        .profile(Profile::High)
        .max_frame_rate(FrameRate::from_hz(25.0))
        .rate_control_mode(RateControlMode::Off)
        .num_threads(1)
        .debug(false);
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config)?;

    let y = vec![128u8; w * h];
    let u = vec![128u8; w * h / 4];
    let v = vec![128u8; w * h / 4];
    fn mk<'a>(y: &'a [u8], u: &'a [u8], v: &'a [u8], w: usize, h: usize) -> YUVSlices<'a> {
        YUVSlices::new((y, u, v), (w, h), (w, w / 2, w / 2))
    }

    encoder.encode(&mk(&y, &u, &v, w, h))?;

    unsafe {
        let raw = encoder.raw_api();
        let mut p = SEncParamExt::default();
        let g = raw.get_option(
            ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
            std::ptr::from_mut(&mut p).cast(),
        );
        println!(
            "get={g} entropy={} profile={:?} loopfilter_idc={}",
            p.iEntropyCodingModeFlag, p.sSpatialLayers[0].uiProfileIdc, p.iLoopFilterDisableIdc
        );
        p.iEntropyCodingModeFlag = 1;
        let s = raw.set_option(
            ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
            std::ptr::from_mut(&mut p).cast(),
        );
        let mut q = SEncParamExt::default();
        raw.get_option(
            ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
            std::ptr::from_mut(&mut q).cast(),
        );
        println!("set={s} entropy_after={}", q.iEntropyCodingModeFlag);
        raw.force_intra_frame(true);
    }

    let bs = encoder.encode(&mk(&y, &u, &v, w, h))?;
    let data = bs.to_vec();
    let mut i = 0;
    while let Some(j) = data[i..].windows(3).position(|w| w == [0, 0, 1]) {
        let start = i + j + 3;
        let ty = data[start] & 0x1f;
        if ty == 8 {
            println!("PPS bytes: {:02x?}", &data[start..(start + 6).min(data.len())]);
        }
        println!("nal type {ty}");
        i = start;
        if i >= data.len() {
            break;
        }
    }
    Ok(())
}
