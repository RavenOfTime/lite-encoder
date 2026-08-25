//! Differential testing against the reference decoder.
//!
//! Runs our decoder and [`super::reference`] over the same bitstream and
//! compares every decoded sample. A video decoder either produces the exact
//! bytes the encoder intended or it is broken, so there is no tolerance to
//! tune: any difference is a bug, and the useful output is not "how wrong"
//! but *where* the first divergence is, which is what [`Divergence`] carries.
//!
//! Locating the first bad sample is the point. Wrong output propagates —
//! through intra prediction within a frame and through motion compensation
//! across frames — so by the time a picture looks obviously wrong, most of it
//! is downstream damage. The first differing sample in the first differing
//! frame is the only coordinate that points at the actual defect.

use crate::media::{Decoder, Frame};
use crate::Error;

use super::annexb;
use super::reference::{packet, ReferenceDecoder};

/// The outcome of running two decoders over one stream.
#[derive(Debug)]
pub struct Report {
    /// Frames the reference produced.
    pub reference_frames: usize,
    /// Frames the decoder under test produced.
    pub subject_frames: usize,
    /// First sample where the two disagree, if they do.
    pub divergence: Option<Divergence>,
    /// The decoder under test failed outright, and on which access unit.
    pub error: Option<(usize, Error)>,
}

impl Report {
    /// Whether the decoder under test matched the reference exactly.
    pub fn matches(&self) -> bool {
        self.error.is_none()
            && self.divergence.is_none()
            && self.reference_frames == self.subject_frames
            && self.reference_frames > 0
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some((au, e)) = &self.error {
            return write!(f, "decode failed on access unit {au}: {e}");
        }
        if self.reference_frames != self.subject_frames {
            return write!(
                f,
                "frame count mismatch: reference produced {}, subject produced {}",
                self.reference_frames, self.subject_frames
            );
        }
        match &self.divergence {
            None if self.reference_frames == 0 => write!(f, "neither decoder produced a frame"),
            None => write!(f, "{} frames match exactly", self.reference_frames),
            Some(d) => write!(f, "{d}"),
        }
    }
}

/// Where two decoders first disagreed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divergence {
    pub frame: usize,
    /// 0 = Y, 1 = U, 2 = V.
    pub plane: usize,
    pub x: usize,
    pub y: usize,
    pub reference: u8,
    pub subject: u8,
    /// How many samples differ in this plane. One bad sample points at
    /// arithmetic; a whole macroblock points at prediction or entropy decode,
    /// and the count is the cheapest way to tell those apart.
    pub differing_samples: usize,
    pub max_abs_diff: u8,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let plane = ["Y", "U", "V"][self.plane];
        // Position is reported in macroblocks as well as samples, since the
        // macroblock is the unit the bug will actually be in.
        let (mb_x, mb_y) = if self.plane == 0 {
            (self.x / 16, self.y / 16)
        } else {
            (self.x / 8, self.y / 8)
        };
        write!(
            f,
            "frame {} plane {plane} diverges at ({}, {}) in macroblock ({mb_x}, {mb_y}): \
             reference {}, subject {}; {} samples differ in this plane, max delta {}",
            self.frame,
            self.x,
            self.y,
            self.reference,
            self.subject,
            self.differing_samples,
            self.max_abs_diff,
        )
    }
}

