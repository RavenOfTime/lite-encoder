//! What each `liteenc` subcommand actually does.
//!
//! Every path here funnels through the same four library pieces: sniff the
//! container ([`lite_encoder::probe`]), open a [`Demuxer`], ask the
//! [`registry`] whether the requested `(container, codec)` pair is something
//! this build can do, and write through a [`Muxer`]. Anything the registry
//! says no to fails before a single byte is written.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;

use lite_encoder::codec::h264::avcc;
use lite_encoder::demux::{AnnexBDemuxer, Demuxer, MkvDemuxer, Mp4Demuxer, TsDemuxer};
use lite_encoder::media::{Codec, Packet, Track, TrackKind};
use lite_encoder::mux::{MkvMuxer, Mp4Muxer, Muxer, WebmMuxer};
use lite_encoder::probe::{self, Container};
use lite_encoder::{registry, remux, Error};

use crate::args::{Command, CodecSpec, ProbeArgs, TranscodeArgs};

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_UNSUPPORTED: i32 = 3;
pub const EXIT_OUTPUT_EXISTS: i32 = 4;

/// A failed run: the message to print and the status to exit with.
///
/// The exit code is the part scripts read, so it is chosen at the point the
/// failure is detected rather than derived from the message afterwards.
pub struct Failure {
    pub code: i32,
    pub message: String,
}

impl Failure {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Failure {
            code,
            message: message.into(),
        }
    }

    /// A format or codec combination this build does not implement. Distinct
    /// from [`EXIT_ERROR`] on purpose: a script can tell "your file is broken"
    /// apart from "liteenc cannot do this", and only the second is worth
    /// retrying with different flags.
    fn unsupported(message: impl Into<String>) -> Self {
        Failure::new(EXIT_UNSUPPORTED, message)
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure::new(EXIT_ERROR, e.to_string())
    }
}

pub fn run(command: Command) -> Result<(), Failure> {
    match command {
        Command::Help => {
            println!("{}", crate::args::USAGE);
            Ok(())
        }
        Command::Version => {
            println!("liteenc {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Formats => {
            print_formats();
            Ok(())
        }
        Command::Probe(args) => probe_file(args),
        Command::Transcode(args) => transcode(args),
    }
}

// ---------------------------------------------------------------- formats --

/// Print the support matrix, read straight out of [`registry`] so this can
/// never drift from what the code will actually accept.
fn print_formats() {
    println!("container  direction  codecs");
    for container in Container::ALL {
        for (direction, supported) in [
            ("read", registry::can_read as fn(Container, Codec) -> bool),
            ("write", registry::can_write),
        ] {
            let codecs: Vec<&str> = Codec::ALL
                .iter()
                .filter(|c| supported(*container, **c))
                .map(|c| c.name())
                .collect();
            let codecs = if codecs.is_empty() {
                "-".to_string()
            } else {
                codecs.join(" ")
            };
            println!("{:<10} {:<10} {codecs}", container.name(), direction);
        }
    }
    println!();
    println!("decode: h264");
    println!("encode: {}", encoders_in_this_build());
}

fn encoders_in_this_build() -> &'static str {
    if cfg!(feature = "av1") {
        "av1"
    } else {
        "none (rebuild with --features av1 for AV1 encode)"
    }
}

// ------------------------------------------------------------------ probe --

