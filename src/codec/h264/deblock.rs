//! The in-loop deblocking filter. Spec 8.7.
//!
//! H.264 codes each macroblock independently enough that block edges show as
//! visible seams, especially at low bitrates. The deblocking filter smooths
//! those edges — and crucially it runs *in loop*, so its output becomes the
//! reference for the next frame. That is why it cannot be treated as an
//! optional post-process: skipping it, or getting the rounding wrong, makes
//! prediction drift and the picture visibly rot over the following seconds.
//!
//! # Structure
//!
//! Three stages, kept separate so each is testable:
//!
//! 1. [`boundary_strength`] — how hard to filter an edge, from the coding
//!    modes either side (8.7.2.1).
//! 2. [`Thresholds`] — the `alpha`/`beta`/`tC0` values for an edge, from the
//!    average QP and the slice's filter offsets (8.7.2.2).
//! 3. [`filter_luma_edge`] and [`filter_chroma_edge`] — the sample filters
//!    themselves (8.7.2.3 and 8.7.2.4).
//!
//! # What the caller owes us
//!
//! Edge *ordering* is the macroblock layer's job: the spec filters all
//! vertical edges of a macroblock left to right, then all horizontal edges top
//! to bottom, and each filter sees the output of the previous one. Getting
//! that order wrong produces subtly wrong output that still looks plausible.
//! This module filters one edge at a time and takes boundary strength as
//! given.
//!
//! The test camera sets `deblocking_filter_control_present_flag`, so slice
//! headers may disable the filter or shift the thresholds. Those offsets are
//! parameters here rather than assumed zero.

use super::deblock_tables::{ALPHA, BETA, TC0};

/// Filter thresholds for one edge. Spec 8.7.2.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub alpha: i32,
    pub beta: i32,
    /// Index into [`TC0`], already clipped. Retained because `tC0` depends on
    /// boundary strength, which varies per four-sample group along the edge.
    index_a: usize,
}

impl Thresholds {
    /// Derive thresholds from the average QP across the edge.
    ///
    /// `qp_av` is the mean of the two macroblocks' QPs, rounded up — the
    /// caller computes it because it needs both macroblocks' state. The
    /// offsets come from the slice header, in units of 2.
    pub fn new(qp_av: i32, alpha_offset: i32, beta_offset: i32) -> Self {
        let index_a = (qp_av + alpha_offset).clamp(0, 51) as usize;
        let index_b = (qp_av + beta_offset).clamp(0, 51) as usize;
        Thresholds {
            alpha: ALPHA[index_a] as i32,
            beta: BETA[index_b] as i32,
            index_a,
        }
    }

    /// `tC0` for a given boundary strength. Only defined for `bs` 1..=3.
    #[inline]
    fn tc0(&self, bs: u8) -> i32 {
        debug_assert!((1..=3).contains(&bs));
        TC0[self.index_a][bs as usize - 1] as i32
    }
}

/// How a block either side of an edge was coded, for strength derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeBlock {
    /// Intra-coded blocks always attract the strongest filtering, because
    /// their errors do not come from a shared reference.
    pub intra: bool,
    /// Whether the block has any non-zero transform coefficients.
    pub has_coeffs: bool,
    /// Motion vector, quarter-sample units. Ignored when `intra`.
    pub mv: (i32, i32),
    /// Reference picture index, or -1 when intra.
    pub ref_idx: i8,
}

/// Derive the boundary strength for one edge. Spec 8.7.2.1.
///
/// `macroblock_edge` must be true when the edge lies on a macroblock boundary
/// rather than an internal transform block edge; intra macroblock edges filter
/// harder than intra block edges inside a macroblock.
///
/// Returns 0..=4, where 0 means do not filter and 4 selects the strong filter.
pub fn boundary_strength(p: EdgeBlock, q: EdgeBlock, macroblock_edge: bool) -> u8 {
    if p.intra || q.intra {
        // Intra content either side is always filtered, and hardest of all
        // across a macroblock boundary.
        return if macroblock_edge { 4 } else { 3 };
    }
    if p.has_coeffs || q.has_coeffs {
        return 2;
    }
    // Both sides are inter coded with no residual, so the only thing that can
    // create a seam is a motion discontinuity. One full sample of difference
    // is the spec's threshold, and motion vectors are in quarter samples.
    if p.ref_idx != q.ref_idx || (p.mv.0 - q.mv.0).abs() >= 4 || (p.mv.1 - q.mv.1).abs() >= 4 {
        return 1;
    }
    0
}

