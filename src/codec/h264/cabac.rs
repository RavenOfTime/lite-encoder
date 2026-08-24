//! CABAC: context-adaptive binary arithmetic decoding. Spec 9.3.
//!
//! The test camera uses CABAC on both streams with no CAVLC fallback, so this
//! is not an optional path for us. See `h264_scope` output in the project
//! notes.
//!
//! # Structure
//!
//! Three layers, and keeping them apart is what makes any of this testable:
//!
//! 1. [`ArithDecoder`] — the arithmetic decoding engine (9.3.3.2). Pure
//!    interval arithmetic over a bit source. Knows nothing about H.264.
//! 2. [`ContextState`] — the adaptive probability models (9.3.1.1, 9.3.3.2.1).
//!    A `pStateIdx`/`valMPS` pair per context, initialised from the slice QP.
//! 3. Binarisation helpers — unary, truncated unary, UEGk and fixed-length
//!    (9.3.2). These turn bin strings back into syntax element values.
//!
//! Deriving *which* `ctxIdx` a given syntax element uses (spec tables 9-11
//! and 9-34 onward) depends on neighbouring macroblock state, so it lives with
//! the macroblock layer rather than here. This module owns the machinery, not
//! the per-element policy.
//!
//! # A note on the decoder's shape
//!
//! The spec writes the engine against a `read_bits` primitive that pulls one
//! bit at a time. That is what is implemented here. Production decoders keep
//! `codIOffset` in a wider register and refill 16 bits at a time to cut
//! renormalisation branches; that is a change to this file alone, and worth
//! doing only once real streams decode correctly.

use super::cabac_tables::{CONTEXT_INIT, RANGE_TAB_LPS, TRANS_IDX_LPS, TRANS_IDX_MPS};

/// Which column of [`CONTEXT_INIT`] a slice selects. Spec 9.3.1.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextVariant {
    /// I and SI slices, which have a single initialisation table.
    Intra,
    /// P, SP and B slices, selected by the slice header's `cabac_init_idc`.
    Inter { cabac_init_idc: u8 },
}

impl ContextVariant {
    fn column(self) -> usize {
        match self {
            ContextVariant::Intra => 0,
            // `cabac_init_idc` is constrained to 0..=2 by the spec; the slice
            // header parser rejects anything else before we see it.
            ContextVariant::Inter { cabac_init_idc } => 1 + cabac_init_idc as usize,
        }
    }
}

/// Number of context models this decoder carries.
///
/// `ctxIdx` 0..=459 is everything frame-coded 4:2:0 needs, including the 8x8
/// transform. The spec's 460..=1023 range exists only for 4:4:4, which is
/// rejected at the parameter-set stage.
pub const NUM_CONTEXTS: usize = 460;

/// One adaptive binary probability model: a state index and the current
/// most-probable symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    /// `pStateIdx`, 0..=63. Higher means the MPS is more probable.
    pub state: u8,
    /// `valMPS`, the bin value currently considered most probable.
    pub mps: u8,
}

/// All context models for one slice.
///
/// CABAC state is reset at the start of every slice, which is what makes
/// slices independently decodable and is why this is a per-slice object.
#[derive(Debug, Clone)]
pub struct ContextState {
    contexts: [Context; NUM_CONTEXTS],
}

impl ContextState {
    /// Initialise every context from the slice QP. Spec 9.3.1.1.
    ///
    /// `slice_qp` is `SliceQPY`, which the caller derives from the PPS and the
    /// slice header's `slice_qp_delta`.
    pub fn new(variant: ContextVariant, slice_qp: i32) -> Self {
        let column = variant.column();
        let qp = slice_qp.clamp(0, 51);

        let mut contexts = [Context { state: 0, mps: 0 }; NUM_CONTEXTS];
        for (ctx_idx, slot) in contexts.iter_mut().enumerate() {
            let (m, n) = CONTEXT_INIT[ctx_idx][column];
            // Spec equations 9-5 through 9-7. The clip to 1..=126 is what
            // keeps `pStateIdx` inside 0..=62; state 63 is reserved for the
            // terminate context and is never produced by initialisation.
            let pre = (((m as i32 * qp) >> 4) + n as i32).clamp(1, 126);
            *slot = if pre <= 63 {
                Context {
                    state: (63 - pre) as u8,
                    mps: 0,
                }
            } else {
                Context {
                    state: (pre - 64) as u8,
                    mps: 1,
                }
            };
        }
        ContextState { contexts }
    }

