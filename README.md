# lite-encoder

A pure-Rust media stack aimed at being an **ffmpeg alternative for a narrow
job**: ingest the formats cameras and live sources actually send, decode them
in Rust, and export **WebM** plus a few other optimized targets.

It is not a general format converter. The complexity budget goes into correct,
fast decoders and a reliable processing pipeline — not into supporting every
container and codec ffmpeg does.

## Why decode at all

WebM permits only **VP8/VP9/AV1** video and **Vorbis/Opus** audio. RTSP cameras
send **H.264/H.265** with AAC or G.711. So camera → WebM is never a remux; it
always decodes and re-encodes. The shipping decode path is pure Rust.
OpenH264 (and any later reference) exists only for testing and validation —
bit-exact differential checks and bench calibration — never as a runtime
fallback. Encoders stay behind `media::Encoder` the same way.

| Path | Cost per 1080p camera | Output |
|---|---|---|
| Passthrough (remux) | ~1% of a core | fMP4 / MKV — **not** WebM |
| Transcode to WebM | ~1–2 cores (software) | WebM |

Hardware help for WebM codecs is uneven; AV1 is the long-term encode target.
`JobSpec::validate` rejects impossible combinations before any I/O happens.

## Decoder-first roadmap

1. **H.264** (`src/codec/h264/`) — primary work. Progressive 8-bit 4:2:0 CABAC
   I/P, the subset surveillance cameras emit. Interlacing, high bit depth,
   exotic chroma, FMO, and the SVC/MVC extensions are refused at the
   parameter-set stage rather than half-implemented.
2. **H.265 / HEVC** — next decoder. Same product reason as H.264: it is what
   newer cameras send on RTSP.
3. **Encode + export** — AV1 (and later other optimized outputs), driven by the
   recording/job layer already sketched in-tree.
4. **Ingest breadth** — deepen RTSP and add other live sources as needed; still
   not “every format”.

Correctness is bit-exact against a reference oracle, sample by sample. A
one-bit CABAC, prediction, or transform mistake propagates across pictures; the
useful failure is the first differing coordinate, not an average quality score.
Speed matters for the same reason: a decoder that is accurate but slower than
real time cannot sit in a continuous recording path.

## Status

Working and tested:

- **H.264 CABAC decoder** (`src/codec/h264/`) — Annex B access-unit parsing,
  SPS/PPS lifetime, CABAC macroblock syntax, intra/inter prediction (block
  motion compensation with gathered patches), residual reconstruction,
  deblocking, cropped YUV420 output, and a small decoded-picture buffer. Matches
  the OpenH264 oracle bit-exactly on synthetic streams and on a real 1080p
  camera capture (224 pictures). OpenH264 is used only to validate that
  result and to calibrate throughput; it is not part of the shipping decoder.
  Release decode clears the continuous-recording gate of **≥60 fps at 1080p**
  (2.0× real time at 30 fps, single thread); measured ~129 fps on the 224-picture
  capture (~4.3× @ 30).
- **EBML/WebM muxer** (`src/mux/`) — written for recording, not conversion.
  The Segment stays unknown-sized while open and clusters are buffered then
  written whole, so a file is playable while growing and a crash costs at most
  one cluster.
- **Timeline normalisation** (`src/source/timeline.rs`) — absorbs NTP clock
  corrections, RTP wraparound, stalls and camera reboots into a monotonic
  timeline, and reports every correction instead of hiding it.
- **RTSP source** (`src/source/rtsp.rs`) — digest auth, interleaved TCP, and
  RTP depacketisation into access units via `retina`.
- **Job and reporting model** (`src/job/`) — job spec, state machine, and a
  typed event stream. `Reporter` is a trait, so a gRPC or HTTP surface is an
  implementation rather than a rewrite.

Still ahead:

- **H.265 decoder** — not started; same pure-Rust + differential-oracle pattern.
- **Encode and recording pipeline** — AV1 encoding, the job supervisor loop
  (segment rollover, reconnect backoff), and the transport surface.
- **End-to-end camera → WebM** — waits on decode confidence at speed and on
  encode.

## H.264 development notes

Shipping decode is pure Rust only. OpenH264 is gated behind
`reference-decoder`, off by default, and used solely for testing and
validation: differential bit-exact checks against real streams, and optional
throughput comparison in the bench. It is never linked into a recording build.

### Supported H.264 input

Input must be Annex B, progressive, 8-bit YUV 4:2:0 H.264. CABAC I and P
slices, including multiple slices per picture, are supported. The decoder
accepts normal SPS/PPS updates, cropping, scaling lists, IDR and non-IDR
pictures, and ordinary non-VCL metadata such as SEI and access-unit delimiters.

It explicitly rejects interlacing (PAFF/MBAFF), non-8-bit video, monochrome,
4:2:2 and 4:4:4 chroma, FMO slice groups, CAVLC, data partitioning, SVC/MVC
and other extension NAL units, auxiliary/depth pictures, reserved or
unspecified NAL types, and long-term `ref_pic_list_modification`. Short-term
list reordering (spec 8.2.4.3.1) is implemented; the Tapo fixture uses
`Subtract(0)` on P slices. B-slice decoding and H.265 are outside current scope.

The checked-in camera fixture (`tests/fixtures/tapo-1080p-cabac-8x8.h264`) is
four 1080p pictures; see `tests/fixtures/README.md`. The full local capture
`camera.h264` (224 pictures) is gitignored and used for manual checks.

**Throughput gate (1080p continuous recording):** best pass of
`bench_h264` on a ≥200-picture 1080p capture must be **≥60 fps** (2.0× real
time at 30 fps, single-threaded release). That leaves half the wall-clock for
AV1 encode and OS jitter on the same box. The four-picture fixture is too short
to use as the gate. Measured 2026-08-26: ~129 fps (4.3× @ 30); OpenH264 ~222 fps.

    cargo test
    cargo test --features reference-decoder codec::h264::differential
    cargo test --features reference-decoder --test h264_camera_regression
    cargo run --features reference-decoder --example diff_h264 -- camera.h264
    cargo run --release --example bench_h264 -- camera.h264

## Layout

    src/codec/       pure-Rust decoders (H.264 today; H.265 planned)
    src/media/       codec set, packets, frames, Decoder/Encoder traits
    src/job/         job spec, state, and the event/report model
    src/source/      Source trait, RTSP ingest, timeline normalisation
    src/mux/         EBML primitives and the WebM muxer
    tools/           independent structural validator for emitted files

## Build

Pinned to Rust 1.98 via `rust-toolchain.toml` (a transitive dependency of
`url` requires edition 2024).

    cargo test
    cargo clippy --all-targets

Decode the first picture in an Annex B H.264 elementary stream into a luma PGM:

    cargo run --example decode_h264 -- camera.h264 first-frame.pgm

Compare every decoded sample against the optional OpenH264 oracle (vendored C,
development-only):

    cargo run --features reference-decoder --example diff_h264 -- camera.h264

Emit a structurally complete WebM and check it with a parser that shares no
code with the muxer:

    cargo run --example write_webm -- out.webm
    python tools/ebml_check.py out.webm

Point the ingest path at a real camera:

    cargo run --example rtsp_probe -- rtsp://host/stream --user U --pass P --secs 20

Add `--dump probe.h264` to write the video elementary stream as Annex B with
parameter sets on every keyframe. There is no WebM output from a camera yet;
that waits on encode.

The AV1 encoder is behind a feature flag (heavy build, transcode path only):

    cargo build --features av1
