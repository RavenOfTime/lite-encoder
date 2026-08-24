//! Intra prediction.
//!
//! Every intra mode predicts a block from already-reconstructed neighbouring
//! samples: the row above, the column to the left, and the corner between
//! them. Spec 8.3.
//!
//! # Why 4x4 and 8x8 share one implementation
//!
//! Intra_4x4 (8.3.1) and Intra_8x8 (8.3.2) define the same nine modes over
//! different block sizes, and the spec writes them out twice with slightly
//! different-looking formulas. They are the same formulas: where the 4x4 text
//! says `p[-1, y-1]` and the 8x8 text says `p[-1, y-2x-1]`, the 4x4 case can
//! only be reached with `x == 0`, so the general form covers both. The two
//! genuine differences are parameterised here:
//!
//! - boundary cases scale with `N` (`x == N-1 && y == N-1`, `zHU == 2N-3`)
//! - Intra_8x8 low-pass filters its reference samples first; Intra_4x4 does not
//!
//! # What the caller owes us
//!
//! Neighbour *availability* is a macroblock-layer question (spec 6.4.11): it
//! depends on slice boundaries, macroblock addresses and, when
//! `constrained_intra_pred_flag` is set, on whether the neighbour was inter
//! coded. That logic lives with the macroblock layer. This module takes
//! availability as given and implements only the arithmetic.

/// Clip1_Y / Clip1_C at 8-bit, spec 5-5.
#[inline]
fn clip1(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// The nine Intra_4x4 / Intra_8x8 prediction modes, spec tables 8-2 and 8-3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntraNxNMode {
    Vertical = 0,
    Horizontal = 1,
    Dc = 2,
    DiagonalDownLeft = 3,
    DiagonalDownRight = 4,
    VerticalRight = 5,
    HorizontalDown = 6,
    VerticalLeft = 7,
    HorizontalUp = 8,
}

impl IntraNxNMode {
    pub fn from_id(id: u8) -> Option<Self> {
        use IntraNxNMode::*;
        Some(match id {
            0 => Vertical,
            1 => Horizontal,
            2 => Dc,
            3 => DiagonalDownLeft,
            4 => DiagonalDownRight,
            5 => VerticalRight,
            6 => HorizontalDown,
            7 => VerticalLeft,
            8 => HorizontalUp,
            _ => return None,
        })
    }
}

/// The four Intra_16x16 modes, spec table 8-4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intra16x16Mode {
    Vertical = 0,
    Horizontal = 1,
    Dc = 2,
    Plane = 3,
}

/// The four chroma modes, spec table 8-5.
///
/// Note the numbering does **not** match [`Intra16x16Mode`]: DC is 0 here and
/// 2 there, and vertical and horizontal are swapped. The spec really does
/// assign them differently, and conflating the two is a standing trap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntraChromaMode {
    Dc = 0,
    Horizontal = 1,
    Vertical = 2,
    Plane = 3,
}

impl IntraChromaMode {
    pub fn from_id(id: u8) -> Option<Self> {
        use IntraChromaMode::*;
        Some(match id {
            0 => Dc,
            1 => Horizontal,
            2 => Vertical,
            3 => Plane,
            _ => return None,
        })
    }
}

/// Neighbouring reconstructed samples, as the caller has them.
///
/// `top` may carry `N` samples or `2 * N`. The upper half is the "top right"
/// group that the diagonal modes read: when it is absent but `top` itself is
/// present, the spec requires replicating the last top sample across it, which
/// [`Refs::new`] does. Supplying `2 * N` samples means top-right is genuinely
/// available.
#[derive(Debug, Clone, Copy, Default)]
pub struct Neighbours<'a> {
    pub top: Option<&'a [u8]>,
    pub left: Option<&'a [u8]>,
    pub top_left: Option<u8>,
}

