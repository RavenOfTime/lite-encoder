//! Shared Matroska container writer behind [`super::WebmMuxer`] and
//! [`super::MkvMuxer`].
//!
//! WebM is a restricted Matroska profile: identical element structure, a
//! narrower codec list, and a `"webm"` DocType instead of `"matroska"`. The
//! two muxers differ only in those two things, so the cluster-batching core
//! — the part worth getting right once — lives here.
//!
//! Written for recording rather than for file conversion, so the priorities
//! are: the file on disk is always playable, and a crash costs at most one
//! cluster. The Segment is left unknown-sized while open and clusters are
//! buffered then written whole, so a truncated file stays valid up to its
//! last complete cluster.

use super::ebml::{self, id};
use crate::media::time::{self, webm_block_offset, webm_ticks, MAX_CLUSTER_SPAN};
use crate::media::{Codec, Packet, Track, TrackKind};
use crate::Error;
use std::io::Write;
use std::time::Duration;

pub(super) struct MatroskaMuxer<W: Write> {
    out: W,
    tracks: Vec<Track>,
    cluster: Vec<u8>,
    cluster_base: Option<Duration>,
    bytes_written: u64,
    last_pts: Duration,
    finalized: bool,
}

impl<W: Write> MatroskaMuxer<W> {
    /// Write the EBML header and track headers.
    ///
    /// `accept` rejects any track this muxer's flavour cannot legally carry,
    /// so an invalid file is never created in the first place.
    pub(super) fn new(
        mut out: W,
        tracks: Vec<Track>,
        doctype: &str,
        accept: &dyn Fn(Codec) -> bool,
    ) -> Result<Self, Error> {
        if tracks.is_empty() {
            return Err(Error::Mux(
                "a Matroska file needs at least one track".into(),
            ));
        }

        let mut head = Vec::new();
        Self::write_ebml_header(&mut head, doctype);
        ebml::write_id(&mut head, id::SEGMENT);
        head.extend_from_slice(&ebml::UNKNOWN_SIZE);
        Self::write_info(&mut head);
        Self::write_tracks(&mut head, &tracks, accept)?;

        out.write_all(&head)?;
        let bytes_written = head.len() as u64;

        Ok(MatroskaMuxer {
            out,
            tracks,
            cluster: Vec::with_capacity(256 * 1024),
            cluster_base: None,
            bytes_written,
            last_pts: Duration::ZERO,
            finalized: false,
        })
    }

    fn write_ebml_header(out: &mut Vec<u8>, doctype: &str) {
        let mut b = Vec::new();
        ebml::write_uint(&mut b, id::EBML_VERSION, 1);
        ebml::write_uint(&mut b, id::EBML_READ_VERSION, 1);
        ebml::write_uint(&mut b, id::EBML_MAX_ID_LENGTH, 4);
        ebml::write_uint(&mut b, id::EBML_MAX_SIZE_LENGTH, 8);
        ebml::write_string(&mut b, id::DOC_TYPE, doctype);
        ebml::write_uint(&mut b, id::DOC_TYPE_VERSION, 2);
        ebml::write_uint(&mut b, id::DOC_TYPE_READ_VERSION, 2);
        ebml::write_master(out, id::EBML, &b);
    }

    fn write_info(out: &mut Vec<u8>) {
        let mut b = Vec::new();
        ebml::write_uint(&mut b, id::TIMESTAMP_SCALE, time::WEBM_TIMESTAMP_SCALE_NS);
        ebml::write_string(&mut b, id::MUXING_APP, "liteenc");
        ebml::write_string(&mut b, id::WRITING_APP, "liteenc");
        // Duration is deliberately omitted: it is unknown while recording,
        // and players treat its absence as "live".
        ebml::write_master(out, id::INFO, &b);
    }