fn probe_file(args: ProbeArgs) -> Result<(), Failure> {
    let data = read_input(&args.input)?;
    let container = input_container(&data, &args.input, args.input_format)?;
    let mut demuxer = open_demuxer(container, &data, crate::args::DEFAULT_FRAME_RATE)?;

    let tracks: Vec<Track> = demuxer.tracks().to_vec();
    let mut stats = vec![StreamStats::default(); tracks.len()];
    while let Some(pkt) = demuxer.read_packet()? {
        let Some(index) = tracks.iter().position(|t| t.id == pkt.track) else {
            // A demuxer that emits packets for a track it never declared is
            // a bug in that demuxer, not a property of the file.
            return Err(Failure::new(
                EXIT_ERROR,
                format!("{container} demuxer emitted a packet for unknown {:?}", pkt.track),
            ));
        };
        stats[index].add(&pkt);
    }

    let duration = stats.iter().map(|s| s.last_pts).max().unwrap_or_default();
    let total: u64 = stats.iter().map(|s| s.packets).sum();

    println!(
        "Input #0, {container}, from '{}':",
        args.input.display()
    );
    println!(
        "  Duration: {}, streams: {}, packets: {total}, size: {}",
        format_timestamp(duration),
        tracks.len(),
        format_size(data.len() as u64),
    );
    for (index, (track, stat)) in tracks.iter().zip(&stats).enumerate() {
        println!("    {}", describe_stream(index, track, stat));
    }
    Ok(())
}

#[derive(Default, Clone)]
struct StreamStats {
    packets: u64,
    bytes: u64,
    last_pts: Duration,
}

impl StreamStats {
    fn add(&mut self, pkt: &Packet) {
        self.packets += 1;
        self.bytes += pkt.data.len() as u64;
        self.last_pts = self.last_pts.max(pkt.pts);
    }

    /// Average frame rate over the packets seen.
    ///
    /// `n` packets span `n - 1` intervals, so dividing by the last PTS (which
    /// is the start of the last packet, not the end of the stream) is exact
    /// rather than off by one frame.
    fn average_fps(&self) -> Option<f64> {
        if self.packets < 2 || self.last_pts.is_zero() {
            return None;
        }
        Some((self.packets - 1) as f64 / self.last_pts.as_secs_f64())
    }
}

fn describe_stream(index: usize, track: &Track, stat: &StreamStats) -> String {
    let mut parts = vec![format!(
        "Stream #0:{index}(track {}): {}: {}",
        track.id.0,
        match track.kind {
            TrackKind::Video { .. } => "Video",
            TrackKind::Audio { .. } => "Audio",
        },
        track.codec.name(),
    )];
    match track.kind {
        TrackKind::Video { width, height } => parts.push(format!("{width}x{height}")),
        TrackKind::Audio {
            sample_rate,
            channels,
        } => parts.push(format!("{sample_rate} Hz, {channels} ch")),
    }
    parts.push(format!("{} packets", stat.packets));
    parts.push(format_size(stat.bytes));
    if let Some(fps) = stat.average_fps() {
        parts.push(format!("avg {fps:.2} fps"));
    }
    parts.join(", ")
}

fn format_timestamp(d: Duration) -> String {
    let ms = d.as_millis();
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1_000) % 60,
        ms % 1_000
    )
}

fn format_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KiB", b as f64 / (1u64 << 10) as f64),
        b => format!("{b} B"),
    }
}

// -------------------------------------------------------------- transcode --

fn transcode(args: TranscodeArgs) -> Result<(), Failure> {
    // Both of these are settled before the input is read, so refusing to
    // clobber a file costs nothing and never happens halfway through a job.
    let out_container = output_container(&args)?;
    check_output_free(&args)?;

    let data = read_input(&args.input)?;
    let in_container = input_container(&data, &args.input, args.input_format)?;

    let mut demuxer = open_demuxer(in_container, &data, args.input_frame_rate)?;
    let tracks: Vec<Track> = demuxer.tracks().to_vec();
    for track in &tracks {
        if !registry::can_read(in_container, track.codec) {
            return Err(Failure::unsupported(format!(
                "this build cannot read {} from {in_container}",
                track.codec.name()
            )));
        }
    }

    match (args.video, args.audio) {
        (CodecSpec::Copy, CodecSpec::Copy) => {
            copy(&mut *demuxer, tracks, out_container, &args)
        }
        (CodecSpec::To(Codec::Av1), CodecSpec::Copy) => {
            transcode_to_av1(&mut *demuxer, tracks, out_container, &args)
        }
        (CodecSpec::To(codec), _) => Err(Failure::unsupported(format!(
            "no {} encoder in this build; encoders: {}",
            codec.name(),
            encoders_in_this_build()
        ))),
        (_, CodecSpec::To(codec)) => Err(Failure::unsupported(format!(
            "no {} encoder in this build; audio streams can only be copied",
            codec.name()
        ))),
    }
}