/// Reference samples laid out for indexing, after availability resolution.
///
/// Mirrors the spec's `p[x, y]` with `x` in `-1..2N-1` and `y` in `-1..N-1`,
/// flattened so `top[x]` is `p[x, -1]` and `left[y]` is `p[-1, y]`.
struct Refs {
    top: [u8; 16],
    left: [u8; 8],
    top_left: u8,
    has_top: bool,
    has_left: bool,
    has_top_left: bool,
}

impl Refs {
    fn new<const N: usize>(n: &Neighbours) -> Self {
        let mut refs = Refs {
            top: [0; 16],
            left: [0; 8],
            top_left: 0,
            has_top: n.top.is_some(),
            has_left: n.left.is_some(),
            has_top_left: n.top_left.is_some(),
        };

        if let Some(top) = n.top {
            refs.top[..N].copy_from_slice(&top[..N]);
            if top.len() >= 2 * N {
                refs.top[N..2 * N].copy_from_slice(&top[N..2 * N]);
            } else {
                // Top-right unavailable: replicate p[N-1, -1]. Spec 8.3.1.2.4
                // and 8.3.2.2.1 both rely on this substitution having already
                // happened, rather than special-casing it per mode.
                for x in N..2 * N {
                    refs.top[x] = top[N - 1];
                }
            }
        }
        if let Some(left) = n.left {
            refs.left[..N].copy_from_slice(&left[..N]);
        }
        if let Some(tl) = n.top_left {
            refs.top_left = tl;
        }
        refs
    }

    #[inline]
    fn t(&self, x: usize) -> i32 {
        self.top[x] as i32
    }

    #[inline]
    fn l(&self, y: usize) -> i32 {
        self.left[y] as i32
    }

    #[inline]
    fn tl(&self) -> i32 {
        self.top_left as i32
    }

    /// `p[x, -1]` allowing `x == -1`, which the spec uses to mean the corner
    /// sample `p[-1, -1]`. The diagonal and the two "corner-crossing" modes
    /// walk off the left end of the top row by exactly one, so this is not an
    /// out-of-range access to guard against but a defined reference.
    #[inline]
    fn ts(&self, x: i32) -> i32 {
        if x < 0 {
            self.tl()
        } else {
            self.top[x as usize] as i32
        }
    }

    /// `p[-1, y]` allowing `y == -1`, for the same reason as [`Refs::ts`].
    #[inline]
    fn ls(&self, y: i32) -> i32 {
        if y < 0 {
            self.tl()
        } else {
            self.left[y as usize] as i32
        }
    }

    /// Low-pass filter for Intra_8x8 reference samples, spec 8.3.2.2.1.
    ///
    /// Intra_8x8 blocks are large enough that unfiltered neighbours produce
    /// visible edge artefacts, so the spec filters them with a [1 2 1] kernel
    /// before prediction. Intra_4x4 does not do this.
    fn filter_8x8(&self) -> Refs {
        let mut out = Refs {
            top: self.top,
            left: self.left,
            top_left: self.top_left,
            has_top: self.has_top,
            has_left: self.has_left,
            has_top_left: self.has_top_left,
        };

        if self.has_top {
            out.top[0] = if self.has_top_left {
                clip1((self.tl() + 2 * self.t(0) + self.t(1) + 2) >> 2)
            } else {
                clip1((3 * self.t(0) + self.t(1) + 2) >> 2)
            };
            for x in 1..15 {
                out.top[x] = clip1((self.t(x - 1) + 2 * self.t(x) + self.t(x + 1) + 2) >> 2);
            }
            out.top[15] = clip1((self.t(14) + 3 * self.t(15) + 2) >> 2);
        }

        if self.has_top_left {
            out.top_left = clip1(match (self.has_top, self.has_left) {
                (true, true) => (self.t(0) + 2 * self.tl() + self.l(0) + 2) >> 2,
                (true, false) => (3 * self.tl() + self.t(0) + 2) >> 2,
                (false, true) => (3 * self.tl() + self.l(0) + 2) >> 2,
                (false, false) => self.tl(),
            });
        }

        if self.has_left {
            out.left[0] = if self.has_top_left {
                clip1((self.tl() + 2 * self.l(0) + self.l(1) + 2) >> 2)
            } else {
                clip1((3 * self.l(0) + self.l(1) + 2) >> 2)
            };
            for y in 1..7 {
                out.left[y] = clip1((self.l(y - 1) + 2 * self.l(y) + self.l(y + 1) + 2) >> 2);
            }
            out.left[7] = clip1((self.l(6) + 3 * self.l(7) + 2) >> 2);
        }

        out
    }
}

