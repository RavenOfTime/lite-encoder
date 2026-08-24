//! Macroblock types, partition geometry and quantiser derivation.
//!
//! The tables in clause 7.4.5 that turn a decoded `mb_type` number into an
//! actual instruction: what kind of prediction to run, how the macroblock is
//! carved up, and which coefficients to expect. They are separated from the
//! entropy decoder that produces the number because the same tables serve
//! CABAC and, if it ever lands, CAVLC, and because a wrong entry here is far
//! easier to find in a table test than in a decoded picture.
//!
//! # Scope
//!
//! I and P slices. B slices need a second reference list, bi-prediction, the
//! direct modes and their own `mb_type` table; the cameras in scope do not
//! emit them, and half-supporting them would be worse than refusing them.
//!
//! Coded block patterns are the CABAC form throughout: four luma bits and a
//! two-valued chroma field, exactly as coded. CAVLC would need the mapping in
//! table 9-4 to get here, which is one more reason the CAVLC path is a
//! separate concern rather than a flag inside this one.

use super::intra::Intra16x16Mode;

/// The slice types in scope. Spec table 7-6, minus B, SI and SP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceKind {
    I,
    P,
}

/// What a macroblock is, once `mb_type` has been decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MbType {
    /// `I_NxN`: sixteen 4x4 or four 8x8 intra blocks. Which of the two is
    /// decided by `transform_size_8x8_flag`, a separate syntax element, so it
    /// is deliberately not part of this value.
    IntraNxN,
    /// `I_16x16`: one prediction over the whole macroblock, with the residual
    /// shape fixed by `mb_type` rather than by a coded block pattern.
    Intra16x16 {
        mode: Intra16x16Mode,
        /// 0 or 15: whole macroblock or nothing. Spec `CodedBlockPatternLuma`.
        cbp_luma: u8,
        /// 0, 1 or 2. Spec `CodedBlockPatternChroma`.
        cbp_chroma: u8,
    },
    /// `I_PCM`: uncoded samples, byte-aligned in the bitstream.
    IPcm,
    /// A P macroblock with coded motion vectors.
    Inter(Partitioning),
    /// `P_Skip`: no residual, no coded vector, motion inferred from
    /// neighbours. The overwhelming majority of macroblocks in surveillance
    /// footage, where most of the frame does not move.
    PSkip,
}

impl MbType {
    pub fn is_intra(self) -> bool {
        matches!(
            self,
            MbType::IntraNxN | MbType::Intra16x16 { .. } | MbType::IPcm
        )
    }

    pub fn is_inter(self) -> bool {
        !self.is_intra()
    }

    /// Whether the macroblock's residual is coded as a `coded_block_pattern`
    /// syntax element.
    ///
    /// `I_16x16` carries its pattern inside `mb_type` and `I_PCM` has no
    /// residual at all, so for those the element is absent from the bitstream
    /// entirely — not present-but-zero.
    pub fn has_coded_block_pattern(self) -> bool {
        matches!(self, MbType::IntraNxN | MbType::Inter(_))
    }
}

/// How a P macroblock is divided. Spec table 7-13.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Partitioning {
    P16x16,
    P16x8,
    P8x16,
    /// Four 8x8 partitions, each with its own [`SubMbType`].
    P8x8,
    /// As `P8x8`, but every partition uses reference index 0 and no
    /// `ref_idx_l0` is coded. A bitrate optimisation, not a different shape.
    P8x8Ref0,
}

/// How one 8x8 partition of a `P_8x8` macroblock is divided. Spec table 7-17.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubMbType {
    S8x8,
    S8x4,
    S4x8,
    S4x4,
}

/// One rectangle of a macroblock, in luma samples relative to its top-left
/// corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

const fn p(x: usize, y: usize, width: usize, height: usize) -> Part {
    Part {
        x,
        y,
        width,
        height,
    }
}

/// Partition layouts, spec table 7-13. Named constants rather than array
/// literals inside the match, so that the slices returned are `'static`.
const PARTS_16X16: [Part; 1] = [p(0, 0, 16, 16)];
const PARTS_16X8: [Part; 2] = [p(0, 0, 16, 8), p(0, 8, 16, 8)];
const PARTS_8X16: [Part; 2] = [p(0, 0, 8, 16), p(8, 0, 8, 16)];
const PARTS_8X8: [Part; 4] = [p(0, 0, 8, 8), p(8, 0, 8, 8), p(0, 8, 8, 8), p(8, 8, 8, 8)];