    #[inline]
    pub fn get(&self, ctx_idx: usize) -> Context {
        self.contexts[ctx_idx]
    }
}

impl std::ops::Index<usize> for ContextState {
    type Output = Context;
    #[inline]
    fn index(&self, ctx_idx: usize) -> &Context {
        &self.contexts[ctx_idx]
    }
}

impl std::ops::IndexMut<usize> for ContextState {
    #[inline]
    fn index_mut(&mut self, ctx_idx: usize) -> &mut Context {
        &mut self.contexts[ctx_idx]
    }
}

/// The arithmetic decoding engine. Spec 9.3.3.2.
///
/// Reads from a slice of RBSP bytes, starting at the first byte of slice data
/// after `cabac_alignment_one_bit` padding.
pub struct ArithDecoder<'a> {
    data: &'a [u8],
    /// Next bit to consume, counted from the start of `data`.
    bit_pos: usize,
    /// `codIRange`, the width of the current interval.
    range: u32,
    /// `codIOffset`, the position of the coded value within the interval.
    offset: u32,
}

impl<'a> ArithDecoder<'a> {
    /// Initialise the engine. Spec 9.3.1.2.
    ///
    /// Returns `None` if there are not the nine bits the spec requires to
    /// prime `codIOffset`.
    pub fn new(data: &'a [u8]) -> Option<Self> {
        if data.len() * 8 < 9 {
            return None;
        }
        let mut d = ArithDecoder {
            data,
            bit_pos: 0,
            range: 510,
            offset: 0,
        };
        d.offset = d.read_bits(9);
        Some(d)
    }

    /// Read one bit, MSB first. Reads past the end yield zeroes.
    ///
    /// The spec guarantees a conforming stream never over-reads, so this is a
    /// robustness measure rather than a decoding rule: a truncated or corrupt
    /// slice should decode to nonsense, not panic. The macroblock layer
    /// detects the resulting desynchronisation through `decode_terminate`.
    #[inline]
    fn read_bit(&mut self) -> u32 {
        let byte = self.bit_pos >> 3;
        let bit = 7 - (self.bit_pos & 7);
        self.bit_pos += 1;
        match self.data.get(byte) {
            Some(&b) => ((b >> bit) & 1) as u32,
            None => 0,
        }
    }

