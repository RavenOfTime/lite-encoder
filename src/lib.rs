//! lite-encoder: a focused media processor.
//!
//! Scope is deliberately narrow. It ingests RTSP, supervises long-running
//! recording jobs, reports on them continuously, and writes WebM. It is not
//! a general format converter.
//!
//! The one constraint that shapes everything: WebM is a Matroska subset that
//! only permits VP8/VP9/AV1 video and Vorbis/Opus audio, while RTSP cameras
//! send H.264/H.265. So there is no such thing as a cheap RTSP-to-WebM
//! remux; that path always decodes and re-encodes. See [`job::Treatment`].

pub mod job;
pub mod media;
pub mod mux;
pub mod source;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The job description itself is impossible; caught before any I/O.
    #[error("invalid job spec: {0}")]
    Spec(String),

    #[error("source error: {0}")]
    Source(String),

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