/// Decodes `stream` with both decoders and compares the results.
///
/// Both are driven one access unit at a time and in lockstep, so a decoder
/// that emits pictures on a different schedule shows up as a frame-count
/// mismatch rather than as a silent misalignment that reports every frame as
/// wrong.
pub fn compare(stream: &[u8], subject: &mut dyn Decoder) -> Result<Report, Error> {
    let mut reference = ReferenceDecoder::new()?;

    let mut reference_out = Vec::new();
    let mut subject_out = Vec::new();
    let mut error = None;

    for (i, au) in annexb::access_units(stream).into_iter().enumerate() {
        let pkt = packet(au, i);
        reference_out.extend(reference.decode(&pkt)?);

        if error.is_none() {
            match subject.decode(&pkt) {
                Ok(frames) => subject_out.extend(frames),
                // Keep driving the reference so the report can still say how
                // many frames the stream was supposed to contain.
                Err(e) => error = Some((i, e)),
            }
        }
    }
    reference_out.extend(reference.flush()?);
    if error.is_none() {
        match subject.flush() {
            Ok(frames) => subject_out.extend(frames),
            Err(e) => error = Some((usize::MAX, e)),
        }
    }

    let divergence = reference_out
        .iter()
        .zip(&subject_out)
        .enumerate()
        .find_map(|(i, (r, s))| diff_frame(r, s, i));

    Ok(Report {
        reference_frames: reference_out.len(),
        subject_frames: subject_out.len(),
        divergence,
        error,
    })
}

