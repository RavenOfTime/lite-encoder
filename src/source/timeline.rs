//! Mapping camera timestamps onto a monotonic recording timeline.
//!
//! This is where multi-day recordings usually break. Real cameras:
//!
//! - correct their clock over NTP mid-stream, jumping the timeline forwards
//!   or backwards by seconds;
//! - wrap their 32-bit RTP timestamp roughly every 13 hours at 90 kHz;
//! - stall and then resume, leaving a real gap in wallclock;
//! - restart entirely, resetting their timestamps to near zero.
//!
//! A recorder that feeds those values straight into a container produces
//! files that seek wrongly or refuse to play. `Timeline` absorbs them into a
//! non-decreasing output timeline and *reports* every correction, rather than
//! hiding it.
//!
//! # One clock domain, several tracks
//!
//! A `Timeline` models the clock of a whole presentation, not of one track.
//! Video and audio arrive interleaved and each runs on its own packet
//! cadence, so at any instant one of them is behind the other: a camera can
//! emit video for 201 ms and then audio for 128 ms without anything being
//! wrong. Treating that as a backwards jump produces a flood of bogus
//! discontinuities and, worse, bridges each one — inflating recorded duration
//! and destroying A/V sync.
//!
//! So the input-to-output *offset* is shared by every track, which
//! is what keeps audio and video aligned, while the *sequence* state used to
//! detect jumps is per track. Only the reference track (the video track, if
//! there is one) can trigger a rebase, because a clock correction happens to
//! the presentation as a whole and must be applied to all tracks once.

use crate::media::TrackId;
use std::collections::BTreeMap;
use std::time::Duration;

/// A backwards jump, or a forwards jump larger than this, is treated as a
/// discontinuity rather than as real elapsed media time.
const DEFAULT_MAX_JUMP: Duration = Duration::from_secs(10);

/// Substituted for the real delta when a discontinuity is bridged, so the
/// timeline advances slightly instead of stalling or leaping.
const BRIDGE_STEP: Duration = Duration::from_millis(33);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineEvent {
    /// Input timestamps moved in a way that cannot reflect real elapsed
    /// time. The output timeline was bridged instead.
    Discontinuity {
        /// The reference track the jump was observed on.
        track: TrackId,
        /// What the input claimed.
        got: Duration,
        /// What we last saw on the input.
        previous: Duration,
    },
}

/// Per-track sequence state. Shared offsets live on `Timeline` itself.
#[derive(Default, Clone, Copy)]
struct TrackState {
    last_in: Duration,
    last_out: Duration,
    seen: bool,
}

