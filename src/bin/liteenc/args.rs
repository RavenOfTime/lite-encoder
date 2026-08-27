//! `liteenc` argument parsing.
//!
//! Hand-rolled rather than derived from a parser crate. ffmpeg's surface is
//! not the shape argument crates model: `-c:v` is not a valid long or short
//! option name anywhere, and the meaning of `-f` and `-r` depends on whether
//! they appear before or after `-i`. Encoding those rules directly is smaller
//! than fighting a crate into them, and costs no dependency.

use std::path::PathBuf;

use lite_encoder::media::Codec;
use lite_encoder::probe::Container;

/// Frame rate assumed for an elementary Annex B input, which carries no
/// timestamps of its own. ffmpeg's default for the same case.
pub const DEFAULT_FRAME_RATE: u32 = 25;

/// Default target video bitrate in bits per second. Matches the camera-scale
/// 1080p bitrate the AV1 encoder settings were tuned against.
pub const DEFAULT_VIDEO_BITRATE: i32 = 1_000_000;

pub const USAGE: &str = "\
usage:
  liteenc [INPUT OPTIONS] -i INPUT [OUTPUT OPTIONS] OUTPUT
  liteenc probe [-f FORMAT] INPUT
  liteenc formats
  liteenc --help | --version

A Rust-native ffmpeg alternative: demux, decode, encode, mux.

transcode options:
  -i PATH      input file (required)
  -o PATH      output file; a trailing positional argument means the same
  -f FORMAT    container format. Before -i it forces the input's, after -i the
               output's. By default the input is sniffed from its bytes and
               the output taken from its file extension.
  -r FPS       frame rate. Before -i, the rate used to synthesize timestamps
               for an elementary Annex B input (default 25). After -i, the
               rate the encoder is configured with (default: the input's).
  -c SPEC      codec for every stream; `-c copy` remuxes without decoding
  -c:v SPEC    video codec: `copy`, or `av1` on a build with --features av1
  -c:a SPEC    audio codec: `copy`
  -b:v RATE    target video bitrate, e.g. `1M`, `800k` (default 1M)
  -y           overwrite OUTPUT if it already exists
  -n           never overwrite OUTPUT (the default)
  -v           log to stderr

Streams are copied unless a codec is named, so `-i in.mp4 out.mkv` and
`-i in.mp4 -c copy out.mkv` do the same thing. `liteenc formats` lists what
this build can read and write.

exit codes:
  0  success
  1  the input could not be read, or processing failed
  2  bad usage
  3  this build cannot do what was asked (unsupported container or codec)
  4  OUTPUT exists and -y was not given

examples:
  liteenc -i clip.h264 -c copy clip.mkv
  liteenc -i clip.mp4 -c:v av1 -b:v 800k clip.webm
  liteenc probe clip.mkv";

/// What the user asked for.
#[derive(Debug, PartialEq)]
pub enum Command {
    Help,
    Version,
    Formats,
    Probe(ProbeArgs),
    Transcode(TranscodeArgs),
}

#[derive(Debug, PartialEq)]
pub struct ProbeArgs {
    pub input: PathBuf,
    pub input_format: Option<Container>,
    pub verbose: bool,
}

/// What to do with one stream: pass its packets through untouched, or decode
/// and re-encode them to `To`'s codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecSpec {
    Copy,
    To(Codec),
}

#[derive(Debug, PartialEq)]
pub struct TranscodeArgs {
    pub input: PathBuf,
    pub output: PathBuf,
    pub input_format: Option<Container>,
    pub output_format: Option<Container>,
    pub video: CodecSpec,
    pub audio: CodecSpec,
    pub input_frame_rate: u32,
    /// `None` means "reuse [`TranscodeArgs::input_frame_rate`]".
    pub output_frame_rate: Option<u32>,
    pub video_bitrate: i32,
    pub overwrite: bool,
    pub verbose: bool,
}

impl TranscodeArgs {
    /// Frame rate the encoder is configured with.
    ///
    /// Only an encode reads this, so a build without an encoder feature has
    /// no caller for it outside the tests below.
    #[cfg_attr(not(feature = "av1"), allow(dead_code))]
    pub fn encoder_frame_rate(&self) -> u32 {
        self.output_frame_rate.unwrap_or(self.input_frame_rate)
    }
}

