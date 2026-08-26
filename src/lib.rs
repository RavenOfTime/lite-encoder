//! lite-encoder: a Rust-native ffmpeg alternative.
//!
//! Demux, decode, encode, and mux through one pipeline. Codecs are implemented
//! in Rust where we own correctness; containers and encoders are added behind
//! the same traits, prioritized in [`README.md`] and `todo.md`.
//!
//! Today: pure-Rust H.264 decode, optional AV1 encode (rav1e), WebM mux,
//! Annex B elementary read for benches. Demuxers, remux/copy, and the CLI are
//! on the roadmap.

pub mod codec;
pub mod demux;
pub mod media;
pub mod mux;
pub mod probe;
pub mod registry;
pub mod remux;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid argument: {0}")]
    Spec(String),

    #[error("demux error: {0}")]
    Demux(String),

    #[error("decode error: {0}")]
    Decode(String),

    #[error("encode error: {0}")]
    Encode(String),

    #[error("mux error: {0}")]
    Mux(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
