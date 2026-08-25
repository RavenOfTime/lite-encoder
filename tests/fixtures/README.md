# H.264 fixtures

## `tapo-1080p-cabac-8x8.h264`

Checked-in prefix of a capture from the project's Tapo test camera
(`stream1`). Used as the High-profile regression fixture: openh264 cannot
encode the 8×8 transform, so synthetic streams alone never cover that path.

| Property | Expected value |
|---|---|
| Container | Annex B elementary stream |
| Pictures | **4** (IDR then three P) |
| Display size | **1920×1080** (coded 1920×1088 with crop) |
| Profile / tools | High, CABAC, 8×8 transform, progressive 8-bit 4:2:0 |
| Size on disk | 63 616 bytes |

Regression coverage:

- Default `cargo test`: decode all four pictures at 1920×1080 without panic
  (`tests/h264_camera_regression.rs`).
- `--features reference-decoder`: bit-exact match against OpenH264 for those
  four pictures (`codec::h264::differential` and the same integration test).

The full local capture `camera.h264` (224 pictures, gitignored) is for manual
`diff_h264` / `bench_h264` only. Do not replace this fixture with that file —
CI must stay small and deterministic.