/// A usage error. Always exit code 2; the caller prints [`USAGE`] alongside.
#[derive(Debug, PartialEq)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, UsageError> {
    Err(UsageError(msg.into()))
}

/// Parse `argv` *without* the program name.
pub fn parse(argv: &[String]) -> Result<Command, UsageError> {
    let Some(first) = argv.first() else {
        return err("no arguments given");
    };
    match first.as_str() {
        "-h" | "--help" | "help" => Ok(Command::Help),
        "-V" | "--version" | "version" => Ok(Command::Version),
        "formats" => {
            if argv.len() > 1 {
                return err("`formats` takes no arguments");
            }
            Ok(Command::Formats)
        }
        "probe" => parse_probe(&argv[1..]).map(Command::Probe),
        _ => parse_transcode(argv).map(Command::Transcode),
    }
}

fn parse_probe(argv: &[String]) -> Result<ProbeArgs, UsageError> {
    let mut input: Option<PathBuf> = None;
    let mut input_format = None;
    let mut verbose = false;

    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            // `-i` is accepted so `probe -i clip.mkv` works for anyone who
            // types it out of transcode habit; ffprobe takes a bare path.
            "-i" => input = Some(PathBuf::from(value(&mut it, "-i")?)),
            "-f" => input_format = Some(container(value(&mut it, "-f")?)?),
            "-v" | "--verbose" => verbose = true,
            other if other.starts_with('-') => return err(format!("unknown option `{other}`")),
            other if input.is_none() => input = Some(PathBuf::from(other)),
            other => return err(format!("unexpected argument `{other}`; probe takes one file")),
        }
    }

    match input {
        Some(input) => Ok(ProbeArgs {
            input,
            input_format,
            verbose,
        }),
        None => err("probe needs an input file"),
    }
}

fn parse_transcode(argv: &[String]) -> Result<TranscodeArgs, UsageError> {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut input_format = None;
    let mut output_format = None;
    let mut video = None;
    let mut audio = None;
    let mut input_frame_rate = None;
    let mut output_frame_rate = None;
    let mut video_bitrate = DEFAULT_VIDEO_BITRATE;
    let mut overwrite = false;
    let mut verbose = false;

    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        // ffmpeg scopes an option to the next file on the command line. We
        // have exactly one input and one output, so "have we passed -i yet"
        // is the whole rule.
        let for_output = input.is_some();
        match arg.as_str() {
            "-i" => {
                if input.is_some() {
                    return err("only one input (-i) is supported");
                }
                input = Some(PathBuf::from(value(&mut it, "-i")?));
            }
            "-o" => set_output(&mut output, value(&mut it, "-o")?)?,
            "-f" => {
                let c = container(value(&mut it, "-f")?)?;
                *if for_output {
                    &mut output_format
                } else {
                    &mut input_format
                } = Some(c);
            }
            "-r" => {
                let fps = frame_rate(value(&mut it, "-r")?)?;
                *if for_output {
                    &mut output_frame_rate
                } else {
                    &mut input_frame_rate
                } = Some(fps);
            }
            "-c" | "-codec" => {
                let spec = codec_spec(value(&mut it, arg)?)?;
                video = Some(spec);
                audio = Some(spec);
            }
            "-c:v" | "-vcodec" => video = Some(codec_spec(value(&mut it, arg)?)?),
            "-c:a" | "-acodec" => audio = Some(codec_spec(value(&mut it, arg)?)?),
            "-b:v" => video_bitrate = bitrate(value(&mut it, "-b:v")?)?,
            "-y" => overwrite = true,
            "-n" => overwrite = false,
            "-v" | "--verbose" => verbose = true,
            other if other.starts_with('-') => return err(format!("unknown option `{other}`")),
            other => set_output(&mut output, other)?,
        }
    }

    let Some(input) = input else {
        return err("no input given; use -i PATH");
    };
    let Some(output) = output else {
        return err("no output given; pass a path, or -o PATH");
    };
    if input == output {
        return err("input and output are the same file");
    }

    Ok(TranscodeArgs {
        input,
        output,
        input_format,
        output_format,
        // Copying is the default: it is the one thing every supported
        // container pair can do, and it never silently spends CPU.
        video: video.unwrap_or(CodecSpec::Copy),
        audio: audio.unwrap_or(CodecSpec::Copy),
        input_frame_rate: input_frame_rate.unwrap_or(DEFAULT_FRAME_RATE),
        output_frame_rate,
        video_bitrate,
        overwrite,
        verbose,
    })
}

