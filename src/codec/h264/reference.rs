//! The openh264 reference decoder, behind the [`Decoder`] seam.
//!
//! Cisco's implementation is the oracle the pure-Rust decoder is checked
//! against. It is compiled from vendored C and so is the exact dependency
//! this project exists to remove; it is therefore gated behind the
//! `reference-decoder` feature, never enabled by default, and used only from
//! tests and [`super::differential`].
//!
//! Correctness in a video decoder is bit-exact and failures are delayed: a
//! wrong rounding mode in deblocking does not soften one frame, it feeds
//! inter prediction and rots the picture seconds later. Hand-written unit
//! tests cannot see that coming. Diffing every frame against a decoder that
//! is known-correct can.

use std::os::raw::c_int;
use std::time::Duration;

use openh264::decoder::{Decoder as Openh264Decoder, DecoderConfig};
use openh264::formats::YUVSource;
use openh264::OpenH264API;

use crate::media::{Decoder, Frame, Packet, TrackId};
use crate::Error;

fn decode_err(e: impl std::fmt::Display) -> Error {
    Error::Decode(format!("openh264: {e}"))
}

/// A [`Decoder`] backed by Cisco's openh264.
pub struct ReferenceDecoder {
    inner: Openh264Decoder,
}

impl ReferenceDecoder {
    pub fn new() -> Result<Self, Error> {
        let config = DecoderConfig::new().debug(false);
        let inner = Openh264Decoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(decode_err)?;
        Ok(Self { inner })
    }
}

impl Decoder for ReferenceDecoder {
    /// `pkt.data` must be one Annex B access unit; see [`super::annexb`].
    ///
    /// openh264 emits at most one picture per call, so the returned vector
    /// holds zero or one frame even though the trait permits more.
    fn decode(&mut self, pkt: &Packet) -> Result<Vec<Frame>, Error> {
        let decoded = self.inner.decode(&pkt.data).map_err(decode_err)?;
        Ok(decoded
            .map(|yuv| to_frame(&yuv, pkt.pts))
            .into_iter()
            .collect())
    }

    fn flush(&mut self) -> Result<Vec<Frame>, Error> {
        // openh264 hands back the pictures still in its buffer, but without
        // usable timestamps; the harness only ever compares pixels, and the
        // live path drives this decoder one access unit at a time, so a
        // placeholder is honest here rather than a guess dressed up as data.
        let frames = self.inner.flush_remaining().map_err(decode_err)?;
        Ok(frames
            .iter()
            .map(|yuv| to_frame(yuv, Duration::ZERO))
            .collect())
    }
}

/// Copies a decoded picture out of openh264's internal buffers.
///
/// The copy is not incidental: openh264 hands out slices pointing into memory
/// it reuses on the next call, and the planes are stride-padded, which our
/// [`Frame`] permits but downstream comparison is simpler without.
fn to_frame(yuv: &impl YUVSource, pts: Duration) -> Frame {
    let (width, height) = yuv.dimensions();
    let (y_stride, u_stride, v_stride) = yuv.strides();
    Frame {
        pts,
        width: width as u32,
        height: height as u32,
        planes: [
            pack(yuv.y(), y_stride, width, height),
            pack(yuv.u(), u_stride, width.div_ceil(2), height.div_ceil(2)),
            pack(yuv.v(), v_stride, width.div_ceil(2), height.div_ceil(2)),
        ],
        strides: [width, width.div_ceil(2), width.div_ceil(2)],
    }
}

/// Drops row padding, producing a tightly packed plane.
fn pack(plane: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
    (0..height)
        .flat_map(|row| &plane[row * stride..row * stride + width])
        .copied()
        .collect()
}