#[inline]
fn clip1(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// One line of samples crossing an edge, as the spec's `p`/`q` naming.
///
/// `p[0]` and `q[0]` sit either side of the boundary, with indices increasing
/// away from it. Abstracting this is what lets one implementation filter both
/// vertical and horizontal edges: the caller supplies the stride between
/// samples, so a vertical edge walks by 1 and a horizontal edge by the plane
/// stride.
struct Line<'a> {
    data: &'a mut [u8],
    /// Index of `q[0]`.
    q0: usize,
    /// Distance between consecutive samples across the edge.
    step: usize,
}

impl Line<'_> {
    #[inline]
    fn p(&self, i: usize) -> i32 {
        self.data[self.q0 - (i + 1) * self.step] as i32
    }
    #[inline]
    fn q(&self, i: usize) -> i32 {
        self.data[self.q0 + i * self.step] as i32
    }
    #[inline]
    fn set_p(&mut self, i: usize, v: i32) {
        let idx = self.q0 - (i + 1) * self.step;
        self.data[idx] = clip1(v);
    }
    #[inline]
    fn set_q(&mut self, i: usize, v: i32) {
        let idx = self.q0 + i * self.step;
        self.data[idx] = clip1(v);
    }
}

/// Whether an edge line is smooth enough to be a coding artefact rather than
/// real detail. Spec 8-468.
///
/// This is the filter's whole trick: a genuine edge in the source has a large
/// jump at the boundary, so `alpha` rejects it and the filter leaves it alone.
/// Only shallow steps — which real images rarely contain but block coding
/// produces constantly — get smoothed.
#[inline]
fn should_filter(line: &Line, t: &Thresholds) -> bool {
    (line.p(0) - line.q(0)).abs() < t.alpha
        && (line.p(1) - line.p(0)).abs() < t.beta
        && (line.q(1) - line.q(0)).abs() < t.beta
}

