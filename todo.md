# lite-encoder completion checklist

This checklist targets a first useful release: reliably ingest an H.264 RTSP
camera, transcode it, and write playable WebM segments. H.265 and broader codec
support come after that vertical slice works.

## P0 — Stabilize the current H.264 work

- [x] Fix the two strict Clippy failures:
  - [x] Replace the `let ... else` in `src/codec/h264/loopfilter.rs` with `?`.
  - [x] Refactor or explicitly justify the 8-argument `intra_sample` helper in
        `src/codec/h264/recon.rs`.
- [x] Run and pass `cargo fmt --check`, `cargo test`, and
      `cargo clippy --all-targets -- -D warnings`.
  - [x] `cargo test` (209 lib + 2 integration) and
        `cargo test --features reference-decoder` (223 lib + 3 integration)
        both pass.
  - [x] `cargo clippy --all-targets -- -D warnings` clean on default features
        and on `--features reference-decoder`.
  - [x] `cargo fmt --check` passes after formatting the working tree.
  - [x] `cargo build --features av1` passes.
- [x] Run the OpenH264 differential suite with `--features reference-decoder`.
- [x] Run `diff_h264` over `camera.h264` and confirm every decoded sample still
      matches the reference for the full capture (224 pictures).
- [x] **DONE (Auto):** Turn the current real-camera differential result into a
      repeatable regression test with a documented expected picture count/result.
      Fixture contract: 4 pictures @ 1920×1080 (`tests/fixtures/README.md`);
      default CI decodes them; `--features reference-decoder` asserts bit-exact
      OpenH264 match (`tests/h264_camera_regression.rs`).
- [ ] **PARTIAL (Codex):** Add malformed/truncated-stream and
      multi-slice-picture integration tests so decoder errors never panic or
      silently emit corrupted frames.
  - [x] Tail-truncation of the fixture's first access unit is rejected without
        panicking and without emitting a frame (`tests/h264_robustness.rs`).
  - [x] **DONE (Codex):** Multi-slice picture assembly now runs in default CI
        over a checked-in three-picture, two-CABAC-slices-per-picture fixture.
        It locks slice count, dimensions, timestamps, and pixel fingerprints;
        `reference-decoder` additionally proves bit-exact OpenH264 agreement.
  - [ ] **IN PROGRESS (Codex):** Truncation coverage is a single access unit cut at the tail. Add
        mid-stream truncation (a P access unit, not only the IDR), a corrupted
        NAL header, bit-flipped slice payloads, and a P slice arriving with an
        empty DPB.
- [ ] **PARTIAL (Codex):** Document and test the exact supported H.264 subset,
      including explicit rejection tests for interlacing, non-8-bit video,
      non-4:2:0 chroma, FMO, CAVLC, and unsupported extensions.
  - [x] Interlacing, bit depth, chroma format, FMO, and the
        partition/SVC/MVC/auxiliary/reserved NAL types are all rejected, with
        default-feature unit tests in `src/codec/h264/decoder.rs`.
  - [ ] CAVLC rejection is tested only behind `reference-decoder` (the test
        needs openh264 to synthesize a CAVLC stream). Default CI has no CAVLC
        rejection test; add one over a checked-in or hand-built slice header.
- [x] **DONE (Auto)** Implement and differentially validate scaling lists and
      any remaining H.264 tools required by the target camera sample set.
      Fills optimization included: no full `mbs`/plane wipe on the hot path;
      `grey_uncovered` paints only unclaimed macroblocks. Scaling lists resolve
      SPS/PPS matrices (flat when absent) with scan→raster remap; 224-frame
      camera capture still bit-exact; ~129 fps at 1080p release.
- [x] **DONE (Auto):** Benchmark release-mode decoding with `bench_h264` and
      record the minimum acceptable throughput for continuous 1080p input
      (with safety margin). Gate: **≥60 fps** at 1080p (2.0× @ 30 fps) on a
      ≥200-picture capture; `bench_h264` exits non-zero if it fails. Measured
      2026-08-26: ~124–129 fps (PASS, ~2.1× the floor); OpenH264 ~222 fps.
- [x] **DONE (Auto):** Profile and optimize until the pure-Rust decoder meets
      that throughput target without changing bit-exact output. Current release
      decode already clears the 60 fps gate with headroom; further SIMD work is
      optional, not a P0 blocker.

## P0 — Reference handling the decoder accepts but does not implement

