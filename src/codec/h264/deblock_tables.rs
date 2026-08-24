//! Deblocking filter threshold tables, ITU-T H.264 tables 8-16 and 8-17.
//!
//! Generated data, not written by hand. Extracted mechanically from the
//! openh264 reference decoder (`codec/decoder/core/src/deblocking.cpp`, Cisco
//! Systems, BSD-2-Clause), which cites the same spec tables. Same provenance
//! and same reasoning as [`super::cabac_tables`].
//!
//! openh264 pads these with twelve entries at each end so it can skip
//! clipping the index; that padding is dropped here and the index is clipped
//! explicitly, which is what the spec actually says.
//!
//! Do not edit by hand.

/// `alpha` thresholds, spec table 8-16, indexed by `indexA` (0..=51).
pub const ALPHA: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 5, 6, 7, 8, 9, 10, 12, 13, 15, 17, 20,
    22, 25, 28, 32, 36, 40, 45, 50, 56, 63, 71, 80, 90, 101, 113, 127, 144, 162, 182, 203, 226,
    255, 255,
];

/// `beta` thresholds, spec table 8-16, indexed by `indexB` (0..=51).
pub const BETA: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 6, 6, 7, 7, 8, 8,
    9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 16, 17, 17, 18, 18,
];

/// `t'C0`, spec table 8-17, indexed by `[indexA][bS - 1]`.
///
/// Only boundary strengths 1, 2 and 3 use this table; `bS == 0` means no
/// filtering and `bS == 4` uses the stronger filter, which has no `tC0`.
pub const TC0: [[u8; 3]; 52] = [
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 1],
    [0, 0, 1],
    [0, 0, 1],
    [0, 0, 1],
    [0, 1, 1],
    [0, 1, 1],
    [1, 1, 1],
    [1, 1, 1],
    [1, 1, 1],
    [1, 1, 1],
    [1, 1, 2],
    [1, 1, 2],
    [1, 1, 2],
    [1, 1, 2],
    [1, 2, 3],
    [1, 2, 3],
    [2, 2, 3],
    [2, 2, 4],
    [2, 3, 4],
    [2, 3, 4],
    [3, 3, 5],
    [3, 4, 6],
    [3, 4, 6],
    [4, 5, 7],
    [4, 5, 8],
    [4, 6, 9],
    [5, 7, 10],
    [6, 8, 11],
    [6, 8, 13],
    [7, 10, 14],
    [8, 11, 16],
    [9, 12, 18],
    [10, 13, 20],
    [11, 15, 23],
    [13, 17, 25],
];