fn set_output(slot: &mut Option<PathBuf>, path: &str) -> Result<(), UsageError> {
    if let Some(existing) = slot {
        return err(format!(
            "two outputs given (`{}` and `{path}`); liteenc writes one file",
            existing.display()
        ));
    }
    *slot = Some(PathBuf::from(path));
    Ok(())
}

fn value<'a>(
    it: &mut impl Iterator<Item = &'a String>,
    flag: &str,
) -> Result<&'a str, UsageError> {
    match it.next() {
        Some(v) => Ok(v.as_str()),
        None => err(format!("`{flag}` needs a value")),
    }
}

fn container(name: &str) -> Result<Container, UsageError> {
    Container::from_name(name).ok_or_else(|| {
        UsageError(format!(
            "unknown format `{name}`; run `liteenc formats` for the list"
        ))
    })
}

fn codec_spec(name: &str) -> Result<CodecSpec, UsageError> {
    if name.eq_ignore_ascii_case("copy") {
        return Ok(CodecSpec::Copy);
    }
    Codec::from_name(name).map(CodecSpec::To).ok_or_else(|| {
        UsageError(format!(
            "unknown codec `{name}`; run `liteenc formats` for the list"
        ))
    })
}

fn frame_rate(value: &str) -> Result<u32, UsageError> {
    match value.parse::<u32>() {
        Ok(fps) if fps > 0 => Ok(fps),
        _ => err(format!("`-r` needs a positive whole frame rate, got `{value}`")),
    }
}