/// Predict a 4x4 luma block. Spec 8.3.1.
///
/// `out` is in raster order. Returns `false` without writing anything if the
/// mode needs a neighbour the caller did not supply, which is a bitstream
/// error rather than something to paper over.
pub fn predict_4x4(mode: IntraNxNMode, n: &Neighbours, out: &mut [u8; 16]) -> bool {
    let refs = Refs::new::<4>(n);
    predict_nxn::<4>(mode, &refs, out)
}

/// Predict an 8x8 luma block. Spec 8.3.2.
///
/// Applies the mandatory reference-sample filtering before prediction.
pub fn predict_8x8(mode: IntraNxNMode, n: &Neighbours, out: &mut [u8; 64]) -> bool {
    let refs = Refs::new::<8>(n).filter_8x8();
    predict_nxn::<8>(mode, &refs, out)
}

/// The nine shared modes, over an `N`x`N` block.
fn predict_nxn<const N: usize>(mode: IntraNxNMode, r: &Refs, out: &mut [u8]) -> bool {
    use IntraNxNMode::*;

    // Availability requirements, spec 8.3.1.2.x / 8.3.2.2.x. DC is the only
    // mode defined for every combination; the rest need specific neighbours.
    let ok = match mode {
        Dc => true,
        Vertical | DiagonalDownLeft | VerticalLeft => r.has_top,
        Horizontal | HorizontalUp => r.has_left,
        DiagonalDownRight | VerticalRight | HorizontalDown => {
            r.has_top && r.has_left && r.has_top_left
        }
    };
    if !ok {
        return false;
    }

    // log2(N), used by the DC rounding shifts.
    let log2n = N.trailing_zeros() as usize;

    let dc = if mode == Dc {
        let top: i32 = (0..N).map(|x| r.t(x)).sum();
        let left: i32 = (0..N).map(|y| r.l(y)).sum();
        match (r.has_top, r.has_left) {
            (true, true) => (top + left + N as i32) >> (log2n + 1),
            (true, false) => (top + (N as i32 >> 1)) >> log2n,
            (false, true) => (left + (N as i32 >> 1)) >> log2n,
            // No neighbours at all: mid-grey, spec 8-65.
            (false, false) => 1 << 7,
        }
    } else {
        0
    };

    for y in 0..N {
        for x in 0..N {
            let (xi, yi) = (x as i32, y as i32);
            let v = match mode {
                Vertical => r.t(x),
                Horizontal => r.l(y),
                Dc => dc,

                DiagonalDownLeft => {
                    if x == N - 1 && y == N - 1 {
                        (r.t(2 * N - 2) + 3 * r.t(2 * N - 1) + 2) >> 2
                    } else {
                        (r.t(x + y) + 2 * r.t(x + y + 1) + r.t(x + y + 2) + 2) >> 2
                    }
                }

                DiagonalDownRight => match xi.cmp(&yi) {
                    std::cmp::Ordering::Greater => {
                        let d = xi - yi;
                        (r.ts(d - 2) + 2 * r.ts(d - 1) + r.ts(d) + 2) >> 2
                    }
                    std::cmp::Ordering::Less => {
                        let d = yi - xi;
                        (r.ls(d - 2) + 2 * r.ls(d - 1) + r.ls(d) + 2) >> 2
                    }
                    std::cmp::Ordering::Equal => (r.t(0) + 2 * r.tl() + r.l(0) + 2) >> 2,
                },

                VerticalRight => {
                    let zvr = 2 * xi - yi;
                    if zvr >= 0 {
                        let base = xi - (yi >> 1);
                        if zvr % 2 == 0 {
                            (r.ts(base - 1) + r.ts(base) + 1) >> 1
                        } else {
                            (r.ts(base - 2) + 2 * r.ts(base - 1) + r.ts(base) + 2) >> 2
                        }
                    } else if zvr == -1 {
                        (r.l(0) + 2 * r.tl() + r.t(0) + 2) >> 2
                    } else {
                        // Reachable for N == 8 at several x; for N == 4 only at
                        // x == 0, where it reduces to the same expression.
                        let b = yi - 2 * xi - 1;
                        (r.ls(b) + 2 * r.ls(b - 1) + r.ls(b - 2) + 2) >> 2
                    }
                }

                HorizontalDown => {
                    let zhd = 2 * yi - xi;
                    if zhd >= 0 {
                        let base = yi - (xi >> 1);
                        if zhd % 2 == 0 {
                            (r.ls(base - 1) + r.ls(base) + 1) >> 1
                        } else {
                            (r.ls(base - 2) + 2 * r.ls(base - 1) + r.ls(base) + 2) >> 2
                        }
                    } else if zhd == -1 {
                        (r.l(0) + 2 * r.tl() + r.t(0) + 2) >> 2
                    } else {
                        let b = xi - 2 * yi - 1;
                        (r.ts(b) + 2 * r.ts(b - 1) + r.ts(b - 2) + 2) >> 2
                    }
                }

                VerticalLeft => {
                    let base = x + (y >> 1);
                    if y % 2 == 0 {
                        (r.t(base) + r.t(base + 1) + 1) >> 1
                    } else {
                        (r.t(base) + 2 * r.t(base + 1) + r.t(base + 2) + 2) >> 2
                    }
                }

                HorizontalUp => {
                    let zhu = xi + 2 * yi;
                    let last = N - 1;
                    if zhu == (2 * N - 3) as i32 {
                        (r.l(last - 1) + 3 * r.l(last) + 2) >> 2
                    } else if zhu > (2 * N - 3) as i32 {
                        r.l(last)
                    } else {
                        let base = y + (x >> 1);
                        if zhu % 2 == 0 {
                            (r.l(base) + r.l(base + 1) + 1) >> 1
                        } else {
                            (r.l(base) + 2 * r.l(base + 1) + r.l(base + 2) + 2) >> 2
                        }
                    }
                }
            };
            out[y * N + x] = clip1(v);
        }
    }
    true
}