/// How a track's packets have to be reframed to reach the output container.
#[derive(Clone, Copy, PartialEq)]
enum Reframe {
    /// The bytes already suit the destination.
    None,
    /// H.264 Annex B start codes to AVCC length prefixes, which is what both
    /// MP4 and Matroska's `V_MPEG4/ISO/AVC` require.
    ToAvcc,
}

fn copy(
    demuxer: &mut dyn Demuxer,
    tracks: Vec<Track>,
    out_container: Container,
    args: &TranscodeArgs,
) -> Result<(), Failure> {
    let mut out_tracks = Vec::with_capacity(tracks.len());
    let mut reframes = Vec::with_capacity(tracks.len());
    for track in tracks {
        check_writable(out_container, track.codec)?;
        let (track, reframe) = retarget(track, out_container)?;
        reframes.push((track.id, reframe));
        out_tracks.push(track);
    }

    let mut output = Output::new(out_container, create_output(args)?, out_tracks)?;
    let packets = if reframes.iter().all(|(_, r)| *r == Reframe::None) {
        // Nothing to rewrite, so this is the library's own `-c copy`.
        remux::copy_remux(demuxer, &mut output)?
    } else {
        let mut packets = 0;
        while let Some(pkt) = demuxer.read_packet()? {
            let reframe = reframes
                .iter()
                .find(|(id, _)| *id == pkt.track)
                .map(|(_, r)| *r)
                .unwrap_or(Reframe::None);
            output.write_packet(&apply(reframe, pkt))?;
            packets += 1;
        }
        output.flush()?;
        packets
    };

    report(out_container, args, packets, output.finalize()?);
    Ok(())
}

fn apply(reframe: Reframe, pkt: Packet) -> Packet {
    match reframe {
        Reframe::None => pkt,
        Reframe::ToAvcc => Packet {
            data: avcc::access_unit_to_avcc(&pkt.data).into(),
            ..pkt
        },
    }
}

/// Adapt a source track to what `out_container` needs, reporting the packet
/// reframing that goes with it.
fn retarget(mut track: Track, out_container: Container) -> Result<(Track, Reframe), Failure> {
    if track.codec != Codec::H264 {
        return Ok((track, Reframe::None));
    }
    match out_container {
        // Both want AVCC samples and an `avcC` record. Input that is already
        // AVCC (MP4, or Matroska written by anything conformant) passes
        // through untouched; Annex B input is reframed.
        Container::Mp4 | Container::Matroska | Container::WebM => {
            if avcc::is_avcc_record(&track.extra_data) {
                Ok((track, Reframe::None))
            } else {
                track.extra_data = avcc::parameter_set_record(&track.extra_data)?;
                Ok((track, Reframe::ToAvcc))
            }
        }
        // Neither is a container this build writes; `check_writable` has
        // already rejected them by the time we get here.
        Container::AnnexB | Container::MpegTs => Ok((track, Reframe::None)),
    }
}

#[cfg(not(feature = "av1"))]
fn transcode_to_av1(
    _demuxer: &mut dyn Demuxer,
    _tracks: Vec<Track>,
    _out_container: Container,
    _args: &TranscodeArgs,
) -> Result<(), Failure> {
    Err(Failure::unsupported(
        "this build has no AV1 encoder; rebuild with `cargo build --release --features av1`",
    ))
}

