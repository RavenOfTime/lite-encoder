# lite-encoder

A focused media processor: RTSP ingest, supervised long-running recording
jobs, continuous job reporting, WebM output.

Not a general format converter. It does few formats on purpose, and spends the
complexity budget on being a reliable *processor* instead — one that runs for
months, reports what it is doing, and recovers from cameras misbehaving.

## The constraint everything follows from

WebM is a Matroska subset that permits only **VP8/VP9/AV1** video and
**Vorbis/Opus** audio. RTSP cameras send **H.264/H.265** with AAC or G.711.

So RTSP → WebM is never a remux. It always decodes and re-encodes:

| Path | Cost per 1080p camera | Output |
|---|---|---|
| Passthrough (remux) | ~1% of a core | fMP4 / MKV — **not** WebM |
| Transcode to WebM | ~1–2 cores (software) | WebM |

Hardware help is uneven: NVENC has never encoded VP8 or VP9, Intel QSV does
VP9 only on recent parts, and AV1 encode is limited to RTX 40 / Arc / RDNA3.
That is the real argument for AV1 as the long-term target.

`JobSpec::validate` rejects impossible combinations before any I/O happens, so
you never end up with files no browser will play.

## Status

Working and tested:

- **EBML/WebM muxer** (`src/mux/`) — written for recording, not conversion.
  The Segment stays unknown-sized while open and clusters are buffered then
  written whole, so a file is playable while growing and a crash costs at most
  one cluster. Clusters split on keyframes and on a 5s cap, which also keeps
  block offsets inside Matroska's signed 16-bit field.
- **Timeline normalisation** (`src/source/timeline.rs`) — absorbs NTP clock
  corrections, RTP wraparound, stalls and camera reboots into a monotonic
  timeline, and reports every correction instead of hiding it. This is where
  multi-day recordings usually break.
- **RTSP source** (`src/source/rtsp.rs`) — built on `retina`, which handles
  digest auth, interleaved TCP, and RTP depacketisation into access units.
- **Job and reporting model** (`src/job/`) — job spec, state machine, and a
  typed event stream (segment started/finished, source lost, timestamp
  discontinuity, dropped media, progress). `Reporter` is a trait, so a gRPC or
  HTTP surface is an implementation rather than a rewrite.
- **H.264 CABAC decoder prototype** (`src/codec/h264/`) — Annex B access-unit
  parsing, SPS/PPS lifetime, CABAC macroblock syntax, intra/inter prediction,
  residual reconstruction, deblocking, cropped YUV420 output, and a small
  decoded-picture buffer. It currently accepts 8-bit, 4:2:0, progressive CABAC
  I/P pictures without FMO slice groups.

In progress:

- **H.264 decoder validation** — the prototype decodes a real camera capture
  through 224 pictures and emits the expected 1920×1080 first frame, but it
  does not yet match the OpenH264 oracle exactly. The current first divergence
  is in frame 0, luma macroblock `(76, 0)`. The differential harness reports
  the first bad sample to make this debugging tractable.
- **Encode and recording pipeline** — AV1 encoding, the job supervisor loop
  that drives segment rollover/reconnect backoff, and the transport surface
  are still not built.

## H.264 decoder development

The decoder is deliberately pure Rust. `media::Decoder` keeps it replaceable,
and the optional OpenH264 integration is retained strictly as a differential
testing oracle, never as the intended shipping decoder.

Validation is exact, sample-by-sample rather than visual. A one-bit CABAC,
prediction, or transform mistake quickly propagates inside and across pictures;
the useful failure is therefore the first differing coordinate, not an average
quality score. Run it on real Annex B camera captures before treating any
decoder path as reliable.

## Layout

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

Decode the first picture in an Annex B H.264 elementary stream into a luma PGM
for inspection:

    cargo run --example decode_h264 -- camera.h264 first-frame.pgm

Compare every decoded sample against the optional OpenH264 oracle. This builds
vendored C and is development-only:

    cargo run --features reference-decoder --example diff_h264 -- camera.h264

Emit a structurally complete WebM and check it with a parser that shares no
code with the muxer:

    cargo run --example write_webm -- out.webm
    python tools/ebml_check.py out.webm

Point the ingest path at a real camera. This is the whole input side end to
end -- DESCRIBE/SETUP/PLAY, codec identification, depacketisation into access
units, timeline normalisation -- and it reports bitrate, GOP length, worst
gaps and every timeline correction:

    cargo run --example rtsp_probe -- rtsp://host/stream --user U --pass P --secs 20

Add `--dump probe.h264` to write the video elementary stream out as Annex B
with parameter sets on every keyframe, so `ffprobe`/`ffplay` can confirm what
we ingested without trusting any of our code. There is no WebM output from a
camera yet; that waits on the decode and encode stages.

The AV1 encoder is behind a feature flag, since it dominates build time and is
only needed on the transcode path:

    cargo build --features av1