/// Predict a 16x16 luma macroblock. Spec 8.3.3.
///
/// `top` and `left` carry 16 samples each. Unlike the NxN modes, no top-right
/// group exists: a macroblock's diagonal modes are the Plane mode instead.
pub fn predict_16x16(mode: Intra16x16Mode, n: &Neighbours, out: &mut [u8; 256]) -> bool {
    let has_top = n.top.is_some();
    let has_left = n.left.is_some();
    let t = |x: usize| n.top.map_or(0, |s| s[x] as i32);
    let l = |y: usize| n.left.map_or(0, |s| s[y] as i32);

    match mode {
        Intra16x16Mode::Vertical => {
            if !has_top {
                return false;
            }
            for y in 0..16 {
                for x in 0..16 {
                    out[y * 16 + x] = clip1(t(x));
                }
            }
        }
        Intra16x16Mode::Horizontal => {
            if !has_left {
                return false;
            }
            for y in 0..16 {
                let v = clip1(l(y));
                for x in 0..16 {
                    out[y * 16 + x] = v;
                }
            }
        }
        Intra16x16Mode::Dc => {
            let top: i32 = (0..16).map(t).sum();
            let left: i32 = (0..16).map(l).sum();
            let dc = match (has_top, has_left) {
                (true, true) => (top + left + 16) >> 5,
                (true, false) => (top + 8) >> 4,
                (false, true) => (left + 8) >> 4,
                (false, false) => 1 << 7,
            };
            out.fill(clip1(dc));
        }
        Intra16x16Mode::Plane => {
            if !(has_top && has_left && n.top_left.is_some()) {
                return false;
            }
            let tl = n.top_left.unwrap() as i32;
            // p[-1, -1] participates as the x' == 7 / y' == 7 term.
            let at = |x: i32| if x < 0 { tl } else { t(x as usize) };
            let al = |y: i32| if y < 0 { tl } else { l(y as usize) };

            let mut h = 0;
            let mut v = 0;
            for i in 0..8i32 {
                h += (i + 1) * (at(8 + i) - at(6 - i));
                v += (i + 1) * (al(8 + i) - al(6 - i));
            }
            let a = 16 * (l(15) + t(15));
            let b = (5 * h + 32) >> 6;
            let c = (5 * v + 32) >> 6;

            for y in 0..16i32 {
                for x in 0..16i32 {
                    let p = (a + b * (x - 7) + c * (y - 7) + 16) >> 5;
                    out[(y * 16 + x) as usize] = clip1(p);
                }
            }
        }
    }
    true
}