#[cfg(feature = "av1")]
fn transcode_to_av1(
    demuxer: &mut dyn Demuxer,
    tracks: Vec<Track>,
    out_container: Container,
    args: &TranscodeArgs,
) -> Result<(), Failure> {
    use lite_encoder::codec::av1::Av1Encoder;
    use lite_encoder::codec::h264::decoder::Frontend;
    use lite_encoder::media::Encoder;

    let video = tracks
        .iter()
        .find(|t| matches!(t.kind, TrackKind::Video { .. }))
        .ok_or_else(|| Failure::unsupported("no video stream to encode"))?;
    if video.codec != Codec::H264 {
        return Err(Failure::unsupported(format!(
            "cannot decode {}; this build decodes h264 only",
            video.codec.name()
        )));
    }
    let TrackKind::Video { width, height } = video.kind else {
        unreachable!("selected by the same pattern")
    };
    check_writable(out_container, Codec::Av1)?;

    // An AVCC source keeps its parameter sets out of band, so they have to go
    // back in front of every keyframe before the decoder sees them.
    let source = SourceFraming::of(video)?;

    let mut decoder = Frontend::new();
    let mut encoder = Av1Encoder::new(
        video.id,
        width,
        height,
        args.encoder_frame_rate(),
        args.video_bitrate,
    )?;

    let mut out_tracks = vec![Track {
        id: video.id,
        codec: Codec::Av1,
        kind: video.kind.clone(),
        extra_data: encoder.extra_data(),
    }];
    // Audio rides along untouched; there is no audio encoder to route it to.
    for track in tracks.iter().filter(|t| t.id != video.id) {
        check_writable(out_container, track.codec)?;
        out_tracks.push(track.clone());
    }

    let video_id = video.id;
    let mut output = Output::new(out_container, create_output(args)?, out_tracks)?;
    let mut packets = 0u64;
    while let Some(pkt) = demuxer.read_packet()? {
        if pkt.track != video_id {
            output.write_packet(&pkt)?;
            packets += 1;
            continue;
        }
        let unit = source.to_annexb(&pkt)?;
        for frame in decoder.decode_access_unit(&unit, pkt.pts)? {
            for encoded in encoder.encode(&frame)? {
                output.write_packet(&encoded)?;
                packets += 1;
            }
        }
    }
    for encoded in encoder.flush()? {
        output.write_packet(&encoded)?;
        packets += 1;
    }
    output.flush()?;

    report(out_container, args, packets, output.finalize()?);
    Ok(())
}

/// How a source track's H.264 packets are framed, and how to get Annex B —
/// the only framing the decoder accepts — back out of them.
#[cfg(feature = "av1")]
struct SourceFraming {
    /// `None` when the packets are already Annex B.
    avcc: Option<AvccSource>,
}

#[cfg(feature = "av1")]
struct AvccSource {
    length_size: usize,
    parameter_sets: Vec<u8>,
}

#[cfg(feature = "av1")]
impl SourceFraming {
    fn of(track: &Track) -> Result<Self, Failure> {
        if !avcc::is_avcc_record(&track.extra_data) {
            return Ok(SourceFraming { avcc: None });
        }
        Ok(SourceFraming {
            avcc: Some(AvccSource {
                length_size: avcc::nal_length_size(&track.extra_data)?,
                parameter_sets: avcc::annexb_parameter_sets(&track.extra_data)?,
            }),
        })
    }

    fn to_annexb(&self, pkt: &Packet) -> Result<Vec<u8>, Failure> {
        let Some(source) = &self.avcc else {
            return Ok(pkt.data.to_vec());
        };
        let mut unit = Vec::with_capacity(pkt.data.len() + source.parameter_sets.len() + 16);
        if pkt.keyframe {
            unit.extend_from_slice(&source.parameter_sets);
        }
        unit.extend_from_slice(&avcc::access_unit_to_annexb(&pkt.data, source.length_size)?);
        Ok(unit)
    }
}

// ------------------------------------------------------------------- plumbing

/// The muxers, behind one type so the packet loop and `finalize` do not care
/// which container they are writing.
///
/// `finalize` consumes the muxer to report a byte count, which is why it is
/// not on the [`Muxer`] trait and why this enum exists at all.
enum Output {
    Mkv(MkvMuxer<BufWriter<File>>),
    Webm(WebmMuxer<BufWriter<File>>),
    Mp4(Mp4Muxer<BufWriter<File>>),
}

