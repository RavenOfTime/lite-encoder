//! Annex B byte-stream splitting.
//!
//! RTSP hands us length-prefixed AVCC packets that are already framed, so the
//! live path never needs this. Captures on disk are Annex B, though, and the
//! differential harness works from captures: to compare two decoders frame by
//! frame we first have to agree on where each frame's bytes begin and end.
//!
//! `h264-reader` splits NAL units for us but deliberately stops there, because
//! grouping NALs into access units needs slice-header context it does not want
//! to assume. We can assume it: the scope in [`super`] is one picture per
//! coded frame, no interlacing and no redundant slices, which reduces spec
//! 7.4.1.2.4 to the two rules in [`starts_access_unit`].

/// Every NAL unit payload in an Annex B stream.
///
/// Yields the bytes *between* start codes, so each slice begins with the NAL
/// header byte. Trailing zero bytes are trimmed: an encoder is allowed to pad
/// with them and they are not part of the NAL.
pub fn nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    located_nal_units(stream).map(|(_, nal)| nal)
}

/// As [`nal_units`], but pairs each NAL with the offset of its start code.
///
/// [`access_units`] needs the start code included in the bytes it hands a
/// decoder, and the code is three or four bytes depending on the encoder's
/// mood, so the offset has to come from the scanner rather than be guessed
/// back from the payload position.
fn located_nal_units(stream: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    NalUnits { stream, pos: 0 }
}

struct NalUnits<'a> {
    stream: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = (usize, &'a [u8]);

    fn next(&mut self) -> Option<(usize, &'a [u8])> {
        let three = find_start_code(self.stream, self.pos)?;
        // A leading zero before the 3-byte code makes it the 4-byte form.
        let code = if three > self.pos && self.stream[three - 1] == 0 {
            three - 1
        } else {
            three
        };
        let start = three + 3;
        // The next start code terminates this NAL. Scanning for the 3-byte
        // form finds the 4-byte form too, with one leading zero left over for
        // the trailing-zero trim below to remove.
        let end = find_start_code(self.stream, start).unwrap_or(self.stream.len());
        self.pos = end;

        let nal = &self.stream[start..end];
        let trimmed = nal.len() - nal.iter().rev().take_while(|&&b| b == 0).count();
        Some((code, &nal[..trimmed]))
    }
}

/// Index of the next `00 00 01` at or after `from`.
fn find_start_code(stream: &[u8], from: usize) -> Option<usize> {
    (from..stream.len().saturating_sub(2)).find(|&i| stream[i..i + 3] == [0, 0, 1])
}

/// Access units, as slices of the original stream including their start codes.
///
/// Each element can be handed to a decoder as one picture's worth of bytes.
pub fn access_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut start = None;
    let mut seen_vcl = false;

    for (offset, nal) in located_nal_units(stream) {
        if start.is_some() && starts_access_unit(nal, seen_vcl) {
            units.push(&stream[start.take().unwrap()..offset]);
            seen_vcl = false;
        }
        // Include the start code, which a decoder needs to resynchronise.
        start.get_or_insert(offset);
        seen_vcl |= is_vcl(nal);
    }

    if let Some(start) = start {
        units.push(&stream[start..]);
    }
    units
}

/// Whether `nal` is the first NAL of a new access unit.
///
/// Both rules are conditioned on a slice having already been seen, because
/// nothing can close an access unit that has not yet opened one: the leading
/// SPS and PPS of an IDR belong to the picture that follows them, not to a
/// unit of their own.
///
/// Given that, a slice starts a new picture when its `first_mb_in_slice` is
/// zero, which as a `ue(v)` is exactly a leading 1 bit. A parameter set, SEI
/// or delimiter starts one unconditionally, since those may only precede
/// slice data and so cannot belong to the picture already in progress.
fn starts_access_unit(nal: &[u8], seen_vcl: bool) -> bool {
    if !seen_vcl {
        return false;
    }
    match nal_unit_type(nal) {
        Some(1 | 5) => nal.get(1).is_some_and(|b| b & 0x80 != 0),
        // SEI, SPS, PPS, AUD.
        Some(6..=9) => true,
        _ => false,
    }
}

fn nal_unit_type(nal: &[u8]) -> Option<u8> {
    Some(nal.first()? & 0x1f)
}

/// Whether this NAL carries coded slice data, as opposed to metadata.
fn is_vcl(nal: &[u8]) -> bool {
    matches!(nal_unit_type(nal), Some(1..=5))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an Annex B stream from `(nal_type, four_byte_start_code)` pairs.
    /// The second payload byte is 0x80, marking `first_mb_in_slice = 0`.
    fn stream(nals: &[(u8, bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        for &(kind, long) in nals {
            out.extend_from_slice(if long { &[0, 0, 0, 1] } else { &[0, 0, 1] });
            out.extend_from_slice(&[kind, 0x80]);
        }
        out
    }

    #[test]
    fn both_start_code_lengths_split_the_same_way() {
        let short = stream(&[(7, false), (8, false), (5, false)]);
        let long = stream(&[(7, true), (8, true), (5, true)]);
        assert_eq!(nal_units(&short).count(), 3);
        assert_eq!(nal_units(&long).count(), 3);
        assert!(nal_units(&long).all(|n| n.len() == 2));
    }

    #[test]
    fn leading_garbage_before_the_first_start_code_is_skipped() {
        let mut s = vec![0xde, 0xad, 0xbe, 0xef];
        s.extend_from_slice(&stream(&[(5, true)]));
        let nals: Vec<_> = nal_units(&s).collect();
        assert_eq!(nals, vec![&[5u8, 0x80][..]]);
    }

    #[test]
    fn parameter_sets_attach_to_the_slice_that_follows_them() {
        let s = stream(&[(7, true), (8, true), (5, true), (1, true)]);
        let units = access_units(&s);
        // SPS+PPS+IDR is one access unit; the following P slice is another.
        assert_eq!(units.len(), 2);
        assert_eq!(nal_units(units[0]).count(), 3);
        assert_eq!(nal_units(units[1]).count(), 1);
    }

    #[test]
    fn a_slice_continuing_a_picture_does_not_open_an_access_unit() {
        let mut s = stream(&[(5, true)]);
        // A second slice of the same picture: first_mb_in_slice != 0, so the
        // leading bit of the payload is clear.
        s.extend_from_slice(&[0, 0, 0, 1, 5, 0x40]);
        assert_eq!(access_units(&s).len(), 1);
    }

    #[test]
    fn access_units_partition_the_stream_without_loss() {
        let s = stream(&[(7, true), (5, true), (1, false), (1, true)]);
        let units = access_units(&s);
        assert_eq!(units.concat(), s);
    }

    #[test]
    fn an_empty_stream_yields_nothing() {
        assert_eq!(access_units(&[]).len(), 0);
        assert_eq!(nal_units(&[]).count(), 0);
    }
}