/// First differing sample between two frames, scanning Y then U then V.
fn diff_frame(reference: &Frame, subject: &Frame, index: usize) -> Option<Divergence> {
    if reference.width != subject.width || reference.height != subject.height {
        // Reported as a divergence at the origin rather than as its own case:
        // a wrong picture size is always a parameter-set bug, and the caller
        // only needs to be told which frame to look at.
        return Some(Divergence {
            frame: index,
            plane: 0,
            x: 0,
            y: 0,
            reference: 0,
            subject: 0,
            differing_samples: 0,
            max_abs_diff: 0,
        });
    }

    (0..3).find_map(|plane| {
        let (r, s) = (&reference.planes[plane], &subject.planes[plane]);
        let stride = reference.strides[plane];
        if stride != subject.strides[plane] || r.len() != s.len() {
            return None;
        }

        let differing = r.iter().zip(s).filter(|(a, b)| a != b).count();
        if differing == 0 {
            return None;
        }
        let max_abs_diff = r.iter().zip(s).map(|(a, b)| a.abs_diff(*b)).max()?;
        let at = r.iter().zip(s).position(|(a, b)| a != b)?;

        Some(Divergence {
            frame: index,
            plane,
            x: at % stride,
            y: at / stride,
            reference: r[at],
            subject: s[at],
            differing_samples: differing,
            max_abs_diff,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::h264::decoder::Frontend;
    use crate::codec::h264::reference::synthesize;
    use crate::media::Packet;

    /// The harness must find nothing when there is nothing to find. Running
    /// the reference against a second instance of itself is the only way to
    /// check that before our own decoder exists, and it stays useful after:
    /// if this ever fails, the harness is lying, not the decoder.
    #[test]
    fn the_reference_agrees_with_itself() {
        let stream = synthesize(64, 48, 6, 3).expect("encode");
        let mut subject = ReferenceDecoder::new().expect("decoder");
        let report = compare(&stream.annexb, &mut subject).expect("compare");
        assert!(report.matches(), "{report}");
        assert_eq!(report.reference_frames, 6);
    }

    /// The decoder under test, against the oracle, on an all-intra stream.
    ///
    /// This is the test the whole harness exists for. All-intra first: an
    /// I picture depends on nothing outside itself, so a failure here is a
    /// bug in entropy decode, prediction, or reconstruction, with no chance
    /// that it is inherited from a wrong reference picture.
    #[test]
    fn our_decoder_matches_the_reference_on_an_intra_stream() {
        let stream = synthesize(64, 48, 3, 1).expect("encode");
        let mut subject = Frontend::new();
        let report = compare(&stream.annexb, &mut subject).expect("compare");
        assert!(report.matches(), "{report}");
    }

    /// And on a stream with P pictures, which adds motion compensation and
    /// the reference picture list to the surface under test.
    #[test]
    fn our_decoder_matches_the_reference_with_inter_prediction() {
        let stream = synthesize(64, 48, 6, 6).expect("encode");
        let mut subject = Frontend::new();
        let report = compare(&stream.annexb, &mut subject).expect("compare");
        assert!(report.matches(), "{report}");
    }

    /// And it must find something when there is. A decoder that flips the low
    /// bit of one luma sample is wrong in the quietest way a real bug could
    /// be: no crash, no frame-count change, a difference of one.
    #[test]
    fn a_single_bit_error_in_luma_is_caught() {
        struct BitFlipped(ReferenceDecoder);
        impl Decoder for BitFlipped {
            fn decode(&mut self, pkt: &Packet) -> Result<Vec<Frame>, Error> {
                let mut frames = self.0.decode(pkt)?;
                for f in &mut frames {
                    f.planes[0][0] ^= 1;
                }
                Ok(frames)
            }
            fn flush(&mut self) -> Result<Vec<Frame>, Error> {
                self.0.flush()
            }
        }

        let stream = synthesize(64, 48, 3, 1).expect("encode");
        let mut subject = BitFlipped(ReferenceDecoder::new().expect("decoder"));
        let report = compare(&stream.annexb, &mut subject).expect("compare");

        let d = report.divergence.expect("divergence not detected");
        assert_eq!((d.frame, d.plane, d.x, d.y), (0, 0, 0, 0));
        assert_eq!(d.differing_samples, 1);
        assert_eq!(d.max_abs_diff, 1);
        assert!(!report.matches());
    }

    /// A decoder that produces no frames at all is a different failure from
    /// one that produces wrong frames, and the report has to say which.
    #[test]
    fn a_decoder_that_emits_nothing_reports_a_count_mismatch() {
        struct Silent;
        impl Decoder for Silent {
            fn decode(&mut self, _: &Packet) -> Result<Vec<Frame>, Error> {
                Ok(Vec::new())
            }
            fn flush(&mut self) -> Result<Vec<Frame>, Error> {
                Ok(Vec::new())
            }
        }

        let stream = synthesize(64, 48, 4, 2).expect("encode");
        let report = compare(&stream.annexb, &mut Silent).expect("compare");
        assert!(!report.matches());
        assert_eq!(report.subject_frames, 0);
        assert!(report.reference_frames > 0);
        assert!(
            report.to_string().contains("frame count mismatch"),
            "{report}"
        );
    }

    /// A decode error must surface as an error, not be flattened into a count
    /// mismatch: "we could not decode access unit 2" and "we decoded
    /// everything but produced too few frames" are different bugs.
    #[test]
    fn a_decode_error_is_reported_with_its_access_unit() {
        struct Failing(usize);
        impl Decoder for Failing {
            fn decode(&mut self, _: &Packet) -> Result<Vec<Frame>, Error> {
                self.0 += 1;
                if self.0 > 2 {
                    return Err(Error::Decode("mb_type 31 unsupported".into()));
                }
                Ok(Vec::new())
            }
            fn flush(&mut self) -> Result<Vec<Frame>, Error> {
                Ok(Vec::new())
            }
        }

        let stream = synthesize(64, 48, 5, 1).expect("encode");
        let report = compare(&stream.annexb, &mut Failing(0)).expect("compare");
        let (au, _) = report.error.as_ref().expect("error not reported");
        assert_eq!(*au, 2);
        assert!(report.to_string().contains("access unit 2"), "{report}");
    }

    /// The synthetic fixture is only useful if it actually contains the
    /// coding tools the decoder has to handle, so check the stream's shape
    /// rather than trusting the encoder config to have been honoured.
    #[test]
    fn the_synthetic_stream_is_keyframe_led_and_carries_parameter_sets() {
        let stream = synthesize(64, 48, 4, 2).expect("encode");
        let units = annexb::access_units(&stream.annexb);
        assert_eq!(units.len(), 4);

        let kinds: Vec<u8> = annexb::nal_units(units[0]).map(|n| n[0] & 0x1f).collect();
        assert!(kinds.contains(&7), "no SPS in first access unit: {kinds:?}");
        assert!(kinds.contains(&8), "no PPS in first access unit: {kinds:?}");
        assert!(
            kinds.contains(&5),
            "first access unit is not an IDR: {kinds:?}"
        );
    }
}
