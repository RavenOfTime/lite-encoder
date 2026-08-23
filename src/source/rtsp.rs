//! RTSP ingest, built on `retina`.
//!
//! `retina` handles the parts of RTSP that are tedious and easy to get subtly
//! wrong: digest auth, interleaved TCP framing, RTP depacketisation into whole
//! access units, and SPS/PPS discovery. What we add on top is the part that
//! matters for recording: normalising timestamps onto a monotonic timeline
//! (see [`crate::source::Timeline`]) and surfacing every correction as an event.

use crate::job::{Input, RtspTransport};
use crate::media::{Codec, Packet, Track, TrackId, TrackKind};
use crate::source::{Source, Timeline, TimelineEvent};
use crate::{Error, Result};

use futures::StreamExt;
use retina::client::{
    Credentials, PlayOptions, SessionOptions, SetupOptions, TcpTransportOptions, Transport,
    UdpTransportOptions,
};
use retina::codec::{CodecItem, FrameFormat, ParametersRef};
use std::time::Duration;

pub struct RtspSource {
    session: retina::client::Demuxed,
    tracks: Vec<Track>,
    /// Maps a retina stream index to our track id.
    stream_to_track: Vec<Option<TrackId>>,
    timeline: Timeline,
    pending: Vec<TimelineEvent>,
}

impl RtspSource {
    /// DESCRIBE, SETUP every stream we understand, then PLAY.
    pub async fn connect(input: &Input) -> Result<Self> {
        let Input::Rtsp {
            url,
            username,
            password,
            transport,
        } = input
        else {
            return Err(Error::Spec("RtspSource requires an Rtsp input".into()));
        };

        let parsed = url::Url::parse(url).map_err(|e| Error::Spec(format!("bad RTSP url: {e}")))?;

        let creds = match (username, password) {
            (Some(u), Some(p)) => Some(Credentials {
                username: u.clone(),
                password: p.clone(),
            }),
            _ => None,
        };

        let mut session =
            retina::client::Session::describe(parsed, SessionOptions::default().creds(creds))
                .await
                .map_err(|e| Error::Source(format!("DESCRIBE failed: {e}")))?;

        let transport = match transport {
            RtspTransport::Tcp => Transport::Tcp(TcpTransportOptions::default()),
            RtspTransport::Udp => Transport::Udp(UdpTransportOptions::default()),
        };

        // Track ids start at 1: Matroska reserves 0.
        let mut tracks = Vec::new();
        let mut stream_to_track = vec![None; session.streams().len()];
        let mut next_id = 1u32;

        // Indexed rather than iterated on purpose: `setup` needs `&mut
        // session` inside the loop, so we cannot hold the `streams()` borrow.
        #[allow(clippy::needless_range_loop)]
        for i in 0..session.streams().len() {
            let s = &session.streams()[i];
            let Some(codec) = codec_of(s.media(), s.encoding_name()) else {
                tracing::warn!(
                    stream = i,
                    media = s.media(),
                    encoding = s.encoding_name(),
                    "skipping unsupported stream"
                );
                continue;
            };

            // Parameters are usually present after DESCRIBE, but some cameras
            // only reveal them in-band. Fall back to placeholder geometry and
            // correct it once the first frame arrives.
            let kind = match s.parameters() {
                Some(ParametersRef::Video(v)) => {
                    let (w, h) = v.pixel_dimensions();
                    TrackKind::Video {
                        width: w,
                        height: h,
                    }
                }
                Some(ParametersRef::Audio(a)) => TrackKind::Audio {
                    sample_rate: a.clock_rate(),
                    channels: a.channels().get() as u8,
                },
                _ if codec.is_video() => TrackKind::Video {
                    width: 0,
                    height: 0,
                },
                _ => TrackKind::Audio {
                    sample_rate: 8000,
                    channels: 1,
                },
            };

            let extra_data = match s.parameters() {
                Some(ParametersRef::Video(v)) => v.extra_data().to_vec(),
                Some(ParametersRef::Audio(a)) => a.extra_data().to_vec(),
                _ => Vec::new(),
            };

            // `SIMPLE` framing: Annex B start codes with SPS/PPS prepended to
            // every keyframe, and ADTS-wrapped AAC. Cameras vary in whether they
            // send parameter sets in-band, and a stream that only carries them
            // in the SDP is undecodable after a reconnect. This makes every
            // keyframe self-contained, which is what a decoder wants and what
            // makes a dumped elementary stream playable on its own. The
            // alternative (`MP4`: length-prefixed, parameters out of band) buys
            // nothing here, because H.264 never reaches our container.
            session
                .setup(
                    i,
                    SetupOptions::default()
                        .transport(transport.clone())
                        .frame_format(FrameFormat::SIMPLE),
                )
                .await
                .map_err(|e| Error::Source(format!("SETUP stream {i} failed: {e}")))?;

            let id = TrackId(next_id);
            next_id += 1;
            stream_to_track[i] = Some(id);
            tracks.push(Track {
                id,
                codec,
                kind,
                extra_data,
            });
        }

        if tracks.is_empty() {
            return Err(Error::Source(
                "no supported streams in RTSP presentation".into(),
            ));
        }

        let session = session
            .play(PlayOptions::default())
            .await
            .map_err(|e| Error::Source(format!("PLAY failed: {e}")))?
            .demuxed()
            .map_err(|e| Error::Source(format!("demux setup failed: {e}")))?;

        // Video drives the timeline when present: its cadence is regular,
        // while camera audio often runs off a free-running clock.
        let timeline = match tracks.iter().find(|t| t.codec.is_video()) {
            Some(t) => Timeline::new().with_reference(t.id),
            None => Timeline::new(),
        };

        Ok(RtspSource {
            session,
            tracks,
            stream_to_track,
            timeline,
            pending: Vec::new(),
        })
    }