/// Parse a bitrate with ffmpeg's `k`/`M` suffixes, which are powers of ten,
/// not of two.
fn bitrate(value: &str) -> Result<i32, UsageError> {
    let (digits, scale) = match value.as_bytes().last() {
        Some(b'k') | Some(b'K') => (&value[..value.len() - 1], 1_000),
        Some(b'm') | Some(b'M') => (&value[..value.len() - 1], 1_000_000),
        _ => (value, 1),
    };
    match digits.parse::<i32>() {
        Ok(n) if n > 0 => n
            .checked_mul(scale)
            .ok_or_else(|| UsageError(format!("bitrate `{value}` is too large"))),
        _ => err(format!(
            "`-b:v` needs a positive bitrate like `800k` or `1M`, got `{value}`"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Command, UsageError> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse(&owned)
    }

    fn transcode(args: &[&str]) -> TranscodeArgs {
        match parse_args(args).unwrap() {
            Command::Transcode(t) => t,
            other => panic!("expected a transcode, got {other:?}"),
        }
    }

    #[test]
    fn output_is_positional_or_dash_o() {
        let positional = transcode(&["-i", "in.h264", "out.mkv"]);
        let flagged = transcode(&["-i", "in.h264", "-o", "out.mkv"]);
        assert_eq!(positional.output, PathBuf::from("out.mkv"));
        assert_eq!(flagged.output, positional.output);
    }

    #[test]
    fn streams_are_copied_when_no_codec_is_named() {
        let t = transcode(&["-i", "in.h264", "out.mkv"]);
        assert_eq!(t.video, CodecSpec::Copy);
        assert_eq!(t.audio, CodecSpec::Copy);
    }

    #[test]
    fn dash_c_sets_both_streams_and_per_stream_flags_override_it() {
        let t = transcode(&["-i", "in.mp4", "-c", "copy", "-c:v", "av1", "out.webm"]);
        assert_eq!(t.video, CodecSpec::To(Codec::Av1));
        assert_eq!(t.audio, CodecSpec::Copy);
    }

    #[test]
    fn format_and_rate_are_scoped_by_where_they_sit_relative_to_input() {
        let t = transcode(&[
            "-f", "annexb", "-r", "30", "-i", "in.bin", "-f", "mkv", "-r", "15", "out.bin",
        ]);
        assert_eq!(t.input_format, Some(Container::AnnexB));
        assert_eq!(t.output_format, Some(Container::Matroska));
        assert_eq!(t.input_frame_rate, 30);
        assert_eq!(t.output_frame_rate, Some(15));
        assert_eq!(t.encoder_frame_rate(), 15);
    }

    #[test]
    fn encoder_frame_rate_falls_back_to_the_input_rate() {
        let t = transcode(&["-r", "30", "-i", "in.h264", "-c:v", "av1", "out.webm"]);
        assert_eq!(t.output_frame_rate, None);
        assert_eq!(t.encoder_frame_rate(), 30);
    }

    #[test]
    fn defaults_match_the_documented_ones() {
        let t = transcode(&["-i", "in.h264", "out.mkv"]);
        assert_eq!(t.input_frame_rate, DEFAULT_FRAME_RATE);
        assert_eq!(t.video_bitrate, DEFAULT_VIDEO_BITRATE);
        assert!(!t.overwrite);
        assert!(!t.verbose);
    }

    #[test]
    fn bitrate_suffixes_are_powers_of_ten() {
        assert_eq!(bitrate("1000").unwrap(), 1_000);
        assert_eq!(bitrate("800k").unwrap(), 800_000);
        assert_eq!(bitrate("2M").unwrap(), 2_000_000);
        assert!(bitrate("0").is_err());
        assert!(bitrate("-5").is_err());
        assert!(bitrate("fast").is_err());
        assert!(bitrate("9999M").is_err(), "must not wrap around i32");
    }

    #[test]
    fn overwrite_flags_take_the_last_one_given() {
        assert!(transcode(&["-i", "a.h264", "-n", "-y", "b.mkv"]).overwrite);
        assert!(!transcode(&["-i", "a.h264", "-y", "-n", "b.mkv"]).overwrite);
    }

    #[test]
    fn probe_takes_one_bare_path() {
        let Command::Probe(p) = parse_args(&["probe", "clip.mkv"]).unwrap() else {
            panic!("expected a probe");
        };
        assert_eq!(p.input, PathBuf::from("clip.mkv"));
        assert_eq!(p.input_format, None);

        let Command::Probe(p) = parse_args(&["probe", "-f", "ts", "-i", "clip.bin"]).unwrap()
        else {
            panic!("expected a probe");
        };
        assert_eq!(p.input_format, Some(Container::MpegTs));
    }

    #[test]
    fn help_version_and_formats_are_recognised() {
        for flag in ["-h", "--help", "help"] {
            assert_eq!(parse_args(&[flag]).unwrap(), Command::Help);
        }
        for flag in ["-V", "--version", "version"] {
            assert_eq!(parse_args(&[flag]).unwrap(), Command::Version);
        }
        assert_eq!(parse_args(&["formats"]).unwrap(), Command::Formats);
        assert!(parse_args(&["formats", "mp4"]).is_err());
    }

    #[test]
    fn bad_usage_is_rejected_rather_than_guessed_at() {
        // Every one of these has a plausible "did you mean" reading that
        // would silently do the wrong thing to someone's file.
        assert!(parse_args(&[]).is_err(), "no arguments");
        assert!(parse_args(&["-i", "in.h264"]).is_err(), "no output");
        assert!(parse_args(&["out.mkv"]).is_err(), "no input");
        assert!(parse_args(&["-i"]).is_err(), "-i with no value");
        assert!(parse_args(&["-i", "a.h264", "-b:v"]).is_err(), "-b:v alone");
        assert!(parse_args(&["-i", "a.h264", "--turbo", "b.mkv"]).is_err());
        assert!(parse_args(&["-i", "a.h264", "b.mkv", "c.mkv"]).is_err());
        assert!(parse_args(&["-i", "a.h264", "-i", "b.h264", "c.mkv"]).is_err());
        assert!(parse_args(&["-i", "a.mkv", "a.mkv"]).is_err(), "in == out");
        assert!(parse_args(&["-i", "a.h264", "-c", "mp3", "b.mkv"]).is_err());
        assert!(parse_args(&["-i", "a.h264", "-f", "ogg", "b.mkv"]).is_err());
        assert!(parse_args(&["-i", "a.h264", "-r", "0", "b.mkv"]).is_err());
    }

    #[test]
    fn codec_and_format_names_accept_the_usual_aliases() {
        assert_eq!(codec_spec("COPY").unwrap(), CodecSpec::Copy);
        assert_eq!(codec_spec("avc1").unwrap(), CodecSpec::To(Codec::H264));
        assert_eq!(container("mkv").unwrap(), Container::Matroska);
    }
}