Found 2026-08-26 while validating the current tree. Each of these is parsed
past and then ignored, so a stream that uses it decodes to **wrong pixels with
no error** — precisely the failure mode the differential strategy exists to
catch. The Tapo camera fixture *does* emit `ref_pic_list_modification`
(`Subtract(0)`) and adaptive MMCO on P slices; with `max_num_ref_frames == 1`
those happen to match the default list / sliding window, which is why bit-exact
checks passed while the syntax was ignored.

Project doctrine is to refuse what is not implemented, at the parameter-set or
slice-header stage, rather than half-implement it. Rejecting is the cheap fix
and should land first; implement only what the target camera set actually
needs, and only with differential proof.

- [x] **DONE (Codex): Honor `nal_ref_idc == 0`.** The NAL reference value is
      preserved in `SliceInfo`; access units mixing values are rejected; and
      disposable pictures are displayed then recycled without entering the
      DPB or displacing a real reference.
- [x] **DONE (Auto): Implement short-term `ref_pic_list_modification`.**
      Commands are carried on `CabacSlice::list_mods`; `Dpb::list0` applies
      spec 8.2.4.3.1 before inter prediction. Long-term modifications are
      rejected. The camera fixture's `Subtract(0)` path is covered by unit
      tests and still bit-exact on the regression fixture.
- [x] **DONE (Codex): Reject weighted P prediction.** PPS registration rejects
      `weighted_pred_flag`, and the standalone slice parser independently
      rejects any parsed `pred_weight_table`, preventing silent unweighted
      reconstruction.
- [x] **DONE (Auto): Short-term `dec_ref_pic_marking`.** Sliding window, IDR
      (no long-term), and adaptive MMCO ops 1 (`ShortTermUnused`) and 5
      (`AllUnused`) are applied via `Dpb::mark_reference` after decode.
      Long-term MMCO / IDR `long_term_reference_flag` are rejected. The Tapo
      fixture's adaptive mark-previous-unused path is covered by unit tests
      and still bit-exact on the regression fixture.
- [x] **DONE (Auto): Follow-up on the above: MMCO 5 does not reset `frame_num`.**
      Spec 8.2.5.4.5: `mark_reference` zeroes the current picture's stored
      `frame_num` after `AllUnused`; unit tests cover the reset and a
      `ref_pic_list_modification` reorder across it.
- [x] **DONE (Auto): Redundant coded pictures are not discarded.** PPS
      registration rejects `redundant_pic_cnt_present_flag`; slice parsing
      also rejects `redundant_pic_cnt > 0` as defense in depth. Unit test in
      `decoder.rs` alongside the other PPS rejection cases.
- [x] **DONE (Auto): `gaps_in_frame_num_value_allowed_flag` is not handled.**
      Decision: reject at SPS registration (do not synthesize non-existing
      frames). Surveillance cameras emit contiguous `frame_num` sequences;
      synthesizing gap placeholders would be half-implemented DPB logic with
      no target use case. Unit test in `decoder.rs`.
- [ ] Add a rejection (or differential) test per item above, and record in the
      README which of these the target camera actually exercises.

## P0 — Make concealment visible to the recorder

- [ ] `Picture::grey_uncovered` paints macroblocks that no slice claimed, and
      `decode_access_unit` then returns that picture as an ordinary `Frame`.
      A recorder cannot distinguish a clean picture from a half-lost one.
      Return the unclaimed-macroblock count (or a per-frame concealment flag)
      so the job layer can report it through `Reporter` and so a soak test can
      assert on it. Today the only signal is that the pixels are grey.

## P1 — Complete the transcode path

- [x] **DONE (Auto):** Implement a production AV1 `media::Encoder` backed by
      the optional `rav1e` dependency. `codec::av1::Av1Encoder`
      (`src/codec/av1/mod.rs`, behind the `av1` feature) uses the benchmarked
      speed-8/4-tile/low-latency configuration from `bench_rav1e.rs`
      (~2x real time at 640x360), tracks PTS per rav1e `input_frameno` so
      packets keep their originating frame's timestamp, and exposes
      `container_sequence_header()` as `extra_data()` for the WebM
      `CodecPrivate`. Also fixed a pre-existing strict-Clippy failure in
      `examples/bench_rav1e.rs` (`field_reassign_with_default`) that blocked
      `cargo clippy --all-targets --features av1`. Unit-tested in isolation
      (keyframe + PTS on first packet, size mismatch rejected, non-empty
      `av1C`, flush drains every sent frame) — **not yet wired into the job
      runner**, which does not exist yet either (see the next section).