/// Sub-partition layouts, spec table 7-17, relative to their 8x8 partition.
const SUB_PARTS_8X8: [Part; 1] = [p(0, 0, 8, 8)];
const SUB_PARTS_8X4: [Part; 2] = [p(0, 0, 8, 4), p(0, 4, 8, 4)];
const SUB_PARTS_4X8: [Part; 2] = [p(0, 0, 4, 8), p(4, 0, 4, 8)];
const SUB_PARTS_4X4: [Part; 4] = [p(0, 0, 4, 4), p(4, 0, 4, 4), p(0, 4, 4, 4), p(4, 4, 4, 4)];

impl Partitioning {
    /// The macroblock partitions, in the order their syntax elements appear.
    ///
    /// `P8x8` and `P8x8Ref0` return the four 8x8 quadrants; each is then
    /// subdivided again according to its own [`SubMbType`].
    pub fn parts(self) -> &'static [Part] {
        match self {
            Partitioning::P16x16 => &PARTS_16X16,
            Partitioning::P16x8 => &PARTS_16X8,
            Partitioning::P8x16 => &PARTS_8X16,
            Partitioning::P8x8 | Partitioning::P8x8Ref0 => &PARTS_8X8,
        }
    }

    /// Whether `ref_idx_l0` is coded for each partition.
    ///
    /// False for `P8x8Ref0`, whose whole purpose is to leave it out.
    pub fn codes_ref_idx(self) -> bool {
        self != Partitioning::P8x8Ref0
    }

    /// Whether the macroblock subdivides into [`SubMbType`] partitions.
    pub fn has_sub_partitions(self) -> bool {
        matches!(self, Partitioning::P8x8 | Partitioning::P8x8Ref0)
    }

    /// The tag motion vector prediction needs for a given partition index.
    ///
    /// The 16x8 and 8x16 shapes override the median rule with a directional
    /// one; everything else, including each 8x8 quadrant, uses the median.
    /// Spec 8.4.1.3, and see [`super::inter::predict_mv`].
    pub fn mv_prediction(self, part: usize) -> super::inter::Partition {
        use super::inter::Partition as P;
        match (self, part) {
            (Partitioning::P16x8, 0) => P::Top16x8,
            (Partitioning::P16x8, _) => P::Bottom16x8,
            (Partitioning::P8x16, 0) => P::Left8x16,
            (Partitioning::P8x16, _) => P::Right8x16,
            _ => P::Other,
        }
    }
}

impl SubMbType {
    /// The sub-partitions, relative to the top-left corner of the 8x8
    /// partition that contains them.
    pub fn parts(self) -> &'static [Part] {
        match self {
            SubMbType::S8x8 => &SUB_PARTS_8X8,
            SubMbType::S8x4 => &SUB_PARTS_8X4,
            SubMbType::S4x8 => &SUB_PARTS_4X8,
            SubMbType::S4x4 => &SUB_PARTS_4X4,
        }
    }
}

/// Interprets a decoded `mb_type` value for the given slice type.
///
/// Returns `None` for values outside the tables, which in a well-formed
/// stream cannot happen and in a damaged one must not be guessed at.
pub fn mb_type(slice: SliceKind, value: u32) -> Option<MbType> {
    match slice {
        SliceKind::I => intra_mb_type(value),
        // Spec table 7-13. Values from 5 up are the intra table shifted, so a
        // P slice can code an intra macroblock without a second escape.
        SliceKind::P => match value {
            0 => Some(MbType::Inter(Partitioning::P16x16)),
            1 => Some(MbType::Inter(Partitioning::P16x8)),
            2 => Some(MbType::Inter(Partitioning::P8x16)),
            3 => Some(MbType::Inter(Partitioning::P8x8)),
            4 => Some(MbType::Inter(Partitioning::P8x8Ref0)),
            _ => intra_mb_type(value - 5),
        },
    }
}

/// Spec table 7-11: the 26 I macroblock types.
fn intra_mb_type(value: u32) -> Option<MbType> {
    match value {
        0 => Some(MbType::IntraNxN),
        25 => Some(MbType::IPcm),
        // The 24 I_16x16 types are a packed product of three fields rather
        // than an arbitrary list: prediction mode cycles fastest, then chroma
        // pattern, and the upper half of the range is the luma-coded half.
        1..=24 => {
            let n = value - 1;
            Some(MbType::Intra16x16 {
                mode: match n % 4 {
                    0 => Intra16x16Mode::Vertical,
                    1 => Intra16x16Mode::Horizontal,
                    2 => Intra16x16Mode::Dc,
                    _ => Intra16x16Mode::Plane,
                },
                cbp_luma: if n >= 12 { 15 } else { 0 },
                cbp_chroma: (n / 4 % 3) as u8,
            })
        }
        _ => None,
    }
}

/// Spec table 7-17: `sub_mb_type` in a P slice.
pub fn sub_mb_type(value: u32) -> Option<SubMbType> {
    Some(match value {
        0 => SubMbType::S8x8,
        1 => SubMbType::S8x4,
        2 => SubMbType::S4x8,
        3 => SubMbType::S4x4,
        _ => return None,
    })
}

