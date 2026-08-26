# lite-encoder

A **Rust-native ffmpeg alternative**: demux, decode, encode, and mux media through
one pipeline. Codecs are implemented in Rust where it matters; containers and
long-tail formats are added over time behind the same traits ffmpeg uses in
spirit — not one hard-coded camera path.

This is not “ffmpeg with every format on day one.” It is ffmpeg’s **shape**
(probe → demux → decode/filter → encode → mux) with a **prioritized format
matrix** and bit-exact validation on the codecs we own.

## Pipeline

```text
Input
  → Probe (sniff container + tracks)
  → Demuxer
  → Packet(s)
  → Decoder  ── or ── copy (remux)
  → Frame
  → Encoder  ── or ── copy
  → Muxer
  → Output
```

Planned CLI:

```text
./liteenc -i input.mp4 -c:v av1 -c:a copy output.webm
./liteenc -i input.h264 -c copy output.mkv
./liteenc probe input.mkv
```

Build the binary:

```text
cargo build --release
./target/release/liteenc --help
```

## What exists today

| Layer | Status |
|---|---|
| **H.264 decode** | Pure Rust, differentially tested vs OpenH264 (`reference-decoder`) |
| **AV1 encode** | rav1e, optional `--features av1` |
| **WebM mux** | EBML/Matroska subset, structurally validated (`tools/ebml_check.py`) |
| **Annex B read** | `demux::AnnexBDemuxer` — elementary H.264, one video track |
| **MKV mux/demux** | `mux::MkvMuxer` + `demux::MkvDemuxer` — full Matroska codec list (H.264 included); SimpleBlock only, 1 ms `TimestampScale` only |
| **`Demuxer` / `Muxer` traits** | `demux`, `mux`; impls: `AnnexBDemuxer`, `MkvDemuxer`, `WebmMuxer`, `MkvMuxer` |
| **`-c copy` remux** | `remux::copy_remux` — no decode, packets straight to muxer |
| **`Probe`** | `probe::probe()` — magic bytes + extension, container only (no track parse yet) |
| **Demux/mux (MP4/fMP4, TS)** | Not started |
| **CLI** | **`liteenc`** stub only — `./target/release/liteenc --help` |

### Out of scope (removed from product)

Live **RTSP ingest**, **recording jobs**, segment rollover, and camera-specific
gates were an earlier vertical slice. They are **not** part of this project
anymore. The core (codecs + mux + transcode path) stays.

## Format matrix (priority)

Legend: **Y** supported, **P** planned, **—** later / evaluate.

| Container | Read | Write | Notes |
|---|---|---|---|
| Annex B (`.h264`) | **Y** | P | Elementary; today’s bench path |
| WebM | — | **Y** | AV1/VP8/VP9 + Vorbis/Opus only |
| Matroska (`.mkv`) | **Y** | **Y** | Full codec list; SimpleBlock only, no lacing |
| MP4 / fMP4 | P | P | Remux + browser MSE path |
| MPEG-TS | P | — | Common broadcast/capture |

| Codec | Decode | Encode | Notes |
|---|---|---|---|
| H.264 | **Y** (Rust) | P | 8-bit 4:2:0 CABAC I/P subset |
| H.265 | P | P | Next decoder |
| AV1 | P | **Y** (rav1e) | Feature-gated |
| VP9 / VP8 | P | P | WebM targets |
| AAC / Opus / Vorbis | P | P | Audio long tail |

“All formats” means **this table grows row by row**, not a single monolithic
release. See [`todo.md`](todo.md) for the ordered checklist.

## Quality bar

- **Decoders we ship in Rust** are validated bit-exact against a reference
  oracle (OpenH264 for H.264 in CI only — never linked in default builds).
- **Mux output** is checked with tooling that shares no code with the muxer
  (`tools/ebml_check.py`).
- Wrong pixels fail on the **first differing coordinate**, not an average score.

## Layout

```text
src/codec/     decoders and encoders (H.264, AV1, …)
src/media/     Codec, Frame, Packet, Decoder/Encoder traits, time base
src/demux/     Demuxer trait; AnnexBDemuxer, MkvDemuxer
src/mux/       Muxer trait; EBML + shared Matroska core; WebmMuxer, MkvMuxer
src/probe.rs   container sniff (magic bytes + extension)
src/registry.rs  (container, codec) → read/write support table
src/remux.rs   `-c copy`: demuxer packets straight to a muxer, no decode
tools/         independent validators
examples/      decode benches, diff, write_webm, transcode experiments
tests/         fixtures and integration tests
```

Historical checklist items (camera RTSP, job runner, etc.) lived in older
revisions of `todo.md`; the current file tracks **ffmpeg-shaped** work only.

## Build

Rust **1.98** (`rust-toolchain.toml`).

```text
cargo test
cargo clippy --all-targets -- -D warnings
```

H.264 decode (first frame to PGM):

```text
cargo run --example decode_h264 -- input.h264 first-frame.pgm
```

Bit-exact check vs OpenH264 (dev only):

```text
cargo run --features reference-decoder --example diff_h264 -- input.h264
```

Synthetic WebM + structural check:

```text
cargo run --example write_webm -- out.webm
python tools/ebml_check.py out.webm
```

Transcode experiment (Annex B → AV1 WebM, `--features av1`):

```text
cargo run --release --features av1 --example bench_av1 -- input.h264
```

Throughput benches (release):

```text
cargo run --release --example bench_h264 -- input.h264
cargo run --release --features av1 --example bench_av1 -- input.h264
```

AV1 encode is behind `av1` (heavy build):

```text
cargo build --features av1
```