impl Output {
    fn new(container: Container, file: File, tracks: Vec<Track>) -> Result<Self, Failure> {
        let out = BufWriter::new(file);
        Ok(match container {
            Container::Matroska => Output::Mkv(MkvMuxer::new(out, tracks)?),
            Container::WebM => Output::Webm(WebmMuxer::new(out, tracks)?),
            Container::Mp4 => Output::Mp4(Mp4Muxer::new(out, tracks)?),
            Container::AnnexB | Container::MpegTs => {
                return Err(Failure::unsupported(format!(
                    "this build cannot write {container}; run `liteenc formats`"
                )))
            }
        })
    }

    fn finalize(self) -> Result<u64, Error> {
        match self {
            Output::Mkv(m) => m.finalize(),
            Output::Webm(m) => m.finalize(),
            Output::Mp4(m) => m.finalize(),
        }
    }
}

impl Muxer for Output {
    fn write_packet(&mut self, pkt: &Packet) -> Result<(), Error> {
        match self {
            Output::Mkv(m) => m.write_packet(pkt),
            Output::Webm(m) => m.write_packet(pkt),
            Output::Mp4(m) => m.write_packet(pkt),
        }
    }

    fn flush(&mut self) -> Result<(), Error> {
        match self {
            Output::Mkv(m) => Muxer::flush(m),
            Output::Webm(m) => Muxer::flush(m),
            Output::Mp4(m) => Muxer::flush(m),
        }
    }
}

fn open_demuxer(
    container: Container,
    data: &[u8],
    frame_rate: u32,
) -> Result<Box<dyn Demuxer>, Failure> {
    Ok(match container {
        Container::AnnexB => Box::new(AnnexBDemuxer::new(data, frame_rate)?),
        // WebM is a Matroska profile; the same element walk reads both.
        Container::Matroska | Container::WebM => Box::new(MkvDemuxer::new(data)?),
        Container::Mp4 => Box::new(Mp4Demuxer::new(data)?),
        Container::MpegTs => Box::new(TsDemuxer::new(data)?),
    })
}

fn input_container(
    data: &[u8],
    path: &Path,
    forced: Option<Container>,
) -> Result<Container, Failure> {
    if let Some(c) = forced {
        return Ok(c);
    }
    probe::probe(data, Some(path)).map_err(|_| {
        Failure::new(
            EXIT_ERROR,
            format!(
                "could not identify the container in '{}'; force it with -f FORMAT",
                path.display()
            ),
        )
    })
}

fn output_container(args: &TranscodeArgs) -> Result<Container, Failure> {
    if let Some(c) = args.output_format {
        return Ok(c);
    }
    probe::container_from_path(&args.output).ok_or_else(|| {
        Failure::new(
            EXIT_USAGE,
            format!(
                "'{}' has no recognised extension; name the format with -f",
                args.output.display()
            ),
        )
    })
}

fn check_writable(container: Container, codec: Codec) -> Result<(), Failure> {
    if registry::can_write(container, codec) {
        return Ok(());
    }
    Err(Failure::unsupported(format!(
        "this build cannot write {} into {container}; run `liteenc formats`",
        codec.name()
    )))
}

fn read_input(path: &Path) -> Result<Vec<u8>, Failure> {
    // Every demuxer here parses a whole buffer rather than streaming, so the
    // file is read in one go. Fine for the file-sized inputs this tool takes.
    std::fs::read(path).map_err(|e| {
        Failure::new(
            EXIT_ERROR,
            format!("cannot read '{}': {e}", path.display()),
        )
    })
}

/// Refuse to clobber an existing output unless `-y` said to.
///
/// Racy against another process by nature, so it is a courtesy check, not a
/// lock — the same guarantee ffmpeg's prompt gives.
fn check_output_free(args: &TranscodeArgs) -> Result<(), Failure> {
    if !args.overwrite && args.output.exists() {
        return Err(Failure::new(
            EXIT_OUTPUT_EXISTS,
            format!(
                "'{}' already exists; pass -y to overwrite it",
                args.output.display()
            ),
        ));
    }
    Ok(())
}

