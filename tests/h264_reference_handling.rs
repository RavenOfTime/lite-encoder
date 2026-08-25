//! Reference-handling contract for the Tapo camera fixture and loud rejections.
//!
//! Each row in the P0 "reference handling" checklist has a test here (camera
//! exercise profile) or in `src/codec/h264/{decoder,picture,slice}.rs` (rejection
//! or implementation proof). See README § "Tapo fixture — reference handling".

use lite_encoder::codec::h264::annexb;
use lite_encoder::codec::h264::decoder::Frontend;
use lite_encoder::codec::h264::picture::{MmcoOp, RefListMod, RefMarking};

const CAMERA_FIXTURE: &[u8] = include_bytes!("fixtures/tapo-1080p-cabac-8x8.h264");

/// The checked-in Tapo stream stays inside the supported reference subset:
/// contiguous frame numbers, no weighted or redundant syntax, only reference VCL
/// NALs, and the short-term list/MMCO paths the decoder implements.
#[test]
fn tapo_fixture_uses_only_supported_reference_handling_features() {
    let mut frontend = Frontend::new();
    let mut saw_list_mod = false;
    let mut saw_mmco = false;

    for access_unit in annexb::access_units(CAMERA_FIXTURE) {
        let slices = frontend
            .parse_access_unit(access_unit)
            .expect("fixture must parse");
        for slice in slices {
            assert!(
                slice.info.nal_ref_idc > 0,
                "Tapo VCL NALs are reference pictures (nal_ref_idc > 0)"
            );
            assert_ne!(
                slice.marking,
                RefMarking::None,
                "reference NALs must not carry disposable marking"
            );
            if !slice.list_mods.is_empty() {
                assert_eq!(slice.list_mods, [RefListMod::Subtract(0)]);
                saw_list_mod = true;
            }
            if let RefMarking::Adaptive(ops) = &slice.marking {
                assert_eq!(
                    *ops,
                    vec![MmcoOp::ShortTermUnused {
                        difference_of_pic_nums_minus1: 0,
                    }]
                );
                saw_mmco = true;
            }
        }
    }

    assert!(saw_list_mod, "Tapo P slices use ref_pic_list_modification");
    assert!(saw_mmco, "Tapo P slices use adaptive MMCO-1");
}
