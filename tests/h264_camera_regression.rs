//! Camera-fixture regression for the pure-Rust H.264 decoder.
//!
//! The checked-in stream is documented in `tests/fixtures/README.md`. These
//! tests pin the contract CI must keep green: four 1920×1080 pictures, and
//! (with the reference feature) bit-exact agreement with OpenH264.

use std::time::Duration;

use lite_encoder::codec::h264::decoder::Frontend;

/// Bytes of `tests/fixtures/tapo-1080p-cabac-8x8.h264`.
const CAMERA_FIXTURE: &[u8] = include_bytes!("fixtures/tapo-1080p-cabac-8x8.h264");

/// Expected picture count for the committed fixture. Changing this means the
/// fixture file itself changed and the README must be updated in lockstep.
const EXPECTED_PICTURES: usize = 4;

const EXPECTED_WIDTH: u32 = 1920;
const EXPECTED_HEIGHT: u32 = 1080;

/// Pure-Rust decode of the camera fixture: no C oracle, runs on every
/// `cargo test`. Guards picture count and display size so a silent crop or
/// early-exit bug cannot land without a red CI cell.
#[test]
fn camera_fixture_decodes_exactly_four_1080p_pictures() {
    let mut decoder = Frontend::new();
    let mut frames = Vec::new();
    for (i, au) in lite_encoder::codec::h264::annexb::access_units(CAMERA_FIXTURE)
        .into_iter()
        .enumerate()
    {
        frames.extend(
            decoder
                .decode_access_unit(au, Duration::from_millis(i as u64 * 40))
                .unwrap_or_else(|e| panic!("decode failed on access unit {i}: {e}")),
        );
    }

    assert_eq!(
        frames.len(),
        EXPECTED_PICTURES,
        "camera fixture picture count drifted; update tests/fixtures/README.md"
    );
    for (i, frame) in frames.iter().enumerate() {
        assert_eq!(
            (frame.width, frame.height),
            (EXPECTED_WIDTH, EXPECTED_HEIGHT),
            "picture {i} display size"
        );
        assert_eq!(
            frame.concealed_macroblocks, 0,
            "picture {i} should be fully covered by slices"
        );
    }
}

/// Bit-exact lock against OpenH264 for the same four pictures. Behind the
/// reference-decoder feature because it needs the vendored C toolchain.
#[cfg(feature = "reference-decoder")]
#[test]
fn camera_fixture_matches_openh264_bit_exactly() {
    use lite_encoder::codec::h264::decoder::Frontend;
    use lite_encoder::codec::h264::differential;

    let mut subject = Frontend::new();
    let report = differential::compare(CAMERA_FIXTURE, &mut subject).expect("compare");

    assert_eq!(report.reference_frames, EXPECTED_PICTURES);
    assert_eq!(report.subject_frames, EXPECTED_PICTURES);
    assert!(
        report.matches(),
        "camera fixture diverged from OpenH264: {report}"
    );
    assert_eq!(
        report.to_string(),
        format!("{EXPECTED_PICTURES} frames match exactly")
    );
}