- [ ] **No encode throughput gate exists.** The decode gate (≥60 fps at 1080p)
      is justified by reserving *half* the wall-clock for AV1 encode, but
      nothing measures whether encode actually fits in that half. The only
      rav1e number on record is ~2x real time at **640x360** from
      `bench_rav1e.rs`; 1080p is roughly 9x the pixels and has never been
      measured with the shipping `Av1Encoder` configuration. Extend
      `bench_rav1e` (or add `bench_av1`) to drive `codec::av1::Av1Encoder` over
      decoded 1080p camera frames, record the achieved fps, and set a pass/fail
      floor the way `bench_h264` does. If 1080p AV1 misses real time at speed
      8, that is a product decision (lower resolution, more threads, or a
      different encode target) and it should surface now, not after the runner
      is built.
- [ ] **`Av1Encoder::packet_from` invents a timestamp on a cache miss.**
      `self.pending.remove(&pkt.input_frameno).unwrap_or_default()` yields
      `Duration::ZERO` when the frame number is unknown, which the muxer would
      write as a real cluster timestamp. Contradicts the project's
      report-every-correction doctrine. Return `Error::Encode` instead, and
      assert `pending` is empty after `flush`.
- [ ] **`Av1Encoder::copy_plane` panics on a malformed `Frame`.** It slices
      `src[row * src_stride .. row * src_stride + width]` with no check that
      `strides` are at least `width` or that each plane is long enough.
      Frames from the H.264 decoder are always tight-strided so this is safe
      today, but `media::Encoder` is a public trait and a supervised recorder
      must not abort the process on bad input. Validate plane lengths and
      strides in `encode` and return `Error::Encode`.
- [ ] **`Av1Encoder` is only tested at 16x16 with even dimensions.** Add cases
      for odd width/height (chroma `div_ceil` and rav1e's padded plane stride)
      and for at least one realistic resolution, so the plane copy is not
      first exercised at 1080p by the job runner.
- [ ] Define timestamp/time-base handling between decoded `Frame`s, encoded
      `Packet`s, and the WebM muxer.
- [ ] Map encoder keyframes to WebM cluster/segment boundaries and make forced
      keyframes available for segment rollover.
- [ ] Implement encoder flushing and delayed-frame draining at rollover and
      end of stream. Note the H.264 side needs nothing here (no B slices, so
      `Frontend::flush` is correctly a no-op) — the buffering is the encoder's.
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
      Specifically: the README's "Supported H.264 input" section lists what is
      rejected but says nothing about the reference-handling features above
      that are silently accepted. Fix the code first, then the prose.
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

## P3 — Housekeeping

- [ ] `src/codec/h264/mod.rs`: the `pub mod` list is out of order (`loopfilter`
      sits after `picture`), and the module plan in the doc comment still names
      `cavlc` and `dpb` modules that were never created — `cavlc` is now
      explicitly out of scope and the DPB lives in `picture.rs`.

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

## Current baseline (2026-08-26, re-validated after commit d605277)

Measured against the committed tree, not carried forward from an earlier
session:

- `cargo fmt --check`: passes.
- `cargo test`: 209 lib tests + `h264_camera_regression` + `h264_robustness`,
  all pass.
- `cargo test --features reference-decoder`: 223 lib tests + 3 integration
  tests, all pass, including the differential cases and the bit-exact
  camera-fixture lock.
- `cargo test --features av1`: 215 lib tests + 2 integration tests, all pass;
  the 4 `codec::av1` tests are among them.
- `cargo clippy --all-targets -- -D warnings`: clean on default features, on
  `--features reference-decoder`, and on `--features av1`.
- `diff_h264 camera.h264`: 224 pictures bit-exact, divergence none — still
  exact after `nal_ref_idc`, list reordering, weighted-prediction rejection
  and MMCO landed.
- `bench_h264 camera.h264` release: best 130.2 fps at 1920x1080
  (7.68 ms/frame, 4.34x real time at 30 fps). Acceptance gate PASS at 2.17x
  the 60 fps floor; the example exits non-zero when it fails.
- No encode-side throughput number exists at 1080p. See the P1 gate item.

## Known scope limits that are correct, not bugs

Recorded so they are not re-filed as gaps:

- No B slices, so decode order is output order, POC is never computed, and
  `Frontend::flush` is legitimately a no-op.
- Multiple reference frames *are* supported: `ref_idx_l0` is decoded and the
  DPB is ordered most-recent-first, which is the spec's default P list.
- A P macroblock referencing an absent picture errors out rather than
  predicting from garbage (`recon.rs`), so reference loss is loud.
- OpenH264 appears only behind `reference-decoder` and is never linked into a
  shipping build — verified: no non-test call sites.
