//! A pure-Rust H.264 decoder, built for the subset IP cameras actually emit.
//!
//! # Scope
//!
//! This is not a general H.264 decoder and does not try to become one. It
//! targets progressive 8-bit 4:2:0 Baseline/Main/High, which is what every
//! RTSP surveillance camera sends. Interlacing (MBAFF/PAFF), 4:2:2, 4:4:4,
//! bit depths above 8, lossless mode, data partitioning and the SVC/MVC
//! extensions are all rejected at the parameter-set stage rather than
//! half-implemented. Refusing them outright removes a large share of the
//! spec's complexity, because interlacing in particular infects reference
//! handling, deblocking and motion-vector prediction everywhere it is allowed.
//!
//! # What we do not write ourselves
//!
//! The bitstream front end is already a dependency. `h264-reader` handles NAL
//! splitting, RBSP unescaping, Exp-Golomb reading, and full SPS/PPS/slice
//! header parsing including scaling lists, weighted-prediction tables and
//! reference-picture marking. That is the tedious, spec-transcription half of
//! the parsing work, and it is maintained. We start below the slice header,
//! at the macroblock layer, which `h264-reader` does not touch.
//!
//! # Module plan
//!
//! Each stage is separately testable, and they land roughly in this order:
//!
//! - `transform` — inverse integer transforms and dequantisation (this file's
//!   first sibling; pure arithmetic, fully testable against the spec alone)
//! - `intra`     — intra prediction modes
//! - `cavlc`     — Baseline entropy decode of the macroblock layer
//! - `cabac`     — Main/High entropy decode; swaps in under the same mb layer
//! - `inter`     — motion compensation, quarter-pel luma and eighth-pel chroma
//! - `deblock`   — the in-loop deblocking filter
//! - `dpb`       — decoded picture buffer, POC ordering, reference marking
//!
//! Reaching `intra` + `transform` + `deblock` already yields a decoder that
//! renders keyframes correctly, which is the first milestone worth having.
//!
//! # Correctness strategy
//!
//! Video decoders are bit-exact: a single wrong rounding mode in deblocking
//! does not soften the picture, it accumulates through inter prediction until
//! frames visibly rot seconds later. So differential testing against a
//! reference decoder is wired up from the start rather than bolted on, using
//! the [`crate::media::Decoder`] seam to run this decoder and a reference
//! side by side over the same stream and compare every decoded sample. See
//! `differential`, behind the off-by-default `reference-decoder` feature.

pub mod annexb;
pub mod cabac;
pub mod cabac_tables;
pub mod deblock;
pub mod deblock_tables;
pub mod decoder;
#[cfg(feature = "reference-decoder")]
pub mod differential;
pub mod inter;
pub mod intra;
pub mod mb;
pub mod neighbour;
pub mod picture;
pub mod loopfilter;
pub mod picture_decode;
pub mod recon;
#[cfg(feature = "reference-decoder")]
pub mod reference;
pub mod residual;
pub mod slice;
pub mod state;
pub mod syntax;
pub mod transform;
