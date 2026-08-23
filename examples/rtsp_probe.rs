//! Point the RTSP ingest path at a real camera and report what came back.
//!
//! This exercises everything that exists today on the input side: DESCRIBE /
//! SETUP / PLAY, codec identification, RTP depacketisation into access units,
//! and timeline normalisation. It deliberately does *not* decode or encode,
//! because those stages are not built yet.
//!
//!     cargo run --example rtsp_probe -- rtsp://host/stream \
//!         --user admin --pass secret --secs 20 --dump probe.h264
//!
//! A dump is Annex B with parameter sets on every keyframe, so it plays
//! directly (`ffplay probe.h264`) without anything of ours being trusted.

use lite_encoder::job::{Input, RtspTransport};
use lite_encoder::media::{TrackId, TrackKind};
use lite_encoder::source::{RtspSource, Source};

use std::collections::BTreeMap;
use std::io::Write;
use std::time::{Duration, Instant};

/// A camera that answers at all answers quickly; one that does not is the
/// interesting failure, and we want it reported rather than hung on.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Longer than any sane keyframe interval, so tripping this means a stall.
const PACKET_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Stats {
    packets: u64,
    bytes: u64,
    keyframes: u64,
    first_pts: Option<Duration>,
    last_pts: Duration,
    /// Largest step in *media* time between consecutive packets.
    max_pts_gap: Duration,
    /// Largest step in *wallclock* time, which is what a viewer feels.
    max_wall_gap: Duration,
    last_wall: Option<Instant>,
    smallest: u64,
    largest: u64,
    /// Media time between keyframes, i.e. the seek granularity a segmenter
    /// has to work with.
    max_gop: Duration,
    last_key_pts: Option<Duration>,
    /// Should never fire: the timeline guarantees non-decreasing output.
    backwards: u64,
}

impl Stats {
    fn observe(&mut self, pts: Duration, keyframe: bool, len: u64, now: Instant) {
        if self.packets > 0 {
            if pts < self.last_pts {
                self.backwards += 1;
            }
            self.max_pts_gap = self.max_pts_gap.max(pts.saturating_sub(self.last_pts));
        }
        if let Some(prev) = self.last_wall {
            self.max_wall_gap = self.max_wall_gap.max(now - prev);
        }
        if keyframe {
            if let Some(k) = self.last_key_pts {
                self.max_gop = self.max_gop.max(pts.saturating_sub(k));
            }
            self.last_key_pts = Some(pts);
            self.keyframes += 1;
        }
        self.first_pts.get_or_insert(pts);
        self.last_pts = pts;
        self.last_wall = Some(now);
        self.smallest = if self.packets == 0 {
            len
        } else {
            self.smallest.min(len)
        };
        self.largest = self.largest.max(len);
        self.packets += 1;
        self.bytes += len;
    }

    /// Media duration actually covered, which is not the same as how long we
    /// sat there: a camera can deliver 3s of media in 20s of wallclock.
    fn span(&self) -> Duration {
        self.last_pts
            .saturating_sub(self.first_pts.unwrap_or_default())
    }
}

struct Args {
    url: String,
    username: Option<String>,
    password: Option<String>,
    transport: RtspTransport,
    secs: u64,
    dump: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut url = None;
    let mut args = Args {
        url: String::new(),
        username: None,
        password: None,
        transport: RtspTransport::Tcp,
        secs: 15,
        dump: None,
    };

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--user" => args.username = Some(value("--user")?),
            "--pass" => args.password = Some(value("--pass")?),
            "--secs" => {
                args.secs = value("--secs")?
                    .parse()
                    .map_err(|_| "--secs must be a number".to_string())?
            }
            "--dump" => args.dump = Some(value("--dump")?),
            "--udp" => args.transport = RtspTransport::Udp,
            "-h" | "--help" => return Err("help".into()),
            other if other.starts_with('-') => return Err(format!("unknown flag {other}")),
            other => url = Some(other.to_string()),
        }
    }

    args.url = url.ok_or("an rtsp:// URL is required")?;
    Ok(args)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,lite_encoder=info".into()),
        )
        .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if e != "help" {
                eprintln!("error: {e}\n");
            }
            eprintln!(
                "usage: rtsp_probe <rtsp url> [--user U] [--pass P] [--udp] \
                 [--secs N] [--dump FILE]"
            );
            std::process::exit(2);
        }
    };

    if let Err(e) = probe(args).await {
        eprintln!("\nFAILED: {e}");
        std::process::exit(1);
    }
}