    /// Timeline corrections observed since the last call.
    ///
    /// The caller drains these into the job's event stream, so clock jumps
    /// show up in the record rather than being silently absorbed.
    pub fn take_events(&mut self) -> Vec<TimelineEvent> {
        std::mem::take(&mut self.pending)
    }

    /// Recorded media position.
    pub fn position(&self) -> Duration {
        self.timeline.position()
    }

    /// Rebase after a reconnect, optionally preserving the outage as a gap.
    pub fn reconnected(&mut self, gap: Duration) {
        self.timeline.reconnected(gap);
    }

    fn map_ts(&mut self, track: TrackId, ts: retina::Timestamp) -> Duration {
        // `elapsed`, not `timestamp`: the raw value is a per-stream RTP
        // counter that starts at a random offset, so video and audio raw
        // values are not comparable. `elapsed` is measured from the stream
        // start announced in the RTSP `RTP-Info` header, which gives both
        // tracks one origin — and that shared origin is what A/V sync is.
        //
        // It is in clock-rate units and already unwrapped by retina, so it is
        // safe to widen to nanoseconds without overflowing for any realistic
        // recording length. A packet may precede the announced start by a
        // frame or two; clamping those to zero costs nothing, because the
        // timeline anchors on the first packet regardless.
        let rate = ts.clock_rate().get() as u64;
        let units = ts.elapsed().max(0) as u64;
        let secs = units / rate;
        let rem = units % rate;
        let d = Duration::new(secs, ((rem * 1_000_000_000) / rate) as u32);

        let (out, ev) = self.timeline.map(track, d);
        if let Some(ev) = ev {
            self.pending.push(ev);
        }
        out
    }
}

impl Source for RtspSource {
    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    async fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            let item = match self.session.next().await {
                None => return Ok(None),
                Some(Ok(item)) => item,
                Some(Err(e)) => return Err(Error::Source(e.to_string())),
            };

            let (stream_id, ts, keyframe, data) = match item {
                CodecItem::VideoFrame(f) => (
                    f.stream_id(),
                    f.timestamp(),
                    f.is_random_access_point(),
                    bytes::Bytes::copy_from_slice(f.data()),
                ),
                CodecItem::AudioFrame(f) => (
                    f.stream_id(),
                    f.timestamp(),
                    true,
                    bytes::Bytes::copy_from_slice(f.data()),
                ),
                // RTCP carries the wallclock mapping and keepalives; neither
                // produces media. Message frames (ONVIF metadata) are ignored.
                // `CodecItem` is non-exhaustive, so anything new is skipped
                // rather than breaking the build on a dependency bump.
                _ => continue,
            };

            let Some(Some(track)) = self.stream_to_track.get(stream_id).copied() else {
                continue;
            };

            let pts = self.map_ts(track, ts);
            return Ok(Some(Packet {
                track,
                pts,
                keyframe,
                data,
            }));
        }
    }
}

/// Map an SDP media type and encoding name to our codec set.
///
/// Returning `None` means "we do not carry this", which is the whole point of
/// a narrow processor: unsupported streams are skipped loudly, not guessed at.
fn codec_of(media: &str, encoding: &str) -> Option<Codec> {
    match (media, encoding.to_ascii_lowercase().as_str()) {
        ("video", "h264") => Some(Codec::H264),
        ("video", "h265" | "hevc") => Some(Codec::H265),
        ("audio", "mpeg4-generic" | "aac") => Some(Codec::Aac),
        ("audio", "opus") => Some(Codec::Opus),
        ("audio", "pcmu") => Some(Codec::Pcmu),
        ("audio", "pcma") => Some(Codec::Pcma),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_codecs_cameras_actually_send() {
        assert_eq!(codec_of("video", "H264"), Some(Codec::H264));
        assert_eq!(codec_of("video", "H265"), Some(Codec::H265));
        assert_eq!(codec_of("audio", "MPEG4-GENERIC"), Some(Codec::Aac));
        assert_eq!(codec_of("audio", "PCMU"), Some(Codec::Pcmu));
    }

    #[test]
    fn unsupported_streams_are_skipped_not_guessed() {
        assert_eq!(codec_of("application", "vnd.onvif.metadata"), None);
        assert_eq!(codec_of("video", "JPEG"), None);
    }

    #[test]
    fn no_camera_codec_can_go_straight_into_webm() {
        // The constraint the whole design turns on: every codec an RTSP
        // camera sends is illegal in WebM, so passthrough can never target it.
        for (m, e) in [
            ("video", "h264"),
            ("video", "h265"),
            ("audio", "mpeg4-generic"),
            ("audio", "pcmu"),
        ] {
            let c = codec_of(m, e).unwrap();
            assert!(!c.webm_legal(), "{c:?} unexpectedly WebM-legal");
        }
    }
}