/// Normalises source timestamps to a monotonic timeline starting at zero.
pub struct Timeline {
    /// Nanoseconds added to any input timestamp to place it on the output
    /// timeline; `None` until the first packet anchors it. Shared by every
    /// track, and *signed*: a track that trails the reference has to be able
    /// to land before the point where the reference last rebased, or a clock
    /// correction would silently collapse the A/V offset to zero.
    offset: Option<i128>,
    /// Output value the next anchor maps to: zero for a fresh job, or the
    /// post-gap position set by `reconnected`.
    anchor_out: Duration,
    /// The track allowed to declare a discontinuity. Defaults to whichever
    /// track delivers first if the caller does not name one.
    reference: Option<TrackId>,
    tracks: BTreeMap<TrackId, TrackState>,
    /// Highest output emitted on any track, i.e. recorded media duration.
    position: Duration,
    max_jump: Duration,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Timeline {
    pub fn new() -> Self {
        Timeline {
            offset: None,
            anchor_out: Duration::ZERO,
            reference: None,
            tracks: BTreeMap::new(),
            position: Duration::ZERO,
            max_jump: DEFAULT_MAX_JUMP,
        }
    }

    /// Name the track whose clock defines the presentation.
    ///
    /// Video is the right choice when there is video: its cadence is regular,
    /// and camera audio is often generated from a free-running clock that
    /// drifts against it.
    pub fn with_reference(mut self, track: TrackId) -> Self {
        self.reference = Some(track);
        self
    }

    pub fn with_max_jump(mut self, max_jump: Duration) -> Self {
        self.max_jump = max_jump;
        self
    }

    /// Map an input timestamp for `track`, returning the output timestamp and
    /// any correction that had to be applied.
    ///
    /// Inputs must share one origin across tracks (for RTSP that means time
    /// relative to the `RTP-Info` start, not raw per-stream RTP values), since
    /// the offset between them is what A/V sync is made of.
    pub fn map(&mut self, track: TrackId, input: Duration) -> (Duration, Option<TimelineEvent>) {
        let reference = *self.reference.get_or_insert(track);
        let state = self.tracks.get(&track).copied().unwrap_or_default();

        let mut event = None;
        if self.offset.is_none() {
            // First packet of the presentation anchors the mapping.
            self.offset = Some(nanos(self.anchor_out) - nanos(input));
        } else if track == reference && state.seen {
            let backwards = input < state.last_in;
            let jumped = input.saturating_sub(state.last_in) > self.max_jump;
            if backwards || jumped {
                event = Some(TimelineEvent::Discontinuity {
                    track,
                    got: input,
                    previous: state.last_in,
                });
                // Rebase so subsequent packets are measured from the new input
                // value, and step the output forward just enough to stay
                // increasing. Every track moves together: the camera has one
                // clock, so a correction to it is not a per-track event, and
                // shifting the shared offset keeps their spacing intact.
                let base = self.position + BRIDGE_STEP;
                self.offset = Some(nanos(base) - nanos(input));
            }
        }

        let offset = self.offset.expect("anchored above");
        // A track can sit before the anchor: audio that trails video at
        // startup, or any non-reference track right after a rebase. Those
        // clamp to zero rather than underflowing.
        let out = duration(nanos(input) + offset);
        // Packets may repeat a timestamp; a track never goes backwards. This
        // is deliberately per track, because two tracks interleaving is not a
        // timeline violation.
        let out = out.max(state.last_out);

        let entry = self.tracks.entry(track).or_default();
        entry.last_in = input;
        entry.last_out = out;
        entry.seen = true;
        self.position = self.position.max(out);
        (out, event)
    }

    /// Mark a source reconnect.
    ///
    /// The camera's timestamps after a reconnect bear no relation to those
    /// before it, so the next packet rebases. `gap` is the wallclock time the
    /// source was away; pass `Duration::ZERO` to close the hole instead of
    /// preserving it.
    pub fn reconnected(&mut self, gap: Duration) {
        self.anchor_out = self.position + gap;
        self.position = self.anchor_out;
        self.offset = None;
        // Pre-outage sequence state would read as a giant jump. Dropping it
        // is safe because `out_base` is already past every value emitted.
        self.tracks.clear();
    }

    /// Latest output timestamp emitted on any track, i.e. recorded media
    /// duration.
    pub fn position(&self) -> Duration {
        self.position
    }
}

fn nanos(d: Duration) -> i128 {
    d.as_nanos() as i128
}

fn duration(ns: i128) -> Duration {
    if ns <= 0 {
        Duration::ZERO
    } else {
        Duration::from_nanos(ns as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: TrackId = TrackId(1);
    const A: TrackId = TrackId(2);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn first_packet_defines_zero() {
        let mut t = Timeline::new();
        // A camera that has been up for a week starts at a huge timestamp.
        let (out, ev) = t.map(V, Duration::from_secs(604_800));
        assert_eq!(out, Duration::ZERO);
        assert!(ev.is_none());
    }

    #[test]
    fn steady_stream_passes_through_as_deltas() {
        let mut t = Timeline::new();
        t.map(V, ms(10_000));
        assert_eq!(t.map(V, ms(10_040)).0, ms(40));
        assert_eq!(t.map(V, ms(10_080)).0, ms(80));
        assert_eq!(t.position(), ms(80));
    }

    #[test]
    fn ntp_correction_backwards_is_bridged_not_replayed() {
        let mut t = Timeline::new();
        t.map(V, ms(10_000));
        let (a, _) = t.map(V, ms(10_040));

        // Camera's clock is corrected 3s backwards.
        let (b, ev) = t.map(V, ms(7_040));
        assert!(matches!(ev, Some(TimelineEvent::Discontinuity { .. })));
        assert!(b > a, "timeline must not go backwards");

        // Subsequent packets continue from the new base.
        let (c, ev) = t.map(V, ms(7_080));
        assert!(ev.is_none());
        assert_eq!(c, b + ms(40));
    }

    #[test]
    fn large_forward_jump_is_treated_as_discontinuity() {
        let mut t = Timeline::new();
        t.map(V, ms(0));
        t.map(V, ms(40));
        // A 13-hour leap is a wrap or a clock jump, not real elapsed media.
        let (out, ev) = t.map(V, Duration::from_secs(46_800));
        assert!(matches!(ev, Some(TimelineEvent::Discontinuity { .. })));
        assert!(out < ms(200), "must not leap the output timeline");
    }

    #[test]
    fn small_gaps_are_preserved_as_real_time() {
        let mut t = Timeline::new();
        t.map(V, ms(0));
        // A 2s stall is under the threshold and is genuine dropped footage.
        let (out, ev) = t.map(V, ms(2_000));
        assert!(ev.is_none());
        assert_eq!(out, ms(2_000));
    }

    #[test]
    fn duplicate_timestamps_stay_monotonic() {
        let mut t = Timeline::new();
        t.map(V, ms(0));
        let (a, _) = t.map(V, ms(40));
        let (b, _) = t.map(V, ms(40));
        assert!(b >= a);
    }

    /// The bug a real Tapo camera exposed: video at ~10 fps interleaved with
    /// 128 ms G.711 audio packets meant each audio packet arrived "behind"
    /// the last video one. On a shared per-source timeline that read as 54
    /// clock corrections in 15 seconds, each bridged, inflating a 15 s
    /// recording to 17.9 s of media.
    #[test]
    fn interleaved_tracks_are_not_a_discontinuity() {
        let mut t = Timeline::new().with_reference(V);
        let video = [201u64, 301, 402, 502, 603, 703, 804, 904];
        let audio = [128u64, 256, 384, 512, 640, 768, 896, 1024];

        for (v, a) in video.iter().zip(audio.iter()) {
            let (_, ev) = t.map(V, ms(*v));
            assert!(ev.is_none(), "video {v} reported {ev:?}");
            let (_, ev) = t.map(A, ms(*a));
            assert!(ev.is_none(), "audio {a} reported {ev:?}");
        }

        // Media duration tracks the input, with no bridge steps added.
        // Audio runs furthest past the anchor, so it sets the duration.
        assert_eq!(t.position(), ms(1024 - 201));
    }

    #[test]
    fn tracks_share_one_offset_so_av_sync_survives() {
        let mut t = Timeline::new().with_reference(V);
        // Audio genuinely trails video by 70ms on the wire; that relationship
        // must appear unchanged on the output timeline.
        t.map(V, ms(1_000));
        let (v, _) = t.map(V, ms(1_100));
        let (a, _) = t.map(A, ms(1_030));
        assert_eq!(v, ms(100));
        assert_eq!(a, ms(30));
    }

    #[test]
    fn a_clock_correction_moves_every_track_together() {
        let mut t = Timeline::new().with_reference(V);
        t.map(V, ms(10_000));
        t.map(A, ms(9_970));
        t.map(V, ms(10_100));

        // Camera jumps its clock back 3s; video sees it first.
        let (v, ev) = t.map(V, ms(7_100));
        assert!(matches!(ev, Some(TimelineEvent::Discontinuity { .. })));

        // Audio's next packet still trails video by 30ms, and does *not*
        // report a second correction for the same jump.
        let (a, ev) = t.map(A, ms(7_070));
        assert!(ev.is_none(), "one clock, one correction");
        assert_eq!(v - a, ms(30), "A/V offset preserved across the jump");
    }

    #[test]
    fn only_the_reference_track_can_rebase() {
        let mut t = Timeline::new().with_reference(V);
        t.map(V, ms(0));
        t.map(A, ms(0));
        t.map(A, ms(100));
        // Audio alone goes backwards: report nothing, clamp, and leave the
        // shared offset for the reference track to decide.
        let (out, ev) = t.map(A, ms(20));
        assert!(ev.is_none());
        assert_eq!(out, ms(100), "clamped, not bridged");
        // Video is unaffected.
        assert_eq!(t.map(V, ms(40)).0, ms(40));
    }

    #[test]
    fn reconnect_rebases_and_can_preserve_the_gap() {
        let mut t = Timeline::new();
        t.map(V, ms(0));
        t.map(V, ms(1_000));

        t.reconnected(Duration::from_secs(30));
        // Camera rebooted, so its timestamps restart near zero.
        let (out, ev) = t.map(V, ms(5));
        assert!(ev.is_none(), "a rebase is expected, not a discontinuity");
        assert_eq!(out, ms(31_000), "gap preserved in the timeline");

        assert_eq!(t.map(V, ms(45)).0, ms(31_040));
    }

    #[test]
    fn reconnect_can_close_the_gap_instead() {
        let mut t = Timeline::new();
        t.map(V, ms(0));
        t.map(V, ms(1_000));
        t.reconnected(Duration::ZERO);
        assert_eq!(t.map(V, ms(9_999)).0, ms(1_000));
    }

    #[test]
    fn output_is_monotonic_under_adversarial_input() {
        let mut t = Timeline::new().with_reference(V);
        let inputs = [0u64, 40, 80, 5, 10_000_000, 120, 119, 200, 0, 240];
        let mut last = Duration::ZERO;
        for i in inputs {
            let (out, _) = t.map(V, ms(i));
            assert!(out >= last, "went backwards: {last:?} -> {out:?}");
            last = out;
        }
    }

    /// Each track is independently non-decreasing even when two tracks
    /// interleave adversarially; the global position never regresses either.
    #[test]
    fn every_track_is_independently_monotonic() {
        let mut t = Timeline::new().with_reference(V);
        let inputs = [0u64, 500, 40, 80, 20, 10_000_000, 120, 60, 200, 0, 240];
        let (mut last_v, mut last_a) = (Duration::ZERO, Duration::ZERO);
        for (n, i) in inputs.iter().enumerate() {
            let track = if n % 2 == 0 { V } else { A };
            let (out, _) = t.map(track, ms(*i));
            let last = if track == V { &mut last_v } else { &mut last_a };
            assert!(
                out >= *last,
                "{track:?} went backwards: {last:?} -> {out:?}"
            );
            *last = out;
        }
        assert!(t.position() >= last_v.max(last_a));
    }
}