/// Filter one line of a luma edge.
fn filter_luma_line(line: &mut Line, bs: u8, t: &Thresholds) {
    if bs == 0 || !should_filter(line, t) {
        return;
    }

    let (p0, p1, p2) = (line.p(0), line.p(1), line.p(2));
    let (q0, q1, q2) = (line.q(0), line.q(1), line.q(2));

    let ap = (p2 - p0).abs();
    let aq = (q2 - q0).abs();

    if bs < 4 {
        // Normal filter, spec 8.7.2.3. `tC` widens by one for each side that
        // is flat enough to also adjust its second sample.
        let tc0 = t.tc0(bs);
        let mut tc = tc0;
        if ap < t.beta {
            tc += 1;
        }
        if aq < t.beta {
            tc += 1;
        }

        let delta = (((q0 - p0) * 4 + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
        line.set_p(0, p0 + delta);
        line.set_q(0, q0 - delta);

        if ap < t.beta {
            let d = ((p2 + ((p0 + q0 + 1) >> 1) - 2 * p1) >> 1).clamp(-tc0, tc0);
            line.set_p(1, p1 + d);
        }
        if aq < t.beta {
            let d = ((q2 + ((p0 + q0 + 1) >> 1) - 2 * q1) >> 1).clamp(-tc0, tc0);
            line.set_q(1, q1 + d);
        }
    } else {
        // Strong filter, spec 8.7.2.4. Reserved for intra macroblock edges,
        // where blocking is worst and there is no shared reference to
        // preserve.
        let strong_p = ap < t.beta && (p0 - q0).abs() < ((t.alpha >> 2) + 2);
        let strong_q = aq < t.beta && (p0 - q0).abs() < ((t.alpha >> 2) + 2);

        if strong_p {
            let p3 = line.p(3);
            line.set_p(0, (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3);
            line.set_p(1, (p2 + p1 + p0 + q0 + 2) >> 2);
            line.set_p(2, (2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3);
        } else {
            line.set_p(0, (2 * p1 + p0 + q1 + 2) >> 2);
        }

        if strong_q {
            let q3 = line.q(3);
            line.set_q(0, (q2 + 2 * q1 + 2 * q0 + 2 * p0 + p1 + 4) >> 3);
            line.set_q(1, (q2 + q1 + q0 + p0 + 2) >> 2);
            line.set_q(2, (2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3);
        } else {
            line.set_q(0, (2 * q1 + q0 + p1 + 2) >> 2);
        }
    }
}

/// Filter one line of a chroma edge.
///
/// Chroma never adjusts more than one sample either side, because chroma
/// blocks are smaller and the extra smoothing is not worth the detail loss.
fn filter_chroma_line(line: &mut Line, bs: u8, t: &Thresholds) {
    if bs == 0 || !should_filter(line, t) {
        return;
    }

    let (p0, p1) = (line.p(0), line.p(1));
    let (q0, q1) = (line.q(0), line.q(1));

    if bs < 4 {
        // Chroma always gets the +1 that luma earns conditionally.
        let tc = t.tc0(bs) + 1;
        let delta = (((q0 - p0) * 4 + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
        line.set_p(0, p0 + delta);
        line.set_q(0, q0 - delta);
    } else {
        line.set_p(0, (2 * p1 + p0 + q1 + 2) >> 2);
        line.set_q(0, (2 * q1 + q0 + p1 + 2) >> 2);
    }
}

/// How an edge runs through the plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// A vertical edge: samples cross it horizontally, lines run downward.
    Vertical,
    /// A horizontal edge: samples cross it vertically, lines run rightward.
    Horizontal,
}

/// One edge to filter, as geometry plus strengths.
///
/// Grouped rather than passed loose because the four fields are meaningless
/// apart: `bs` is indexed by position along the edge, so its length only makes
/// sense against `length`.
#[derive(Debug, Clone, Copy)]
pub struct Edge<'a> {
    pub kind: EdgeKind,
    /// The first sample on the `q` side of the edge.
    pub x: usize,
    pub y: usize,
    /// How many lines to filter: 16 for a full luma macroblock edge, 8 for
    /// chroma.
    pub length: usize,
    /// One boundary strength per equal group of lines, matching the
    /// four-sample granularity at which the spec derives it.
    pub bs: &'a [u8],
}

/// Walk an edge, applying `filter` to each line whose strength is non-zero.
///
/// Luma and chroma differ only in the per-line filter, so the traversal — and
/// with it the stride arithmetic that is easy to get wrong — is written once.
fn filter_edge(
    plane: &mut [u8],
    stride: usize,
    edge: &Edge<'_>,
    t: &Thresholds,
    filter: fn(&mut Line, u8, &Thresholds),
) {
    let (step, line_step) = match edge.kind {
        EdgeKind::Vertical => (1, stride),
        EdgeKind::Horizontal => (stride, 1),
    };
    let origin = edge.y * stride + edge.x;

    for i in 0..edge.length {
        let strength = edge.bs[(i * edge.bs.len()) / edge.length];
        if strength == 0 {
            continue;
        }
        let mut line = Line {
            data: plane,
            q0: origin + i * line_step,
            step,
        };
        filter(&mut line, strength, t);
    }
}

/// Filter a luma edge. Spec 8.7.2.3 and 8.7.2.4.
pub fn filter_luma_edge(plane: &mut [u8], stride: usize, edge: &Edge<'_>, t: &Thresholds) {
    filter_edge(plane, stride, edge, t, filter_luma_line);
}

/// Filter a chroma edge. Spec 8.7.2.3 and 8.7.2.4.
pub fn filter_chroma_edge(plane: &mut [u8], stride: usize, edge: &Edge<'_>, t: &Thresholds) {
    filter_edge(plane, stride, edge, t, filter_chroma_line);
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Tables -----------------------------------------------------------

    #[test]
    fn thresholds_are_monotonic_and_start_at_zero() {
        // Below qP 16 the filter is disabled entirely by a zero alpha, which
        // is what keeps high-quality streams from being smeared.
        for i in 0..16 {
            assert_eq!(ALPHA[i], 0, "alpha {i}");
            assert_eq!(BETA[i], 0, "beta {i}");
        }
        for w in ALPHA.windows(2) {
            assert!(w[1] >= w[0], "alpha is not monotonic");
        }
        for w in BETA.windows(2) {
            assert!(w[1] >= w[0], "beta is not monotonic");
        }
        assert_eq!(ALPHA[51], 255);
        assert_eq!(BETA[51], 18);
    }

    #[test]
    fn tc0_is_monotonic_in_both_axes() {
        for (i, row) in TC0.iter().enumerate() {
            for w in row.windows(2) {
                assert!(w[1] >= w[0], "tc0 row {i} not monotonic in strength");
            }
        }
        for w in TC0.windows(2) {
            for (k, (&lo, &hi)) in w[0].iter().zip(w[1].iter()).enumerate() {
                assert!(hi >= lo, "tc0 column {k} not monotonic in qP");
            }
        }
    }

    #[test]
    fn threshold_index_is_clipped() {
        let low = Thresholds::new(-30, 0, 0);
        assert_eq!(low, Thresholds::new(0, 0, 0));
        let high = Thresholds::new(90, 0, 0);
        assert_eq!(high, Thresholds::new(51, 0, 0));
        // Slice offsets shift the index, which is the point of them.
        assert_eq!(Thresholds::new(30, 6, 0).alpha, ALPHA[36] as i32);
        assert_eq!(Thresholds::new(30, 0, -6).beta, BETA[24] as i32);
    }

    // -- Boundary strength ------------------------------------------------

    #[test]
    fn intra_content_always_filters_hardest() {
        let intra = EdgeBlock {
            intra: true,
            has_coeffs: false,
            mv: (0, 0),
            ref_idx: -1,
        };
        let inter = EdgeBlock {
            intra: false,
            has_coeffs: false,
            mv: (0, 0),
            ref_idx: 0,
        };
        assert_eq!(boundary_strength(intra, inter, true), 4);
        assert_eq!(boundary_strength(inter, intra, true), 4);
        // Inside a macroblock, intra gets 3 rather than the strong filter.
        assert_eq!(boundary_strength(intra, inter, false), 3);
    }

    #[test]
    fn coefficients_outrank_motion_differences() {
        let coded = EdgeBlock {
            intra: false,
            has_coeffs: true,
            mv: (0, 0),
            ref_idx: 0,
        };
        let clean = EdgeBlock {
            intra: false,
            has_coeffs: false,
            mv: (0, 0),
            ref_idx: 0,
        };
        assert_eq!(boundary_strength(coded, clean, false), 2);
        assert_eq!(boundary_strength(clean, coded, false), 2);
    }

    #[test]
    fn motion_discontinuity_triggers_the_weakest_filter() {
        let base = EdgeBlock {
            intra: false,
            has_coeffs: false,
            mv: (0, 0),
            ref_idx: 0,
        };

        // Identical motion, nothing to smooth.
        assert_eq!(boundary_strength(base, base, false), 0);

        // Three quarter-samples apart is below the one-sample threshold.
        let near = EdgeBlock { mv: (3, 0), ..base };
        assert_eq!(boundary_strength(base, near, false), 0);

        // Exactly one full sample apart does trigger it.
        let far = EdgeBlock { mv: (4, 0), ..base };
        assert_eq!(boundary_strength(base, far, false), 1);
        let far_y = EdgeBlock {
            mv: (0, -4),
            ..base
        };
        assert_eq!(boundary_strength(base, far_y, false), 1);

        // Different reference pictures, regardless of vector similarity.
        let other_ref = EdgeBlock { ref_idx: 1, ..base };
        assert_eq!(boundary_strength(base, other_ref, false), 1);
    }

    // -- Filtering --------------------------------------------------------

    /// Build a plane with a vertical step edge at x == 8, so `p` samples hold
    /// `left` and `q` samples hold `right`.
    fn step_plane(left: u8, right: u8) -> Vec<u8> {
        let mut p = vec![0u8; 16 * 16];
        for y in 0..16 {
            for x in 0..16 {
                p[y * 16 + x] = if x < 8 { left } else { right };
            }
        }
        p
    }

    /// A large step is real picture content, not a coding artefact, and
    /// `alpha` must reject it. This is the property that keeps the filter from
    /// destroying genuine edges.
    #[test]
    fn a_large_step_is_left_alone() {
        let mut plane = step_plane(20, 200);
        let before = plane.clone();
        let t = Thresholds::new(30, 0, 0);
        filter_luma_edge(
            &mut plane,
            16,
            &Edge {
                kind: EdgeKind::Vertical,
                x: 8,
                y: 0,
                length: 16,
                bs: &[3],
            },
            &t,
        );
        assert_eq!(plane, before, "a 180-level step must exceed alpha");
    }

    /// A small step is exactly what block coding produces, and must be
    /// smoothed: the samples nearest the edge move toward each other.
    #[test]
    fn a_small_step_is_smoothed() {
        let mut plane = step_plane(100, 108);
        let t = Thresholds::new(40, 0, 0);
        filter_luma_edge(
            &mut plane,
            16,
            &Edge {
                kind: EdgeKind::Vertical,
                x: 8,
                y: 0,
                length: 16,
                bs: &[2],
            },
            &t,
        );

        for y in 0..16 {
            let p0 = plane[y * 16 + 7];
            let q0 = plane[y * 16 + 8];
            assert!(p0 > 100, "row {y}: p0 {p0} should have risen");
            assert!(q0 < 108, "row {y}: q0 {q0} should have fallen");
            assert!(p0 <= q0, "row {y}: filter must not invert the step");
        }
    }

    /// Boundary strength 0 means the edge is not filtered at all, whatever the
    /// samples look like.
    #[test]
    fn strength_zero_filters_nothing() {
        let mut plane = step_plane(100, 106);
        let before = plane.clone();
        let t = Thresholds::new(40, 0, 0);
        filter_luma_edge(
            &mut plane,
            16,
            &Edge {
                kind: EdgeKind::Vertical,
                x: 8,
                y: 0,
                length: 16,
                bs: &[0],
            },
            &t,
        );
        assert_eq!(plane, before);
    }

    /// A flat plane has no seam, so filtering must be a no-op regardless of
    /// strength. Catches sign and rounding errors that a step test can hide.
    #[test]
    fn a_flat_plane_survives_every_strength() {
        for bs in 1..=4u8 {
            for value in [0u8, 1, 128, 254, 255] {
                let mut plane = vec![value; 16 * 16];
                let before = plane.clone();
                let t = Thresholds::new(45, 0, 0);
                filter_luma_edge(
                    &mut plane,
                    16,
                    &Edge {
                        kind: EdgeKind::Vertical,
                        x: 8,
                        y: 0,
                        length: 16,
                        bs: &[bs],
                    },
                    &t,
                );
                assert_eq!(plane, before, "bs {bs} on flat {value}");

                let mut chroma = vec![value; 16 * 16];
                filter_chroma_edge(
                    &mut chroma,
                    16,
                    &Edge {
                        kind: EdgeKind::Vertical,
                        x: 8,
                        y: 0,
                        length: 8,
                        bs: &[bs],
                    },
                    &t,
                );
                assert_eq!(chroma, before, "chroma bs {bs} on flat {value}");
            }
        }
    }

    /// Vertical and horizontal edges must filter identically on transposed
    /// input. This is what proves the `step`/`line_step` abstraction is right,
    /// and it is a classic place to get a stride wrong.
    #[test]
    fn horizontal_edges_match_transposed_vertical_edges() {
        let vertical_in = step_plane(100, 112);
        let mut horizontal_in = vec![0u8; 16 * 16];
        for y in 0..16 {
            for x in 0..16 {
                horizontal_in[x * 16 + y] = vertical_in[y * 16 + x];
            }
        }

        let t = Thresholds::new(40, 0, 0);
        let mut v = vertical_in.clone();
        filter_luma_edge(
            &mut v,
            16,
            &Edge {
                kind: EdgeKind::Vertical,
                x: 8,
                y: 0,
                length: 16,
                bs: &[3],
            },
            &t,
        );

        let mut h = horizontal_in.clone();
        filter_luma_edge(
            &mut h,
            16,
            &Edge {
                kind: EdgeKind::Horizontal,
                x: 0,
                y: 8,
                length: 16,
                bs: &[3],
            },
            &t,
        );

        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(v[y * 16 + x], h[x * 16 + y], "mismatch at ({x},{y})");
            }
        }
    }

    /// The strong filter adjusts three samples either side; the normal filter
    /// at most two. Confirms `bs == 4` actually selects a different path.
    #[test]
    fn strong_filter_reaches_further_than_the_normal_one() {
        let t = Thresholds::new(45, 0, 0);

        let mut normal = step_plane(100, 110);
        filter_luma_edge(
            &mut normal,
            16,
            &Edge {
                kind: EdgeKind::Vertical,
                x: 8,
                y: 0,
                length: 16,
                bs: &[3],
            },
            &t,
        );

        let mut strong = step_plane(100, 110);
        filter_luma_edge(
            &mut strong,
            16,
            &Edge {
                kind: EdgeKind::Vertical,
                x: 8,
                y: 0,
                length: 16,
                bs: &[4],
            },
            &t,
        );

        // p2 is the third sample left of the edge, at x == 5.
        assert_eq!(normal[5], 100, "normal filter must not touch p2");
        assert_ne!(strong[5], 100, "strong filter must touch p2");
    }

    /// Chroma only ever moves the samples immediately either side.
    #[test]
    fn chroma_touches_only_the_nearest_samples() {
        let mut plane = step_plane(100, 110);
        let t = Thresholds::new(45, 0, 0);
        filter_chroma_edge(
            &mut plane,
            16,
            &Edge {
                kind: EdgeKind::Vertical,
                x: 8,
                y: 0,
                length: 8,
                bs: &[4],
            },
            &t,
        );

        for y in 0..8 {
            assert_eq!(plane[y * 16 + 6], 100, "p1 must be untouched");
            assert_ne!(plane[y * 16 + 7], 100, "p0 must be filtered");
            assert_ne!(plane[y * 16 + 8], 110, "q0 must be filtered");
            assert_eq!(plane[y * 16 + 9], 110, "q1 must be untouched");
        }
    }

    /// Per-group boundary strengths must be applied to the right lines: the
    /// spec derives one strength per four samples along the edge.
    #[test]
    fn per_group_strengths_apply_to_their_own_lines() {
        let mut plane = step_plane(100, 108);
        let t = Thresholds::new(40, 0, 0);
        // Filter the first and third groups of four lines only.
        filter_luma_edge(
            &mut plane,
            16,
            &Edge {
                kind: EdgeKind::Vertical,
                x: 8,
                y: 0,
                length: 16,
                bs: &[2, 0, 2, 0],
            },
            &t,
        );

        for y in 0..4 {
            assert_ne!(plane[y * 16 + 7], 100, "group 0 should be filtered");
        }
        for y in 4..8 {
            assert_eq!(plane[y * 16 + 7], 100, "group 1 should be untouched");
        }
        for y in 8..12 {
            assert_ne!(plane[y * 16 + 7], 100, "group 2 should be filtered");
        }
        for y in 12..16 {
            assert_eq!(plane[y * 16 + 7], 100, "group 3 should be untouched");
        }
    }

    /// A zero alpha disables the filter, which is how low-QP streams opt out.
    #[test]
    fn low_qp_disables_filtering() {
        let mut plane = step_plane(100, 104);
        let before = plane.clone();
        let t = Thresholds::new(10, 0, 0);
        assert_eq!(t.alpha, 0);
        filter_luma_edge(
            &mut plane,
            16,
            &Edge {
                kind: EdgeKind::Vertical,
                x: 8,
                y: 0,
                length: 16,
                bs: &[4],
            },
            &t,
        );
        assert_eq!(plane, before);
    }
}