async fn probe(args: Args) -> Result<(), String> {
    let input = Input::Rtsp {
        url: args.url.clone(),
        username: args.username,
        password: args.password,
        transport: args.transport,
    };

    // Credentials are in the spec, not in the log line.
    println!("connecting to {} over {:?}", args.url, args.transport);
    let began = Instant::now();

    let mut source = tokio::time::timeout(CONNECT_TIMEOUT, RtspSource::connect(&input))
        .await
        .map_err(|_| format!("no response within {CONNECT_TIMEOUT:?} (DESCRIBE/SETUP/PLAY)"))?
        .map_err(|e| e.to_string())?;

    println!("connected in {:?}\n", began.elapsed());
    println!("tracks:");
    let mut video_track = None;
    for t in source.tracks() {
        match t.kind {
            TrackKind::Video { width, height } => {
                if video_track.is_none() {
                    video_track = Some(t.id);
                }
                println!(
                    "  {:?} video {:?} {}x{}  extra_data {} bytes",
                    t.id,
                    t.codec,
                    width,
                    height,
                    t.extra_data.len()
                );
            }
            TrackKind::Audio {
                sample_rate,
                channels,
            } => println!(
                "  {:?} audio {:?} {} Hz {}ch  extra_data {} bytes",
                t.id,
                t.codec,
                sample_rate,
                channels,
                t.extra_data.len()
            ),
        }
    }

    // Opening the dump only after tracks are known means a failed connection
    // never leaves a zero-byte file behind to be mistaken for a result.
    let mut dump = match (&args.dump, video_track) {
        (Some(path), Some(id)) => {
            let f = std::fs::File::create(path).map_err(|e| format!("cannot write {path}: {e}"))?;
            println!("\ndumping {id:?} to {path}");
            Some((id, std::io::BufWriter::new(f)))
        }
        (Some(_), None) => return Err("--dump given but the camera has no video track".into()),
        (None, _) => None,
    };

    let deadline = Instant::now() + Duration::from_secs(args.secs);
    let mut stats: BTreeMap<TrackId, Stats> = BTreeMap::new();
    let mut discontinuities = 0u64;
    let mut ended_early = false;

    println!("\nreading for {}s...", args.secs);
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        let stalled = left > PACKET_TIMEOUT;
        let packet = match tokio::time::timeout(left.min(PACKET_TIMEOUT), source.next_packet())
            .await
        {
            // Whether a timeout is a stall or just the end of the run
            // depends on which of the two clocks ran out first.
            Err(_) if stalled => return Err(format!("stalled: no packet for {PACKET_TIMEOUT:?}")),
            Err(_) => break,
            Ok(Err(e)) => return Err(format!("stream error after {:?}: {e}", began.elapsed())),
            Ok(Ok(None)) => {
                ended_early = true;
                break;
            }
            Ok(Ok(Some(p))) => p,
        };

        stats.entry(packet.track).or_default().observe(
            packet.pts,
            packet.keyframe,
            packet.data.len() as u64,
            Instant::now(),
        );

        if let Some((id, w)) = dump.as_mut() {
            if packet.track == *id {
                w.write_all(&packet.data)
                    .map_err(|e| format!("dump write failed: {e}"))?;
            }
        }

        for ev in source.take_events() {
            discontinuities += 1;
            println!("  timeline: {ev:?}");
        }
    }

    if let Some((_, w)) = dump.as_mut() {
        w.flush().map_err(|e| format!("dump flush failed: {e}"))?;
    }

    let wall = began.elapsed();
    println!("\n--- after {:.1}s wallclock ---", wall.as_secs_f64());

    for t in source.tracks() {
        let Some(s) = stats.get(&t.id) else {
            println!("\n{:?} {:?}: NO PACKETS", t.id, t.codec);
            continue;
        };
        let span = s.span().as_secs_f64();
        println!("\n{:?} {:?}:", t.id, t.codec);
        println!(
            "  {} packets, {} keyframes, {:.2} MiB",
            s.packets,
            s.keyframes,
            s.bytes as f64 / (1024.0 * 1024.0)
        );
        println!(
            "  media span {:.2}s (pts {:.2}s..{:.2}s)",
            span,
            s.first_pts.unwrap_or_default().as_secs_f64(),
            s.last_pts.as_secs_f64()
        );
        if span > 0.0 {
            println!(
                "  {:.2} packets/s of media, {:.0} kbit/s",
                s.packets as f64 / span,
                (s.bytes as f64 * 8.0 / 1000.0) / span
            );
        }
        println!("  packet size {}..{} bytes", s.smallest, s.largest);
        println!(
            "  worst gap: {:.0} ms media, {:.0} ms wallclock",
            s.max_pts_gap.as_secs_f64() * 1000.0,
            s.max_wall_gap.as_secs_f64() * 1000.0
        );
        if t.codec.is_video() {
            println!("  longest GOP: {:.2}s", s.max_gop.as_secs_f64());
            if s.keyframes == 0 {
                println!("  WARNING: no keyframe in this window; segments cannot start");
            }
        }
        if s.backwards > 0 {
            println!(
                "  BUG: {} packets went backwards in time (Timeline must prevent this)",
                s.backwards
            );
        }
    }

    println!("\nverdict:");
    if ended_early {
        println!("  stream ended before the time limit");
    }
    println!("  {discontinuities} timeline correction(s)");
    // Media time lagging wallclock badly means we are not keeping up with the
    // camera, which on a recorder shows up later as drift, not as an error.
    if let Some(id) = video_track {
        if let Some(s) = stats.get(&id) {
            let lag = wall.as_secs_f64() - s.span().as_secs_f64();
            println!("  video media time trails wallclock by {lag:.2}s");
        }
    }
    println!("  ingest path OK; decode/encode stages are not built yet");

    if let Some(path) = &args.dump {
        println!("\ncheck the dump with something that shares no code with us:");
        println!("  ffprobe -hide_banner {path}");
        println!("  ffplay {path}");
    }
    Ok(())
}