/// Encodes a synthetic Annex B stream, for tests that need real bitstream
/// data without a camera on the network.
///
/// Captures from the Tapo are the streams that actually matter, but they are
/// large, and a test that only runs when someone remembered to record one is
/// a test that does not run. This produces the same coding tools — High
/// profile, CABAC, 8x8 transform — deterministically and in milliseconds.
pub struct SyntheticStream {
    pub annexb: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// Encodes `frames` pictures of a moving test pattern at `width`x`height`.
///
/// `keyframe_interval` is in frames; 1 gives an all-intra stream.
pub fn synthesize(
    width: usize,
    height: usize,
    frames: usize,
    keyframe_interval: u32,
) -> Result<SyntheticStream, Error> {
    let mut encoder = RawEncoder::new(width, height, keyframe_interval)?;
    let mut annexb = Vec::new();
    for i in 0..frames {
        let (y, u, v) = test_pattern(width, height, i);
        encoder.encode(&y, &u, &v, width, height, i, &mut annexb)?;
    }
    Ok(SyntheticStream {
        annexb,
        width,
        height,
    })
}

/// The openh264 encoder driven through its raw C interface.
///
/// The safe wrapper is used everywhere else in this module, but it cannot
/// produce a fixture this decoder can read: `iEntropyCodingModeFlag` is the
/// one parameter it never sets, openh264 defaults it to CAVLC, and the flag
/// is rejected once `InitializeExt` has run — verified, not assumed, by
/// setting it through `SetOption` on a live encoder and reading back a PPS
/// that still said CAVLC. Since it can only be set at initialisation, and the
/// wrapper owns initialisation, the encoder has to be built here.
struct RawEncoder {
    api: openh264_sys2::DynamicAPI,
    encoder: *mut openh264_sys2::ISVCEncoder,
    info: Box<openh264_sys2::SFrameBSInfo>,
}

impl RawEncoder {
    fn new(width: usize, height: usize, keyframe_interval: u32) -> Result<Self, Error> {
        use openh264_sys2::{
            SEncParamExt, API, CAMERA_VIDEO_REAL_TIME, ISVCEncoder, PRO_HIGH, RC_OFF_MODE,
            SM_SINGLE_SLICE,
        };

        let api = OpenH264API::from_source();
        let mut encoder: *mut ISVCEncoder = std::ptr::null_mut();
        // SAFETY: every call below goes through the vtable openh264 itself
        // populated, with the argument types its headers declare. `encoder` is
        // checked for null before it is dereferenced, and this struct's `Drop`
        // is the only thing that frees it.
        unsafe {
            if api.WelsCreateSVCEncoder(&raw mut encoder) != 0 || encoder.is_null() {
                return Err(Error::Encode("openh264: could not create encoder".into()));
            }
            let vtbl = &**encoder;
            let mut params = SEncParamExt::default();
            let Some(get_default) = vtbl.GetDefaultParams else {
                api.WelsDestroySVCEncoder(encoder);
                return Err(Error::Encode("openh264: no GetDefaultParams".into()));
            };
            get_default(encoder, &raw mut params);

            params.iUsageType = CAMERA_VIDEO_REAL_TIME;
            params.iPicWidth = width as c_int;
            params.iPicHeight = height as c_int;
            params.fMaxFrameRate = 25.0;
            params.uiIntraPeriod = keyframe_interval;
            // The whole reason this exists.
            params.iEntropyCodingModeFlag = 1;
            // Deterministic output matters more than bitrate: rate control
            // that adapts to wall-clock timing, or a scene-change heuristic
            // that inserts a keyframe of its own, would make the fixture
            // differ between runs and turn any regression into a coin flip.
            // Disabling frame skip also keeps one access unit per input frame,
            // which the harness relies on to compare the two decoders in
            // lockstep.
            params.iRCMode = RC_OFF_MODE;
            params.bEnableFrameSkip = false;
            params.bEnableSceneChangeDetect = false;
            params.bEnableAdaptiveQuant = false;
            params.bEnableDenoise = false;
            params.bEnableLongTermReference = false;
            // Diagnostic escape hatch: with the loop filter off, any
            // divergence is reconstruction rather than deblocking.
            if std::env::var_os("LITE_ENCODER_NO_DEBLOCK").is_some() {
                params.iLoopFilterDisableIdc = 1;
            }
            params.iMultipleThreadIdc = 1;
            params.iSpatialLayerNum = 1;
            params.iTemporalLayerNum = 1;
            params.sSpatialLayers[0].uiProfileIdc = PRO_HIGH;
            params.sSpatialLayers[0].iVideoWidth = width as c_int;
            params.sSpatialLayers[0].iVideoHeight = height as c_int;
            params.sSpatialLayers[0].fFrameRate = 25.0;
            params.sSpatialLayers[0].sSliceArgument.uiSliceMode = SM_SINGLE_SLICE;
            params.sSpatialLayers[0].sSliceArgument.uiSliceNum = 1;

            let Some(initialize) = vtbl.InitializeExt else {
                api.WelsDestroySVCEncoder(encoder);
                return Err(Error::Encode("openh264: no InitializeExt".into()));
            };
            if initialize(encoder, &raw const params) != 0 {
                api.WelsDestroySVCEncoder(encoder);
                return Err(Error::Encode("openh264: encoder rejected parameters".into()));
            }
        }
        Ok(Self {
            api,
            encoder,
            info: Box::default(),
        })
    }