    #[inline]
    fn read_bits(&mut self, n: usize) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.read_bit();
        }
        v
    }

    /// Whether the engine has consumed past the end of its data, which means
    /// the slice was truncated or the decoder lost synchronisation.
    pub fn overran(&self) -> bool {
        self.bit_pos > self.data.len() * 8
    }

    /// `RenormD`, spec figure 9-6. Rescale until the interval is at least 256.
    #[inline]
    fn renorm(&mut self) {
        while self.range < 256 {
            self.range <<= 1;
            self.offset = (self.offset << 1) | self.read_bit();
        }
    }

    /// `DecodeDecision`, spec 9.3.3.2.1: decode one bin against an adaptive
    /// context, updating that context's probability state.
    pub fn decode_decision(&mut self, ctx: &mut Context) -> u8 {
        // Which quarter of the 256..511 range we are in selects the row of
        // the LPS table; this is the range quantisation the whole scheme
        // depends on for staying in integer arithmetic.
        let q = ((self.range >> 6) & 3) as usize;
        let lps_range = RANGE_TAB_LPS[ctx.state as usize][q] as u32;
        self.range -= lps_range;

        let bin;
        if self.offset >= self.range {
            // Least probable symbol.
            bin = 1 - ctx.mps;
            self.offset -= self.range;
            self.range = lps_range;
            // Reaching state 0 and coding an LPS means the model was wrong
            // about which symbol is more probable, so flip it.
            if ctx.state == 0 {
                ctx.mps = 1 - ctx.mps;
            }
            ctx.state = TRANS_IDX_LPS[ctx.state as usize];
        } else {
            bin = ctx.mps;
            ctx.state = TRANS_IDX_MPS[ctx.state as usize];
        }

        self.renorm();
        bin
    }

    /// `DecodeBypass`, spec 9.3.3.2.3: decode one bin with a fixed 50/50
    /// model and no context update.
    ///
    /// Used for sign bits and the suffix of large coefficient magnitudes,
    /// where adaptation buys nothing.
    pub fn decode_bypass(&mut self) -> u8 {
        self.offset = (self.offset << 1) | self.read_bit();
        if self.offset >= self.range {
            self.offset -= self.range;
            1
        } else {
            0
        }
    }

    /// `DecodeTerminate`, spec 9.3.3.2.4: decode the `end_of_slice_flag` and
    /// `mb_type`-terminating bins.
    ///
    /// Returns 1 when the slice ends. Note this steals two counts from the
    /// range rather than consulting a context, and only renormalises when the
    /// answer is 0.
    pub fn decode_terminate(&mut self) -> u8 {
        self.range -= 2;
        if self.offset >= self.range {
            1
        } else {
            self.renorm();
            0
        }
    }

    // -- Binarisation helpers, spec 9.3.2 ---------------------------------

    /// Truncated unary, spec 9.3.2.2: up to `c_max` bins, each with its own
    /// context supplied by `ctx_for`.
    ///
    /// The bin index is passed to `ctx_for` because most syntax elements
    /// change context as the prefix grows.
    pub fn decode_truncated_unary(
        &mut self,
        c_max: u32,
        contexts: &mut ContextState,
        mut ctx_for: impl FnMut(u32) -> usize,
    ) -> u32 {
        let mut value = 0;
        while value < c_max {
            let idx = ctx_for(value);
            if self.decode_decision(&mut contexts[idx]) == 0 {
                break;
            }
            value += 1;
        }
        value
    }

    /// Unbounded unary, spec 9.3.2.1.
    ///
    /// `limit` guards against a corrupt stream producing an unbounded loop;
    /// it is not part of the binarisation.
    pub fn decode_unary(
        &mut self,
        limit: u32,
        contexts: &mut ContextState,
        mut ctx_for: impl FnMut(u32) -> usize,
    ) -> u32 {
        let mut value = 0;
        while value < limit {
            let idx = ctx_for(value);
            if self.decode_decision(&mut contexts[idx]) == 0 {
                break;
            }
            value += 1;
        }
        value
    }

    /// Fixed-length binarisation, spec 9.3.2.4: `n` bypass bins, MSB first.
    pub fn decode_fixed_bypass(&mut self, n: usize) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.decode_bypass() as u32;
        }
        v
    }

    /// The Exp-Golomb suffix of a UEGk binarisation, spec 9.3.2.3.
    ///
    /// Only reached once the truncated-unary prefix saturates, which is why
    /// it takes the prefix length rather than deriving it: the caller already
    /// knows whether the escape happened.
    pub fn decode_exp_golomb_bypass(&mut self, k: u32) -> u32 {
        let mut k = k;
        let mut value = 0;
        // Prefix of ones, each doubling the remaining range.
        while self.decode_bypass() == 1 {
            value += 1 << k;
            k += 1;
            // A conforming stream cannot reach this; a corrupt one must not
            // spin forever.
            if k > 30 {
                return value;
            }
        }
        // Suffix: `k` bypass bits.
        while k > 0 {
            k -= 1;
            value += (self.decode_bypass() as u32) << k;
        }
        value
    }

    /// Bits consumed so far, for callers that need to resynchronise.
    pub fn bits_read(&self) -> usize {
        self.bit_pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Table integrity --------------------------------------------------
    //
    // The tables are generated, so these check for corruption and for the
    // structural properties the spec guarantees, not for individual values.

    #[test]
    fn lps_range_shrinks_with_state_and_grows_with_range() {
        // Higher `pStateIdx` means a more skewed model, so the LPS sub-range
        // must not grow as the state increases.
        for (s, pair) in RANGE_TAB_LPS.windows(2).enumerate() {
            for (q, (&prev, &next)) in pair[0].iter().zip(pair[1].iter()).enumerate() {
                assert!(next <= prev, "state {} column {q} is not monotonic", s + 1);
            }
        }
        // A wider interval must give a wider LPS sub-range.
        for (s, row) in RANGE_TAB_LPS.iter().enumerate() {
            for (q, w) in row.windows(2).enumerate() {
                assert!(
                    w[1] >= w[0],
                    "state {s} is not monotonic across range quarters at {q}"
                );
            }
        }
        // The LPS sub-range must always leave something behind, or the
        // interval collapses and renormalisation cannot recover.
        for (s, row) in RANGE_TAB_LPS.iter().enumerate() {
            for (q, &v) in row.iter().enumerate() {
                assert!((2..=240).contains(&v), "state {s} column {q} is {v}");
            }
        }
    }

    #[test]
    fn state_transitions_move_in_the_right_direction() {
        for s in 0..64usize {
            // An MPS makes the model more confident, never less.
            assert!(
                TRANS_IDX_MPS[s] as usize >= s,
                "MPS transition from {s} went backwards"
            );
            // An LPS makes it less confident, never more.
            assert!(
                (TRANS_IDX_LPS[s] as usize) <= s,
                "LPS transition from {s} went forwards"
            );
            assert!(TRANS_IDX_MPS[s] < 64 && TRANS_IDX_LPS[s] < 64);
        }
        // State 62 is the most skewed adapting state and must be a fixed
        // point under repeated MPS, and 63 is the terminate state.
        assert_eq!(TRANS_IDX_MPS[62], 62);
        assert_eq!(TRANS_IDX_LPS[63], 63);
    }

    #[test]
    fn context_init_produces_valid_states() {
        for variant in [
            ContextVariant::Intra,
            ContextVariant::Inter { cabac_init_idc: 0 },
            ContextVariant::Inter { cabac_init_idc: 1 },
            ContextVariant::Inter { cabac_init_idc: 2 },
        ] {
            for qp in 0..=51 {
                let cs = ContextState::new(variant, qp);
                for i in 0..NUM_CONTEXTS {
                    let c = cs.get(i);
                    // 63 is reserved for terminate and must never be produced
                    // by initialisation.
                    assert!(
                        c.state <= 62,
                        "{variant:?} qp {qp} ctx {i} state {}",
                        c.state
                    );
                    assert!(c.mps <= 1);
                }
            }
        }
    }

    /// Out-of-range slice QPs must be clipped, not indexed with.
    #[test]
    fn context_init_clips_slice_qp() {
        let low = ContextState::new(ContextVariant::Intra, -20);
        let zero = ContextState::new(ContextVariant::Intra, 0);
        let high = ContextState::new(ContextVariant::Intra, 200);
        let max = ContextState::new(ContextVariant::Intra, 51);
        for i in 0..NUM_CONTEXTS {
            assert_eq!(low.get(i), zero.get(i), "ctx {i} below range");
            assert_eq!(high.get(i), max.get(i), "ctx {i} above range");
        }
    }

    // -- Engine round-trip ------------------------------------------------
    //
    // A CABAC *encoder* built only from the spec's own definitions, used to
    // prove the decoder inverts it. Encoder and decoder share the lookup
    // tables, so this validates the engine logic and the state machine rather
    // than the table values; the table tests above cover those separately.

    struct ArithEncoder {
        low: u32,
        range: u32,
        bits_outstanding: u32,
        first: bool,
        out: Vec<u8>,
        cur: u8,
        nbits: u8,
    }

    impl ArithEncoder {
        fn new() -> Self {
            ArithEncoder {
                low: 0,
                range: 510,
                bits_outstanding: 0,
                first: true,
                out: Vec::new(),
                cur: 0,
                nbits: 0,
            }
        }

        fn put_bit(&mut self, b: u8) {
            // The very first bit produced by the encoder is a leading zero the
            // decoder's 9-bit prime does not expect; spec 9.3.4.1.
            if self.first {
                self.first = false;
            } else {
                self.cur = (self.cur << 1) | b;
                self.nbits += 1;
                if self.nbits == 8 {
                    self.out.push(self.cur);
                    self.cur = 0;
                    self.nbits = 0;
                }
            }
            while self.bits_outstanding > 0 {
                let inv = 1 - b;
                self.cur = (self.cur << 1) | inv;
                self.nbits += 1;
                if self.nbits == 8 {
                    self.out.push(self.cur);
                    self.cur = 0;
                    self.nbits = 0;
                }
                self.bits_outstanding -= 1;
            }
        }

        /// `PutBit` plus renormalisation, spec figure 9-8.
        fn renorm(&mut self) {
            while self.range < 256 {
                if self.low < 256 {
                    self.put_bit(0);
                } else if self.low >= 512 {
                    self.low -= 512;
                    self.put_bit(1);
                } else {
                    self.low -= 256;
                    self.bits_outstanding += 1;
                }
                self.range <<= 1;
                self.low <<= 1;
            }
        }

        fn encode_decision(&mut self, ctx: &mut Context, bin: u8) {
            let q = ((self.range >> 6) & 3) as usize;
            let lps_range = RANGE_TAB_LPS[ctx.state as usize][q] as u32;
            self.range -= lps_range;

            if bin != ctx.mps {
                self.low += self.range;
                self.range = lps_range;
                if ctx.state == 0 {
                    ctx.mps = 1 - ctx.mps;
                }
                ctx.state = TRANS_IDX_LPS[ctx.state as usize];
            } else {
                ctx.state = TRANS_IDX_MPS[ctx.state as usize];
            }
            self.renorm();
        }

        fn encode_bypass(&mut self, bin: u8) {
            self.low <<= 1;
            if bin != 0 {
                self.low += self.range;
            }
            if self.low >= 1024 {
                self.put_bit(1);
                self.low -= 1024;
            } else if self.low < 512 {
                self.put_bit(0);
            } else {
                self.low -= 512;
                self.bits_outstanding += 1;
            }
        }

        fn encode_terminate(&mut self, bin: u8) {
            self.range -= 2;
            if bin != 0 {
                self.low += self.range;
                self.range = 2;
            }
            self.renorm();
        }

        /// `EncodeFlush`, spec figure 9-11.
        fn finish(mut self) -> Vec<u8> {
            self.range = 2;
            self.renorm();
            self.put_bit(((self.low >> 9) & 1) as u8);
            // The final two bits plus a stop bit, per the flush procedure.
            let b = (((self.low >> 7) & 3) | 1) as u8;
            self.put_bit((b >> 1) & 1);
            self.put_bit(b & 1);
            while self.nbits != 0 {
                self.cur <<= 1;
                self.nbits += 1;
                if self.nbits == 8 {
                    self.out.push(self.cur);
                    self.cur = 0;
                    self.nbits = 0;
                }
            }
            // Padding so the decoder never reads past the end of real data.
            self.out.extend_from_slice(&[0xff; 8]);
            self.out
        }
    }

    /// Deterministic pseudo-random bins, so failures reproduce.
    fn lcg(seed: &mut u32) -> u32 {
        *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        *seed >> 16
    }

    #[test]
    fn engine_round_trips_context_coded_bins() {
        let mut seed = 12345u32;
        let bins: Vec<(usize, u8)> = (0..4000)
            .map(|_| {
                let ctx = (lcg(&mut seed) % 64) as usize;
                let bin = (lcg(&mut seed) % 2) as u8;
                (ctx, bin)
            })
            .collect();

        let mut enc_ctx = ContextState::new(ContextVariant::Intra, 26);
        let mut enc = ArithEncoder::new();
        for &(ctx, bin) in &bins {
            enc.encode_decision(&mut enc_ctx[ctx], bin);
        }
        let data = enc.finish();

        let mut dec_ctx = ContextState::new(ContextVariant::Intra, 26);
        let mut dec = ArithDecoder::new(&data).expect("enough data");
        for (i, &(ctx, expected)) in bins.iter().enumerate() {
            let got = dec.decode_decision(&mut dec_ctx[ctx]);
            assert_eq!(got, expected, "bin {i} in context {ctx}");
        }
    }

    #[test]
    fn engine_round_trips_bypass_bins() {
        let mut seed = 999u32;
        let bins: Vec<u8> = (0..2000).map(|_| (lcg(&mut seed) % 2) as u8).collect();

        let mut enc = ArithEncoder::new();
        for &b in &bins {
            enc.encode_bypass(b);
        }
        let data = enc.finish();

        let mut dec = ArithDecoder::new(&data).expect("enough data");
        for (i, &expected) in bins.iter().enumerate() {
            assert_eq!(dec.decode_bypass(), expected, "bypass bin {i}");
        }
    }

    /// Context-coded and bypass bins interleave freely in real slice data, and
    /// bypass decoding perturbs the same `offset`/`range` pair, so the mixed
    /// case is the one that catches state-sharing mistakes.
    #[test]
    fn engine_round_trips_mixed_bins() {
        #[derive(Clone, Copy)]
        enum Op {
            Ctx(usize, u8),
            Bypass(u8),
        }

        let mut seed = 424242u32;
        let ops: Vec<Op> = (0..4000)
            .map(|_| {
                if lcg(&mut seed).is_multiple_of(3) {
                    Op::Bypass((lcg(&mut seed) % 2) as u8)
                } else {
                    Op::Ctx(
                        (lcg(&mut seed) % NUM_CONTEXTS as u32) as usize,
                        (lcg(&mut seed) % 2) as u8,
                    )
                }
            })
            .collect();

        let mut enc_ctx = ContextState::new(ContextVariant::Inter { cabac_init_idc: 1 }, 30);
        let mut enc = ArithEncoder::new();
        for &op in &ops {
            match op {
                Op::Ctx(c, b) => enc.encode_decision(&mut enc_ctx[c], b),
                Op::Bypass(b) => enc.encode_bypass(b),
            }
        }
        let data = enc.finish();

        let mut dec_ctx = ContextState::new(ContextVariant::Inter { cabac_init_idc: 1 }, 30);
        let mut dec = ArithDecoder::new(&data).expect("enough data");
        for (i, &op) in ops.iter().enumerate() {
            match op {
                Op::Ctx(c, expected) => {
                    assert_eq!(dec.decode_decision(&mut dec_ctx[c]), expected, "op {i}")
                }
                Op::Bypass(expected) => assert_eq!(dec.decode_bypass(), expected, "op {i}"),
            }
        }
    }

    /// `decode_terminate` returning 1 is how a slice ends, so a run of zeroes
    /// followed by a one must survive the round trip exactly.
    #[test]
    fn engine_round_trips_terminate() {
        let mut enc_ctx = ContextState::new(ContextVariant::Intra, 28);
        let mut enc = ArithEncoder::new();
        for i in 0..50 {
            enc.encode_decision(&mut enc_ctx[i % NUM_CONTEXTS], (i % 2) as u8);
            enc.encode_terminate(0);
        }
        enc.encode_terminate(1);
        let data = enc.finish();

        let mut dec_ctx = ContextState::new(ContextVariant::Intra, 28);
        let mut dec = ArithDecoder::new(&data).expect("enough data");
        for i in 0..50 {
            assert_eq!(
                dec.decode_decision(&mut dec_ctx[i % NUM_CONTEXTS]),
                (i % 2) as u8
            );
            assert_eq!(dec.decode_terminate(), 0, "premature terminate at {i}");
        }
        assert_eq!(dec.decode_terminate(), 1, "slice end not detected");
    }

    #[test]
    fn exp_golomb_suffix_round_trips() {
        // UEGk suffixes are pure bypass bins, so encode the bin string the
        // spec's binarisation would produce and check the decoder rebuilds
        // the value. k = 0 is the coefficient-magnitude case.
        for value in [0u32, 1, 2, 3, 7, 8, 15, 16, 100, 1000] {
            let mut bins = Vec::new();
            let mut k = 0;
            let mut remaining = value;
            while remaining >= (1 << k) {
                bins.push(1u8);
                remaining -= 1 << k;
                k += 1;
            }
            bins.push(0);
            for j in (0..k).rev() {
                bins.push(((remaining >> j) & 1) as u8);
            }

            let mut enc = ArithEncoder::new();
            for &b in &bins {
                enc.encode_bypass(b);
            }
            let data = enc.finish();

            let mut dec = ArithDecoder::new(&data).expect("enough data");
            assert_eq!(dec.decode_exp_golomb_bypass(0), value, "value {value}");
        }
    }

    #[test]
    fn truncated_unary_stops_at_c_max() {
        // All-ones input: the decoder must stop at c_max without consuming a
        // terminating zero that is not there.
        let mut enc_ctx = ContextState::new(ContextVariant::Intra, 26);
        let mut enc = ArithEncoder::new();
        for _ in 0..5 {
            enc.encode_decision(&mut enc_ctx[0], 1);
        }
        // A marker bin in a different context, to prove nothing extra was read.
        enc.encode_decision(&mut enc_ctx[7], 1);
        let data = enc.finish();

        let mut dec_ctx = ContextState::new(ContextVariant::Intra, 26);
        let mut dec = ArithDecoder::new(&data).expect("enough data");
        let v = dec.decode_truncated_unary(5, &mut dec_ctx, |_| 0);
        assert_eq!(v, 5);
        assert_eq!(
            dec.decode_decision(&mut dec_ctx[7]),
            1,
            "over-read the prefix"
        );
    }

    #[test]
    fn truncated_unary_stops_early_on_zero() {
        let mut enc_ctx = ContextState::new(ContextVariant::Intra, 26);
        let mut enc = ArithEncoder::new();
        enc.encode_decision(&mut enc_ctx[0], 1);
        enc.encode_decision(&mut enc_ctx[0], 1);
        enc.encode_decision(&mut enc_ctx[0], 0);
        enc.encode_decision(&mut enc_ctx[7], 1);
        let data = enc.finish();

        let mut dec_ctx = ContextState::new(ContextVariant::Intra, 26);
        let mut dec = ArithDecoder::new(&data).expect("enough data");
        assert_eq!(dec.decode_truncated_unary(9, &mut dec_ctx, |_| 0), 2);
        assert_eq!(dec.decode_decision(&mut dec_ctx[7]), 1);
    }

    #[test]
    fn rejects_data_too_short_to_initialise() {
        assert!(ArithDecoder::new(&[]).is_none());
        assert!(ArithDecoder::new(&[0x00]).is_none());
        assert!(ArithDecoder::new(&[0x00, 0x00]).is_some());
    }

    /// A truncated slice must degrade rather than panic. Cameras drop packets,
    /// so this is a real path, not a theoretical one.
    #[test]
    fn truncated_data_does_not_panic() {
        let data = [0x12, 0x34];
        let mut ctx = ContextState::new(ContextVariant::Intra, 26);
        let mut dec = ArithDecoder::new(&data).expect("enough data");
        for i in 0..500 {
            dec.decode_decision(&mut ctx[i % NUM_CONTEXTS]);
            dec.decode_bypass();
        }
        assert!(dec.overran(), "expected the overrun flag to be set");
    }
}