/// Predict one 8x8 chroma plane for 4:2:0. Spec 8.3.4.
///
/// Called once for Cb and once for Cr.
pub fn predict_chroma_8x8(mode: IntraChromaMode, n: &Neighbours, out: &mut [u8; 64]) -> bool {
    let has_top = n.top.is_some();
    let has_left = n.left.is_some();
    let t = |x: usize| n.top.map_or(0, |s| s[x] as i32);
    let l = |y: usize| n.left.map_or(0, |s| s[y] as i32);

    match mode {
        IntraChromaMode::Vertical => {
            if !has_top {
                return false;
            }
            for y in 0..8 {
                for x in 0..8 {
                    out[y * 8 + x] = clip1(t(x));
                }
            }
        }
        IntraChromaMode::Horizontal => {
            if !has_left {
                return false;
            }
            for y in 0..8 {
                let v = clip1(l(y));
                for x in 0..8 {
                    out[y * 8 + x] = v;
                }
            }
        }
        IntraChromaMode::Dc => {
            // Chroma DC is per 4x4 sub-block, and which neighbours a sub-block
            // prefers depends on where it sits. The corner sub-blocks average
            // both edges; the off-diagonal ones prefer the edge they touch and
            // only fall back to the other. Spec 8.3.4.1 through 8.3.4.3.
            for blk_y in [0usize, 4] {
                for blk_x in [0usize, 4] {
                    let sum_top: i32 = (blk_x..blk_x + 4).map(t).sum();
                    let sum_left: i32 = (blk_y..blk_y + 4).map(l).sum();

                    let prefer_top = blk_x > 0 && blk_y == 0;
                    let prefer_left = blk_x == 0 && blk_y > 0;

                    let dc = if prefer_top {
                        if has_top {
                            (sum_top + 2) >> 2
                        } else if has_left {
                            (sum_left + 2) >> 2
                        } else {
                            1 << 7
                        }
                    } else if prefer_left {
                        if has_left {
                            (sum_left + 2) >> 2
                        } else if has_top {
                            (sum_top + 2) >> 2
                        } else {
                            1 << 7
                        }
                    } else {
                        match (has_top, has_left) {
                            (true, true) => (sum_top + sum_left + 4) >> 3,
                            (true, false) => (sum_top + 2) >> 2,
                            (false, true) => (sum_left + 2) >> 2,
                            (false, false) => 1 << 7,
                        }
                    };

                    let v = clip1(dc);
                    for y in blk_y..blk_y + 4 {
                        for x in blk_x..blk_x + 4 {
                            out[y * 8 + x] = v;
                        }
                    }
                }
            }
        }
        IntraChromaMode::Plane => {
            if !(has_top && has_left && n.top_left.is_some()) {
                return false;
            }
            let tl = n.top_left.unwrap() as i32;
            let at = |x: i32| if x < 0 { tl } else { t(x as usize) };
            let al = |y: i32| if y < 0 { tl } else { l(y as usize) };

            let mut h = 0;
            let mut v = 0;
            for i in 0..4i32 {
                h += (i + 1) * (at(4 + i) - at(2 - i));
                v += (i + 1) * (al(4 + i) - al(2 - i));
            }
            let a = 16 * (l(7) + t(7));
            // 34 rather than the luma 5, because the chroma block is 8 wide
            // instead of 16. Spec 8-144.
            let b = (34 * h + 32) >> 6;
            let c = (34 * v + 32) >> 6;

            for y in 0..8i32 {
                for x in 0..8i32 {
                    let p = (a + b * (x - 3) + c * (y - 3) + 16) >> 5;
                    out[(y * 8 + x) as usize] = clip1(p);
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_NXN: [IntraNxNMode; 9] = [
        IntraNxNMode::Vertical,
        IntraNxNMode::Horizontal,
        IntraNxNMode::Dc,
        IntraNxNMode::DiagonalDownLeft,
        IntraNxNMode::DiagonalDownRight,
        IntraNxNMode::VerticalRight,
        IntraNxNMode::HorizontalDown,
        IntraNxNMode::VerticalLeft,
        IntraNxNMode::HorizontalUp,
    ];

    /// Every intra mode is an interpolation of its neighbours with weights
    /// summing to a power of two, so constant neighbours must give a constant
    /// block equal to that value. This holds for all nine NxN modes, both
    /// block sizes, the 16x16 modes and chroma, which makes it the broadest
    /// single check available: almost any indexing mistake breaks it.
    #[test]
    fn flat_neighbours_give_flat_prediction() {
        for v in [0u8, 1, 17, 128, 200, 255] {
            let top = [v; 16];
            let left = [v; 16];
            let n = Neighbours {
                top: Some(&top),
                left: Some(&left),
                top_left: Some(v),
            };

            for mode in ALL_NXN {
                let mut out4 = [0u8; 16];
                assert!(predict_4x4(mode, &n, &mut out4), "{mode:?} 4x4 refused");
                assert!(
                    out4.iter().all(|&s| s == v),
                    "4x4 {mode:?} on flat {v} gave {out4:?}"
                );

                let mut out8 = [0u8; 64];
                assert!(predict_8x8(mode, &n, &mut out8), "{mode:?} 8x8 refused");
                assert!(
                    out8.iter().all(|&s| s == v),
                    "8x8 {mode:?} on flat {v} was not flat"
                );
            }

            for mode in [
                Intra16x16Mode::Vertical,
                Intra16x16Mode::Horizontal,
                Intra16x16Mode::Dc,
                Intra16x16Mode::Plane,
            ] {
                let mut out = [0u8; 256];
                assert!(predict_16x16(mode, &n, &mut out));
                assert!(
                    out.iter().all(|&s| s == v),
                    "16x16 {mode:?} on flat {v} was not flat"
                );
            }

            for mode in [
                IntraChromaMode::Dc,
                IntraChromaMode::Horizontal,
                IntraChromaMode::Vertical,
                IntraChromaMode::Plane,
            ] {
                let mut out = [0u8; 64];
                assert!(predict_chroma_8x8(mode, &n, &mut out));
                assert!(
                    out.iter().all(|&s| s == v),
                    "chroma {mode:?} on flat {v} was not flat"
                );
            }
        }
    }

    #[test]
    fn vertical_copies_top_row_and_horizontal_copies_left() {
        let top: Vec<u8> = (10..26).collect();
        let left: Vec<u8> = (100..116).collect();
        let n = Neighbours {
            top: Some(&top),
            left: Some(&left),
            top_left: Some(7),
        };

        let mut out = [0u8; 16];
        predict_4x4(IntraNxNMode::Vertical, &n, &mut out);
        for y in 0..4 {
            assert_eq!(&out[y * 4..y * 4 + 4], &top[..4], "row {y}");
        }

        predict_4x4(IntraNxNMode::Horizontal, &n, &mut out);
        for y in 0..4 {
            assert!(out[y * 4..y * 4 + 4].iter().all(|&s| s == left[y]));
        }
    }

    #[test]
    fn dc_with_no_neighbours_is_mid_grey() {
        let n = Neighbours::default();

        let mut out4 = [0u8; 16];
        assert!(predict_4x4(IntraNxNMode::Dc, &n, &mut out4));
        assert!(out4.iter().all(|&s| s == 128));

        let mut out16 = [0u8; 256];
        assert!(predict_16x16(Intra16x16Mode::Dc, &n, &mut out16));
        assert!(out16.iter().all(|&s| s == 128));

        let mut outc = [0u8; 64];
        assert!(predict_chroma_8x8(IntraChromaMode::Dc, &n, &mut outc));
        assert!(outc.iter().all(|&s| s == 128));
    }

    /// DC must average both edges when both are available, and use only the
    /// one that is available otherwise.
    #[test]
    fn dc_averages_available_edges() {
        let top = [40u8; 16];
        let left = [80u8; 16];

        let both = Neighbours {
            top: Some(&top),
            left: Some(&left),
            top_left: None,
        };
        let mut out = [0u8; 16];
        predict_4x4(IntraNxNMode::Dc, &both, &mut out);
        assert!(out.iter().all(|&s| s == 60), "expected mean of 40 and 80");

        let top_only = Neighbours {
            top: Some(&top),
            left: None,
            top_left: None,
        };
        predict_4x4(IntraNxNMode::Dc, &top_only, &mut out);
        assert!(out.iter().all(|&s| s == 40));

        let left_only = Neighbours {
            top: None,
            left: Some(&left),
            top_left: None,
        };
        predict_4x4(IntraNxNMode::Dc, &left_only, &mut out);
        assert!(out.iter().all(|&s| s == 80));
    }

    /// Modes that need neighbours the caller did not supply must refuse rather
    /// than read zeroes. Only DC is defined for every combination.
    #[test]
    fn modes_refuse_missing_neighbours() {
        let empty = Neighbours::default();
        let mut out = [0u8; 16];
        for mode in ALL_NXN {
            let accepted = predict_4x4(mode, &empty, &mut out);
            assert_eq!(
                accepted,
                mode == IntraNxNMode::Dc,
                "{mode:?} availability handling"
            );
        }

        // Plane needs the corner, not just the two edges.
        let top = [5u8; 16];
        let left = [5u8; 16];
        let no_corner = Neighbours {
            top: Some(&top),
            left: Some(&left),
            top_left: None,
        };
        let mut out16 = [0u8; 256];
        assert!(!predict_16x16(
            Intra16x16Mode::Plane,
            &no_corner,
            &mut out16
        ));
    }

    /// Plane mode fits a gradient through the neighbours, so a linear ramp in
    /// the neighbours must extend into the block as the same ramp. This is the
    /// only test that pins the `a`, `b`, `c` coefficients rather than just
    /// their behaviour on constants.
    #[test]
    fn plane_extends_a_linear_ramp() {
        // p[x, -1] = 60 + x, p[-1, y] = 60 + y, corner 59. Continuing this
        // surface into the block gives pred[x, y] = 60 + x + y.
        let top: Vec<u8> = (0..16).map(|x| 60 + x as u8).collect();
        let left: Vec<u8> = (0..16).map(|y| 60 + y as u8).collect();
        let n = Neighbours {
            top: Some(&top),
            left: Some(&left),
            top_left: Some(59),
        };

        let mut out = [0u8; 256];
        assert!(predict_16x16(Intra16x16Mode::Plane, &n, &mut out));
        for y in 0..16usize {
            for x in 0..16usize {
                let expected = 60 + x as i32 + y as i32;
                let got = out[y * 16 + x] as i32;
                assert!(
                    (got - expected).abs() <= 1,
                    "at ({x},{y}): got {got}, expected about {expected}"
                );
            }
        }
    }

    /// The top-right samples the diagonal modes read must be replicated from
    /// the last top sample when the caller supplies only `N` of them, so a
    /// short `top` and an explicitly padded one must predict identically.
    #[test]
    fn missing_top_right_is_replicated() {
        let short = [10u8, 20, 30, 40];
        let padded = [10u8, 20, 30, 40, 40, 40, 40, 40];

        for mode in [IntraNxNMode::DiagonalDownLeft, IntraNxNMode::VerticalLeft] {
            let mut a = [0u8; 16];
            let mut b = [0u8; 16];
            predict_4x4(
                mode,
                &Neighbours {
                    top: Some(&short),
                    left: None,
                    top_left: None,
                },
                &mut a,
            );
            predict_4x4(
                mode,
                &Neighbours {
                    top: Some(&padded),
                    left: None,
                    top_left: None,
                },
                &mut b,
            );
            assert_eq!(a, b, "{mode:?} disagreed on replicated top-right");
        }
    }

    /// Genuine top-right samples must actually be used, otherwise the
    /// replication test above would pass a decoder that ignored them.
    #[test]
    fn present_top_right_changes_diagonal_prediction() {
        let short = [10u8, 20, 30, 40];
        let with_tr = [10u8, 20, 30, 40, 200, 200, 200, 200];

        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        predict_4x4(
            IntraNxNMode::DiagonalDownLeft,
            &Neighbours {
                top: Some(&short),
                left: None,
                top_left: None,
            },
            &mut a,
        );
        predict_4x4(
            IntraNxNMode::DiagonalDownLeft,
            &Neighbours {
                top: Some(&with_tr),
                left: None,
                top_left: None,
            },
            &mut b,
        );
        assert_ne!(a, b);
    }

    /// Intra_8x8 filters its reference samples; Intra_4x4 does not. On a step
    /// edge the filter must visibly smooth the result, which is the cheapest
    /// way to prove the filter is actually running.
    #[test]
    fn intra_8x8_filters_reference_samples() {
        let mut top = [0u8; 16];
        top[8..].fill(255);
        let n = Neighbours {
            top: Some(&top),
            left: None,
            top_left: None,
        };

        let mut out = [0u8; 64];
        assert!(predict_8x8(IntraNxNMode::Vertical, &n, &mut out));

        // Unfiltered, the top row would be a hard 0/255 step at x == 8. After
        // the [1 2 1] kernel, x == 7 must have lifted off zero.
        assert!(
            out[7] > 0 && out[7] < 255,
            "expected a filtered transition, got {}",
            out[7]
        );
    }

    /// Chroma DC treats sub-blocks differently by position: the top-right
    /// sub-block prefers the top edge and the bottom-left prefers the left,
    /// so with disagreeing edges the four sub-blocks must not be uniform.
    #[test]
    fn chroma_dc_is_per_sub_block() {
        let top = [40u8; 16];
        let left = [200u8; 16];
        let n = Neighbours {
            top: Some(&top),
            left: Some(&left),
            top_left: None,
        };

        let mut out = [0u8; 64];
        assert!(predict_chroma_8x8(IntraChromaMode::Dc, &n, &mut out));

        let at = |x: usize, y: usize| out[y * 8 + x];
        // Top-left and bottom-right average both edges.
        assert_eq!(at(0, 0), 120);
        assert_eq!(at(4, 4), 120);
        // Top-right prefers the top edge, bottom-left prefers the left edge.
        assert_eq!(at(4, 0), 40);
        assert_eq!(at(0, 4), 200);
    }

    #[test]
    fn chroma_mode_ids_do_not_match_luma_ids() {
        // Guards the numbering trap called out on `IntraChromaMode`.
        assert_eq!(IntraChromaMode::from_id(0), Some(IntraChromaMode::Dc));
        assert_eq!(IntraNxNMode::from_id(0), Some(IntraNxNMode::Vertical));
        assert_eq!(IntraChromaMode::from_id(4), None);
        assert_eq!(IntraNxNMode::from_id(9), None);
    }
}