    /// Encodes one picture, appending its Annex B NAL units to `out`.
    #[allow(clippy::too_many_arguments)]
    fn encode(
        &mut self,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        width: usize,
        height: usize,
        index: usize,
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        use openh264_sys2::SSourcePicture;

        let chroma_stride = width.div_ceil(2) as c_int;
        let picture = SSourcePicture {
            iColorFormat: openh264_sys2::videoFormatI420,
            iStride: [width as c_int, chroma_stride, chroma_stride, 0],
            // openh264 only reads these planes; casting away `const` is what
            // its C signature demands, not permission to write.
            pData: [
                y.as_ptr().cast_mut(),
                u.as_ptr().cast_mut(),
                v.as_ptr().cast_mut(),
                std::ptr::null_mut(),
            ],
            iPicWidth: width as c_int,
            iPicHeight: height as c_int,
            uiTimeStamp: (index as i64) * 40,
            bPsnrY: false,
            bPsnrU: false,
            bPsnrV: false,
        };
        // SAFETY: `self.encoder` was initialised in `new` and stays non-null
        // for the lifetime of this struct. The buffers read below belong to
        // the encoder and stay valid until the next `EncodeFrame`, which is
        // after they have been copied into `out`.
        unsafe {
            let Some(encode) = (**self.encoder).EncodeFrame else {
                return Err(Error::Encode("openh264: no EncodeFrame".into()));
            };
            if encode(self.encoder, &raw const picture, &raw mut *self.info) != 0 {
                return Err(Error::Encode("openh264: encode failed".into()));
            }
            for layer in &self.info.sLayerInfo[..self.info.iLayerNum as usize] {
                let total: usize = (0..layer.iNalCount as usize)
                    .map(|n| *layer.pNalLengthInByte.add(n) as usize)
                    .sum();
                out.extend_from_slice(std::slice::from_raw_parts(layer.pBsBuf, total));
            }
        }
        Ok(())
    }
}

impl Drop for RawEncoder {
    fn drop(&mut self) {
        use openh264_sys2::API;

        // SAFETY: `encoder` is non-null and initialised, and `Drop` runs once.
        unsafe {
            if let Some(uninitialize) = (**self.encoder).Uninitialize {
                uninitialize(self.encoder);
            }
            self.api.WelsDestroySVCEncoder(self.encoder);
        }
    }
}

/// A moving gradient with a hard-edged block sliding across it.
///
/// The gradient gives the transform something with low-frequency content to
/// work on, the edges give the deblocking filter something to smooth, and the
/// motion gives inter prediction a reason to produce non-zero vectors. Flat
/// or noisy input would exercise none of the three.
fn test_pattern(width: usize, height: usize, frame: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let shift = (frame * 3) % width;

    let mut y = vec![0u8; width * height];
    for row in 0..height {
        for col in 0..width {
            let gradient = ((row + col + frame) % 256) as u8;
            let in_block = (col + shift) % width < width / 8 && row % height < height / 2;
            y[row * width + col] = if in_block { 235 } else { gradient };
        }
    }

    let mut u = vec![0u8; cw * ch];
    let mut v = vec![0u8; cw * ch];
    for row in 0..ch {
        for col in 0..cw {
            u[row * cw + col] = (128 + (col as i32 - cw as i32 / 2).clamp(-64, 64)) as u8;
            v[row * cw + col] = (128 + (row as i32 - ch as i32 / 2).clamp(-64, 64)) as u8;
        }
    }
    (y, u, v)
}

/// Wraps one access unit as a [`Packet`] for the [`Decoder`] seam.
pub fn packet(au: &[u8], index: usize) -> Packet {
    Packet {
        track: TrackId(0),
        pts: Duration::from_millis(index as u64 * 40),
        keyframe: super::annexb::nal_units(au).any(|n| n[0] & 0x1f == 5),
        data: bytes::Bytes::copy_from_slice(au),
    }
}