    fn write_tracks(
        out: &mut Vec<u8>,
        tracks: &[Track],
        accept: &dyn Fn(Codec) -> bool,
    ) -> Result<(), Error> {
        let mut all = Vec::new();
        for t in tracks {
            let codec_id = t
                .codec
                .matroska_id()
                .ok_or_else(|| Error::Mux(format!("{:?} has no Matroska CodecID", t.codec)))?;
            if !accept(t.codec) {
                return Err(Error::Mux(format!(
                    "{:?} is not carried by this muxer",
                    t.codec
                )));
            }

            let mut e = Vec::new();
            ebml::write_uint(&mut e, id::TRACK_NUMBER, t.id.0 as u64);
            ebml::write_uint(&mut e, id::TRACK_UID, t.id.0 as u64);
            ebml::write_uint(&mut e, id::FLAG_LACING, 0);
            ebml::write_string(&mut e, id::CODEC_ID, codec_id);

            match &t.kind {
                TrackKind::Video { width, height } => {
                    ebml::write_uint(&mut e, id::TRACK_TYPE, 1);
                    let mut v = Vec::new();
                    ebml::write_uint(&mut v, id::PIXEL_WIDTH, *width as u64);
                    ebml::write_uint(&mut v, id::PIXEL_HEIGHT, *height as u64);
                    ebml::write_master(&mut e, id::VIDEO, &v);
                }
                TrackKind::Audio {
                    sample_rate,
                    channels,
                } => {
                    ebml::write_uint(&mut e, id::TRACK_TYPE, 2);
                    let mut a = Vec::new();
                    ebml::write_float(&mut a, id::SAMPLING_FREQUENCY, *sample_rate as f64);
                    ebml::write_uint(&mut a, id::CHANNELS, *channels as u64);
                    ebml::write_master(&mut e, id::AUDIO, &a);
                }
            }

            // Opus in particular is undecodable without its OpusHead.
            if !t.extra_data.is_empty() {
                ebml::write_bytes(&mut e, id::CODEC_PRIVATE, &t.extra_data);
            } else if t.codec == Codec::Opus {
                return Err(Error::Mux(
                    "Opus track requires OpusHead in extra_data".into(),
                ));
            }

            ebml::write_master(&mut all, id::TRACK_ENTRY, &e);
        }
        ebml::write_master(out, id::TRACKS, &all);
        Ok(())
    }

    /// Append a packet.
    ///
    /// Starts a new cluster on a video keyframe, or when the current cluster
    /// has run long, so every cluster begins at a seekable point.
    pub(super) fn write_packet(&mut self, pkt: &Packet) -> Result<(), Error> {
        if self.finalized {
            return Err(Error::Mux("muxer already finalized".into()));
        }
        let track = self
            .tracks
            .iter()
            .find(|t| t.id == pkt.track)
            .ok_or_else(|| Error::Mux(format!("unknown track {:?}", pkt.track)))?;
        let is_video = track.codec.is_video();
        let track_num = track.id.0 as u64;

        let base = match self.cluster_base {
            None => {
                self.cluster_base = Some(pkt.pts);
                pkt.pts
            }
            Some(base) => {
                let long = pkt.pts.saturating_sub(base) >= MAX_CLUSTER_SPAN;
                if (is_video && pkt.keyframe) || long {
                    self.flush_cluster()?;
                    self.cluster_base = Some(pkt.pts);
                    pkt.pts
                } else {
                    base
                }
            }
        };

        let rel_ms = webm_block_offset(pkt.pts, base)?;

        ebml::write_simple_block(
            &mut self.cluster,
            track_num,
            rel_ms,
            pkt.keyframe,
            &pkt.data,
        );
        self.last_pts = self.last_pts.max(pkt.pts);
        Ok(())
    }

    fn flush_cluster(&mut self) -> Result<(), Error> {
        let Some(base) = self.cluster_base else {
            return Ok(());
        };
        if self.cluster.is_empty() {
            return Ok(());
        }

        let mut body = Vec::with_capacity(self.cluster.len() + 16);
        ebml::write_uint(&mut body, id::TIMESTAMP, webm_ticks(base));
        body.extend_from_slice(&self.cluster);

        let mut framed = Vec::with_capacity(body.len() + 16);
        ebml::write_master(&mut framed, id::CLUSTER, &body);

        self.out.write_all(&framed)?;
        self.bytes_written += framed.len() as u64;
        self.cluster.clear();
        Ok(())
    }

    /// Flush the open cluster so everything written so far is durable.
    pub(super) fn flush(&mut self) -> Result<(), Error> {
        self.flush_cluster()?;
        self.cluster_base = None;
        self.out.flush()?;
        Ok(())
    }

    /// Close the file and report its size.
    pub(super) fn finalize(mut self) -> Result<u64, Error> {
        self.flush()?;
        self.finalized = true;
        Ok(self.bytes_written)
    }

    /// Bytes on disk plus whatever is buffered in the open cluster.
    pub(super) fn bytes_written(&self) -> u64 {
        self.bytes_written + self.cluster.len() as u64
    }

    /// Media duration seen so far.
    pub(super) fn media_duration(&self) -> Duration {
        self.last_pts
    }
}

#[cfg(test)]
impl<W: Write> MatroskaMuxer<W> {
    /// Unwrap the writer. Test-only: production code drains bytes through
    /// `finalize`'s byte count, never the writer itself.
    pub(super) fn into_writer(self) -> W {
        self.out
    }
}