/// Spec 7.4.5: applies `mb_qp_delta` to the running slice quantiser.
///
/// The wrap is not a clamp. `mb_qp_delta` has range -26..25 and the result is
/// taken modulo 52, so a delta that runs off one end of the range reappears
/// at the other. Encoders rely on it to code a large quantiser change in few
/// bits, so clamping instead would decode real streams incorrectly.
pub fn next_qp(prev: u8, delta: i32) -> u8 {
    ((prev as i32 + delta + 52) % 52) as u8
}

/// Spec table 8-15: the chroma quantiser for a given luma quantiser.
///
/// Chroma is quantised more coarsely than luma below QP 30 only in the sense
/// that it tracks it exactly; above 30 the mapping flattens out, so a large
/// luma quantiser costs chroma much less quality than it costs luma. That
/// flattening is why the table exists rather than the identity.
pub fn chroma_qp(qp_y: u8, offset: i32) -> u8 {
    /// `QPC` for `qPI` values 30..=51; below 30 the mapping is the identity.
    const MAPPED: [u8; 22] = [
        29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
    ];

    let qpi = (qp_y as i32 + offset).clamp(0, 51) as u8;
    if qpi < 30 {
        qpi
    } else {
        MAPPED[qpi as usize - 30]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_i_slice_table_ends_are_the_two_special_cases() {
        assert_eq!(mb_type(SliceKind::I, 0), Some(MbType::IntraNxN));
        assert_eq!(mb_type(SliceKind::I, 25), Some(MbType::IPcm));
        assert_eq!(mb_type(SliceKind::I, 26), None);
    }

    /// The 24 I_16x16 types pack three fields; check the corners of that
    /// product rather than every entry, since a packing error shows up at the
    /// boundaries.
    #[test]
    fn the_intra_16x16_types_unpack_mode_and_pattern() {
        assert_eq!(
            mb_type(SliceKind::I, 1),
            Some(MbType::Intra16x16 {
                mode: Intra16x16Mode::Vertical,
                cbp_luma: 0,
                cbp_chroma: 0,
            })
        );
        // Mode cycles fastest.
        assert_eq!(
            mb_type(SliceKind::I, 4),
            Some(MbType::Intra16x16 {
                mode: Intra16x16Mode::Plane,
                cbp_luma: 0,
                cbp_chroma: 0,
            })
        );
        // Then chroma pattern.
        assert_eq!(
            mb_type(SliceKind::I, 5),
            Some(MbType::Intra16x16 {
                mode: Intra16x16Mode::Vertical,
                cbp_luma: 0,
                cbp_chroma: 1,
            })
        );
        // The upper half of the range is the luma-coded half.
        assert_eq!(
            mb_type(SliceKind::I, 13),
            Some(MbType::Intra16x16 {
                mode: Intra16x16Mode::Vertical,
                cbp_luma: 15,
                cbp_chroma: 0,
            })
        );
        assert_eq!(
            mb_type(SliceKind::I, 24),
            Some(MbType::Intra16x16 {
                mode: Intra16x16Mode::Plane,
                cbp_luma: 15,
                cbp_chroma: 2,
            })
        );
    }

    #[test]
    fn every_intra_16x16_type_has_a_legal_chroma_pattern() {
        for value in 1..=24 {
            let Some(MbType::Intra16x16 { cbp_chroma, .. }) = mb_type(SliceKind::I, value) else {
                panic!("value {value} is not an I_16x16 type");
            };
            assert!(cbp_chroma <= 2, "value {value} gave chroma {cbp_chroma}");
        }
    }

    #[test]
    fn p_slice_values_below_five_are_the_inter_shapes() {
        let shapes = [
            Partitioning::P16x16,
            Partitioning::P16x8,
            Partitioning::P8x16,
            Partitioning::P8x8,
            Partitioning::P8x8Ref0,
        ];
        for (value, shape) in shapes.into_iter().enumerate() {
            assert_eq!(
                mb_type(SliceKind::P, value as u32),
                Some(MbType::Inter(shape))
            );
        }
    }

    /// A P slice reaches the intra table by an offset of five, so every I
    /// type must be reachable and land on the same thing.
    #[test]
    fn p_slice_values_from_five_up_are_the_intra_table_shifted() {
        for value in 0..=25 {
            assert_eq!(
                mb_type(SliceKind::P, value + 5),
                mb_type(SliceKind::I, value),
                "intra type {value}"
            );
        }
        assert_eq!(mb_type(SliceKind::P, 31), None);
    }

    #[test]
    fn partitions_tile_the_macroblock_exactly() {
        for shape in [
            Partitioning::P16x16,
            Partitioning::P16x8,
            Partitioning::P8x16,
            Partitioning::P8x8,
        ] {
            let covered: usize = shape.parts().iter().map(|p| p.width * p.height).sum();
            assert_eq!(covered, 256, "{shape:?} does not cover the macroblock");
            for part in shape.parts() {
                assert!(
                    part.x + part.width <= 16 && part.y + part.height <= 16,
                    "{part:?}"
                );
            }
        }
    }

    #[test]
    fn sub_partitions_tile_their_eight_by_eight_partition_exactly() {
        for sub in [
            SubMbType::S8x8,
            SubMbType::S8x4,
            SubMbType::S4x8,
            SubMbType::S4x4,
        ] {
            let covered: usize = sub.parts().iter().map(|p| p.width * p.height).sum();
            assert_eq!(covered, 64, "{sub:?} does not cover the partition");
            for part in sub.parts() {
                assert!(
                    part.x + part.width <= 8 && part.y + part.height <= 8,
                    "{part:?}"
                );
            }
        }
    }

    #[test]
    fn only_the_split_shapes_use_directional_vector_prediction() {
        use super::super::inter::Partition as P;
        assert_eq!(Partitioning::P16x8.mv_prediction(0), P::Top16x8);
        assert_eq!(Partitioning::P16x8.mv_prediction(1), P::Bottom16x8);
        assert_eq!(Partitioning::P8x16.mv_prediction(0), P::Left8x16);
        assert_eq!(Partitioning::P8x16.mv_prediction(1), P::Right8x16);
        assert_eq!(Partitioning::P16x16.mv_prediction(0), P::Other);
        for part in 0..4 {
            assert_eq!(Partitioning::P8x8.mv_prediction(part), P::Other);
        }
    }

    #[test]
    fn ref_zero_partitioning_codes_no_reference_index() {
        assert!(!Partitioning::P8x8Ref0.codes_ref_idx());
        assert!(Partitioning::P8x8.codes_ref_idx());
        assert!(Partitioning::P8x8Ref0.has_sub_partitions());
    }

    #[test]
    fn only_i_nxn_and_inter_macroblocks_code_a_block_pattern() {
        assert!(MbType::IntraNxN.has_coded_block_pattern());
        assert!(MbType::Inter(Partitioning::P16x16).has_coded_block_pattern());
        assert!(!MbType::IPcm.has_coded_block_pattern());
        assert!(!MbType::PSkip.has_coded_block_pattern());
        let i16 = mb_type(SliceKind::I, 13).unwrap();
        assert!(!i16.has_coded_block_pattern());
    }

    #[test]
    fn intra_and_inter_classification_covers_every_type() {
        for value in 0..=25 {
            assert!(mb_type(SliceKind::I, value).unwrap().is_intra());
        }
        for value in 0..5 {
            assert!(mb_type(SliceKind::P, value).unwrap().is_inter());
        }
        assert!(MbType::PSkip.is_inter());
    }

    /// The quantiser wraps rather than clamping, and encoders depend on it.
    #[test]
    fn the_quantiser_delta_wraps_modulo_fifty_two() {
        assert_eq!(next_qp(26, 0), 26);
        assert_eq!(next_qp(26, 5), 31);
        assert_eq!(next_qp(26, -5), 21);
        // Off the top and off the bottom, both reappearing at the far end.
        assert_eq!(next_qp(50, 4), 2);
        assert_eq!(next_qp(2, -4), 50);
        // The extremes of the legal delta range stay in range.
        for prev in 0..52u8 {
            for delta in -26..=25 {
                assert!(next_qp(prev, delta) < 52);
            }
        }
    }

    #[test]
    fn chroma_tracks_luma_below_thirty_and_flattens_above_it() {
        for qp in 0..30u8 {
            assert_eq!(chroma_qp(qp, 0), qp);
        }
        assert_eq!(chroma_qp(30, 0), 29);
        assert_eq!(chroma_qp(39, 0), 35);
        assert_eq!(chroma_qp(51, 0), 39);
        // Monotonic, and never above the luma quantiser it came from.
        let mut prev = 0;
        for qp in 0..52u8 {
            let c = chroma_qp(qp, 0);
            assert!(c >= prev && c <= qp, "qp {qp} gave {c}");
            prev = c;
        }
    }

    #[test]
    fn the_chroma_offset_is_applied_before_the_table_and_clamped() {
        assert_eq!(chroma_qp(30, -2), 28);
        assert_eq!(chroma_qp(28, 2), 29);
        // Offsets have range -12..12 and must not push the index out of the
        // table at either end.
        for qp in 0..52u8 {
            for offset in -12..=12 {
                assert!(chroma_qp(qp, offset) < 52);
            }
        }
    }
}
