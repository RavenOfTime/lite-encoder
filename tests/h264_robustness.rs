use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

use lite_encoder::codec::h264::{annexb, decoder::Frontend};

const CAMERA_FIXTURE: &[u8] = include_bytes!("fixtures/tapo-1080p-cabac-8x8.h264");

/// Packet loss can truncate a slice at any byte. Decoder must reject it rather
/// than panic or return a partly reconstructed picture as valid output.
#[test]
fn truncated_camera_slices_fail_without_panicking_or_emitting_a_frame() {
    let access_unit = annexb::access_units(CAMERA_FIXTURE)[0];
    // The fixture has one Annex B trailing-zero byte, which is padding rather
    // than slice data and deliberately has no effect when removed.
    for removed in [2, 8, 32, 256, 1024] {
        let truncated = &access_unit[..access_unit.len() - removed];
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            Frontend::new().decode_access_unit(truncated, Duration::ZERO)
        }));
        let decoded = outcome.unwrap_or_else(|_| panic!("panicked after removing {removed} bytes"));
        assert!(
            decoded.is_err(),
            "accepted a slice truncated by {removed} bytes"
        );
    }
}
