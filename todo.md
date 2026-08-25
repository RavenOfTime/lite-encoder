# lite-encoder completion checklist

This checklist targets a first useful release: reliably ingest an H.264 RTSP
camera, transcode it, and write playable WebM segments. H.265 and broader codec
support come after that vertical slice works.

## P0 — Stabilize the current H.264 work

- [x] Fix the two strict Clippy failures:
  - [x] Replace the `let ... else` in `src/codec/h264/loopfilter.rs` with `?`.
  - [x] Refactor or explicitly justify the 8-argument `intra_sample` helper in
        `src/codec/h264/recon.rs`.
- [ ] Run and pass `cargo fmt --check`, `cargo test`, and
      `cargo clippy --all-targets -- -D warnings`.
- [x] Run the OpenH264 differential suite with `--features reference-decoder`.
- [x] Run `diff_h264` over `camera.h264` and confirm every decoded sample still
      matches the reference for the full capture (224 pictures).
- [ ] Turn the current real-camera differential result into a repeatable
      regression test with a documented expected picture count/result.
- [ ] Add malformed/truncated-stream and multi-slice-picture integration tests
      so decoder errors never panic or silently emit corrupted frames.
- [ ] Document and test the exact supported H.264 subset, including explicit
      rejection tests for interlacing, non-8-bit video, non-4:2:0 chroma, FMO,
      CAVLC, and unsupported extensions.
- [x] **DONE (Auto)** Implement and differentially validate scaling lists and
      any remaining H.264 tools required by the target camera sample set.
      Fills optimization included: no full `mbs`/plane wipe on the hot path;
      `grey_uncovered` paints only unclaimed macroblocks. Scaling lists resolve
      SPS/PPS matrices (flat when absent) with scan→raster remap; 224-frame
      camera capture still bit-exact; ~129 fps at 1080p release.
- [ ] Benchmark release-mode decoding with `bench_h264` and record the minimum
      acceptable throughput for continuous 1080p input (with safety margin).
- [ ] Profile and optimize until the pure-Rust decoder meets that throughput
      target without changing bit-exact output.

## P1 — Complete the transcode path

- [ ] Implement a production AV1 `media::Encoder` backed by the optional
      `rav1e` dependency.
- [ ] Define timestamp/time-base handling between decoded `Frame`s, encoded
      `Packet`s, and the WebM muxer.
- [ ] Map encoder keyframes to WebM cluster/segment boundaries and make forced
      keyframes available for segment rollover.
- [ ] Implement encoder flushing and delayed-frame draining at rollover and
      end of stream.
- [ ] Decide the first-release audio behavior: transcode supported camera audio
      to Opus, or explicitly ship video-only and reject/drop audio with a
      reported event.
- [ ] Add deterministic decoder → encoder → WebM integration tests using a
      short checked-in fixture.
- [ ] Validate generated files with `tools/ebml_check.py` and an independent
      player/prober; check duration, dimensions, timestamps, keyframes, and
      clean end-of-stream behavior.

## P1 — Build the recording supervisor

- [ ] Add the job runner that validates `JobSpec`, opens the source, selects
      decoders/encoders, and drives packets through the muxer.
- [ ] Implement duration- and byte-based segment rollover from `SegmentPolicy`.
- [ ] Use temporary/in-progress names and atomic finalization so completed
      segments are distinguishable after a crash.
- [ ] Implement RTSP reconnect with bounded exponential backoff and
      `RetryPolicy::max_attempts`.
- [ ] Preserve a monotonic output timeline across reconnects and report every
      discontinuity/correction.
- [ ] Wire all job state transitions and operational events through `Reporter`.
- [ ] Implement graceful cancellation: stop intake, flush codecs, finish the
      current segment, report its result, then enter a terminal state.
- [ ] Define backpressure and bounded queues so a slow encoder or disk cannot
      grow memory without limit; report dropped media if dropping is allowed.
- [ ] Add end-to-end tests for rollover, camera loss/reconnect, cancellation,
      write failure, malformed input, and recovery after an incomplete segment.

## P2 — Make it operable

- [ ] Add a small CLI that loads a `JobSpec`, starts/stops a recording job, and
      emits structured progress/errors.
- [ ] Add the first external control/reporting surface (HTTP or gRPC) only after
      the in-process runner and `Reporter` contract are stable.
- [ ] Add structured tracing around connection attempts, decoder/encoder
      throughput, queue depth, dropped media, rollover, and output finalization.
- [ ] Ensure credentials are redacted from logs, errors, debug output, and
      serialized reports.
- [ ] Add disk-space/write-error handling and define retention/cleanup behavior
      for long-running recordings.
- [ ] Add CI for formatting, default tests, strict Clippy, the AV1 feature, and
      (where the C toolchain is available) reference-decoder tests.
- [ ] Test on the supported operating systems and document native build/runtime
      requirements for optional features.

## P2 — Release readiness

- [ ] Reconcile README status claims with automated evidence and benchmark
      results; avoid calling paths “working” unless CI exercises them.
- [ ] Decide whether large local artifacts (`camera.h264`, `first-frame.pgm`)
      belong in version control, test fixtures, Git LFS, or ignored local data.
- [ ] Add license, contribution notes, changelog, and an explicit stability/API
      policy for the `0.1` release.
- [ ] Document a reproducible camera → segmented WebM example, including how to
      inspect output and diagnose reconnect or performance problems.
- [ ] Run a multi-day soak test with forced disconnects, clock jumps, slow disk,
      and process termination; verify bounded memory and playable completed
      segments.
- [ ] Tag `v0.1.0` only after the end-to-end acceptance criteria below pass.

## P3 — Post-v0.1 roadmap

- [ ] Implement the pure-Rust H.265/HEVC decoder using the same restricted
      camera-profile, explicit-rejection, differential-oracle approach.
- [ ] Add H.265 RTSP → AV1/WebM end-to-end coverage.
- [ ] Add more output codecs/containers or ingest sources only when a concrete
      product use case justifies them.

## v0.1 acceptance criteria

- [ ] A supported H.264 RTSP camera can record segmented WebM continuously.
- [ ] Completed segments pass independent structural validation and playback.
- [ ] H.264 output is bit-exact against the reference corpus and fast enough
      for the documented 1080p workload.
- [ ] Disconnects recover according to policy without timestamp regression.
- [ ] Cancellation and failures leave prior completed segments playable.
- [ ] Memory and queue sizes remain bounded during a multi-day soak test.
- [ ] Formatting, tests, strict Clippy, AV1 builds, and integration tests pass
      in CI from a clean checkout.

## Current baseline (2026-08-26)

- `cargo test`: H.264 lib tests pass (scaling-list coverage added).
- `cargo test --features reference-decoder codec::h264::differential`: 10/10.
- `diff_h264 camera.h264`: 224 frames match exactly.
- `bench_h264` release 1080p: ~129 fps (7.8 ms/frame, ~4.3× real time at 30 fps).
- `cargo clippy --all-targets -- -D warnings`: passes.
- The working tree contains uncommitted H.264 decoder/performance changes; keep
  them intact while completing and validating this checklist.