fn create_output(args: &TranscodeArgs) -> Result<File, Failure> {
    File::create(&args.output).map_err(|e| {
        Failure::new(
            EXIT_ERROR,
            format!("cannot write '{}': {e}", args.output.display()),
        )
    })
}

/// ffmpeg-style one-line summary, on stderr so it never pollutes a pipeline
/// that is reading `probe` output.
fn report(container: Container, args: &TranscodeArgs, packets: u64, bytes: u64) {
    let _ = writeln!(
        std::io::stderr(),
        "Output #0, {container}, to '{}': {packets} packets, {}",
        args.output.display(),
        format_size(bytes)
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use lite_encoder::media::TrackId;

    #[test]
    fn timestamps_are_formatted_like_ffprobe() {
        assert_eq!(format_timestamp(Duration::ZERO), "00:00:00.000");
        assert_eq!(format_timestamp(Duration::from_millis(136)), "00:00:00.136");
        assert_eq!(format_timestamp(Duration::from_secs(3_723)), "01:02:03.000");
    }

    #[test]
    fn sizes_step_through_binary_units() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KiB");
        assert_eq!(format_size(3 << 20), "3.0 MiB");
    }

    #[test]
    fn average_fps_needs_at_least_two_packets_and_a_span() {
        let mut stats = StreamStats::default();
        assert_eq!(stats.average_fps(), None);

        for i in 0..4u64 {
            stats.add(&Packet {
                track: TrackId(1),
                pts: Duration::from_millis(i * 1000 / 22),
                keyframe: i == 0,
                data: bytes::Bytes::from_static(&[0; 10]),
            });
        }
        assert_eq!(stats.packets, 4);
        assert_eq!(stats.bytes, 40);
        // Three intervals across the 136 ms to the last packet's PTS.
        let fps = stats.average_fps().unwrap();
        assert!((fps - 22.0).abs() < 0.5, "expected ~22 fps, got {fps}");
    }

    #[test]
    fn annexb_h264_is_reframed_for_avcc_containers_only() {
        let track = Track {
            id: TrackId(1),
            codec: Codec::H264,
            kind: TrackKind::Video {
                width: 16,
                height: 16,
            },
            // Minimal SPS (profile/level bytes) and PPS, Annex B framed.
            extra_data: vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x0a, 0, 0, 0, 1, 0x68, 0xce],
        };

        for container in [Container::Mp4, Container::Matroska, Container::WebM] {
            let (out, reframe) = retarget(track.clone(), container).unwrap();
            assert!(reframe == Reframe::ToAvcc, "{container} wants AVCC");
            assert!(avcc::is_avcc_record(&out.extra_data));

            // Already-AVCC input must not be reframed a second time.
            let (again, reframe) = retarget(out, container).unwrap();
            assert!(reframe == Reframe::None, "{container} double-reframed");
            assert!(avcc::is_avcc_record(&again.extra_data));
        }
    }

    #[test]
    fn non_h264_tracks_are_left_alone() {
        let track = Track {
            id: TrackId(2),
            codec: Codec::Opus,
            kind: TrackKind::Audio {
                sample_rate: 48_000,
                channels: 2,
            },
            extra_data: b"OpusHead".to_vec(),
        };
        let (out, reframe) = retarget(track, Container::WebM).unwrap();
        assert!(reframe == Reframe::None);
        assert_eq!(out.extra_data, b"OpusHead");
    }

    #[test]
    fn unsupported_write_targets_get_the_unsupported_exit_code() {
        let failure = check_writable(Container::MpegTs, Codec::H264).unwrap_err();
        assert_eq!(failure.code, EXIT_UNSUPPORTED);
        assert!(failure.message.contains("mpegts"), "{}", failure.message);

        assert!(check_writable(Container::Matroska, Codec::H264).is_ok());
    }
}
