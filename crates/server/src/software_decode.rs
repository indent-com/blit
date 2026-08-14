//! Codec-specific software camera decoders — no FFmpeg dependency.
//!
//! H.264 is decoded by the pure-Rust `oxideav-h264` implementation and AV1
//! by the pure-Rust `rav1d` port.  Both decoders are confined to the camera
//! worker which owns this object.  They are the exact-format fallback when a
//! native NVDEC, VA-API, or Vulkan Video device rejects a profile.

#![cfg(target_os = "linux")]

use oxideav_core::{CodecId, Decoder as _, Error as OxideError, Frame, Packet, TimeBase};
use oxideav_h264::h264_decoder::H264CodecDecoder;
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I444};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_flush,
    dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data,
};
use std::fmt;
use std::mem::MaybeUninit;
use std::ptr::{self, NonNull};

const MAX_ENCODED_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    H264,
    Av1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Chroma {
    Cs420,
    Cs444,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Error {
    Unavailable(String),
    Invalid(String),
    Unsupported(String),
    Resource(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(f, "software decoder unavailable: {detail}"),
            Self::Invalid(detail) => write!(f, "invalid encoded video: {detail}"),
            Self::Unsupported(detail) => write!(f, "unsupported decoded video: {detail}"),
            Self::Resource(detail) => write!(f, "video decoder resource limit: {detail}"),
        }
    }
}

impl std::error::Error for Error {}

enum Inner {
    H264(Box<H264Decoder>),
    Av1(Av1Decoder),
}

pub(crate) struct Decoder {
    inner: Inner,
}

impl Decoder {
    pub(crate) fn new(
        codec: Codec,
        chroma: Chroma,
        width: u16,
        height: u16,
    ) -> Result<Self, Error> {
        validate_dimensions(chroma, width, height)?;
        let inner = match codec {
            Codec::H264 => Inner::H264(Box::new(H264Decoder::new(chroma, width, height))),
            Codec::Av1 => Inner::Av1(Av1Decoder::new(chroma, width, height)?),
        };
        Ok(Self { inner })
    }

    pub(crate) fn decode(
        &mut self,
        encoded: &[u8],
        keyframe: bool,
    ) -> Result<Option<Vec<u8>>, Error> {
        if encoded.is_empty() || encoded.len() > MAX_ENCODED_BYTES {
            return Err(Error::Invalid(format!(
                "encoded packet length {} is outside 1..={MAX_ENCODED_BYTES}",
                encoded.len()
            )));
        }
        match &mut self.inner {
            Inner::H264(decoder) => decoder.decode(encoded, keyframe),
            Inner::Av1(decoder) => decoder.decode(encoded),
        }
    }

    pub(crate) fn flush(&mut self) {
        match &mut self.inner {
            Inner::H264(decoder) => decoder.reset(),
            Inner::Av1(decoder) => decoder.reset(),
        }
    }
}

fn validate_dimensions(chroma: Chroma, width: u16, height: u16) -> Result<(), Error> {
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err(Error::Resource(format!(
            "dimensions {width}x{height} are outside 1..=4096"
        )));
    }
    if chroma == Chroma::Cs420 && (width & 1 != 0 || height & 1 != 0) {
        return Err(Error::Unsupported(
            "4:2:0 dimensions must both be even".into(),
        ));
    }
    Ok(())
}

struct H264Decoder {
    decoder: H264CodecDecoder,
    chroma: Chroma,
    width: usize,
    height: usize,
    timestamp: i64,
}

impl H264Decoder {
    fn new(chroma: Chroma, width: u16, height: u16) -> Self {
        Self {
            decoder: H264CodecDecoder::new(CodecId::new("h264")),
            chroma,
            width: usize::from(width),
            height: usize::from(height),
            timestamp: 0,
        }
    }

    fn decode(&mut self, encoded: &[u8], keyframe: bool) -> Result<Option<Vec<u8>>, Error> {
        // WebCodecs gives us one complete Annex-B access unit.  oxideav's
        // parser normally observes the boundary when the next VCL NAL starts;
        // append a synthetic AUD so this packet is finalized immediately
        // without flushing (which would discard its reference-picture state).
        let mut access_unit = Vec::with_capacity(encoded.len() + 6);
        access_unit.extend_from_slice(encoded);
        access_unit.extend_from_slice(&[0, 0, 0, 1, 0x09, 0x10]);
        let packet = Packet::new(0, TimeBase::new(1, 1_000_000), access_unit)
            .with_pts(self.timestamp)
            .with_keyframe(keyframe);
        self.timestamp = self.timestamp.wrapping_add(1);
        self.decoder.send_packet(&packet).map_err(map_oxide_error)?;
        // A complete camera packet is also a complete access unit. OxideAV's
        // normal streaming API keeps completed pictures in its output DPB
        // until the advertised reorder depth is exceeded; `flush` marks the
        // access-unit boundary and drains that output queue without clearing
        // SPS/PPS, reference pictures, or decoder state. `reset`, used by our
        // public `flush` method below, is the operation that discards those
        // references on a transport discontinuity.
        self.decoder.flush().map_err(map_oxide_error)?;

        let mut output = None;
        loop {
            match self.decoder.receive_frame() {
                Ok(Frame::Video(frame)) => {
                    let rgba = planar_frame_to_rgba(
                        &frame.planes,
                        self.chroma,
                        self.width,
                        self.height,
                        false,
                    )?;
                    if output.replace(rgba).is_some() {
                        return Err(Error::Invalid(
                            "one H.264 access unit produced multiple frames".into(),
                        ));
                    }
                }
                Ok(_) => {
                    return Err(Error::Unsupported(
                        "H.264 decoder returned a non-video frame".into(),
                    ));
                }
                Err(OxideError::NeedMore | OxideError::Eof) => break,
                Err(error) => return Err(map_oxide_error(error)),
            }
        }
        Ok(output)
    }

    fn reset(&mut self) {
        let _ = self.decoder.reset();
        self.timestamp = 0;
    }
}

fn map_oxide_error(error: OxideError) -> Error {
    match error {
        OxideError::InvalidData(detail) => Error::Invalid(detail),
        OxideError::Unsupported(detail) => Error::Unsupported(detail),
        other => Error::Invalid(other.to_string()),
    }
}

fn planar_frame_to_rgba(
    planes: &[oxideav_core::VideoPlane],
    chroma: Chroma,
    width: usize,
    height: usize,
    full_range: bool,
) -> Result<Vec<u8>, Error> {
    if planes.len() < 3 {
        return Err(Error::Unsupported(format!(
            "planar decoder returned {} image planes, expected 3",
            planes.len()
        )));
    }
    let (chroma_width, chroma_height) = match chroma {
        Chroma::Cs420 => (width / 2, height / 2),
        Chroma::Cs444 => (width, height),
    };
    validate_plane(&planes[0].data, planes[0].stride, width, height, "Y")?;
    validate_plane(
        &planes[1].data,
        planes[1].stride,
        chroma_width,
        chroma_height,
        "U",
    )?;
    validate_plane(
        &planes[2].data,
        planes[2].stride,
        chroma_width,
        chroma_height,
        "V",
    )?;

    let mut rgba = vec![0; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let chroma_x = if chroma == Chroma::Cs420 { x / 2 } else { x };
            let chroma_y = if chroma == Chroma::Cs420 { y / 2 } else { y };
            let yy = planes[0].data[y * planes[0].stride + x];
            let u = planes[1].data[chroma_y * planes[1].stride + chroma_x];
            let v = planes[2].data[chroma_y * planes[2].stride + chroma_x];
            write_rgba(&mut rgba, width, x, y, yy, u, v, full_range);
        }
    }
    Ok(rgba)
}

fn validate_plane(
    data: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    name: &str,
) -> Result<(), Error> {
    let required = stride
        .checked_mul(height)
        .ok_or_else(|| Error::Resource(format!("{name} plane size overflow")))?;
    if stride < width || data.len() < required {
        return Err(Error::Unsupported(format!(
            "invalid {name} plane stride/length {stride}/{} for {width}x{height}",
            data.len()
        )));
    }
    Ok(())
}

struct Av1Decoder {
    context: Option<Dav1dContext>,
    chroma: Chroma,
    width: usize,
    height: usize,
}

// The rav1d context is created, used, flushed, and destroyed exclusively on
// one camera worker thread.  Its raw Arc wrapper is an FFI ownership token;
// moving that token between threads before first use is safe, concurrent use
// is intentionally impossible through `&mut self`.
unsafe impl Send for Av1Decoder {}

impl Av1Decoder {
    fn new(chroma: Chroma, width: u16, height: u16) -> Result<Self, Error> {
        let mut settings = MaybeUninit::<Dav1dSettings>::zeroed();
        // SAFETY: the pointer targets writable, properly aligned storage for
        // the complete settings value, which rav1d initializes.
        unsafe { dav1d_default_settings(NonNull::new(settings.as_mut_ptr()).unwrap()) };
        // SAFETY: `dav1d_default_settings` initialized every field.
        let mut settings = unsafe { settings.assume_init() };
        settings.n_threads = 1;
        settings.max_frame_delay = 1;
        settings.apply_grain = 0;
        settings.frame_size_limit = u32::from(width) * u32::from(height);

        let mut context = None;
        // SAFETY: both non-null pointers refer to live writable values;
        // `dav1d_open` writes one owned context token on success.
        let result = unsafe {
            dav1d_open(
                Some(NonNull::from(&mut context)),
                Some(NonNull::from(&mut settings)),
            )
        };
        if result.0 != 0 {
            return Err(Error::Unavailable(format!(
                "rav1d initialization failed with {}",
                result.0
            )));
        }
        if context.is_none() {
            return Err(Error::Unavailable(
                "rav1d returned success without a context".into(),
            ));
        }
        Ok(Self {
            context,
            chroma,
            width: usize::from(width),
            height: usize::from(height),
        })
    }

    fn decode(&mut self, encoded: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let context = self
            .context
            .ok_or_else(|| Error::Unavailable("rav1d context is closed".into()))?;
        let mut input = Dav1dData::default();
        // SAFETY: `input` is writable and encoded.len() is bounded above.
        let destination =
            unsafe { dav1d_data_create(Some(NonNull::from(&mut input)), encoded.len()) };
        if destination.is_null() {
            return Err(Error::Resource("rav1d packet allocation failed".into()));
        }
        // SAFETY: dav1d_data_create allocated exactly encoded.len() writable
        // bytes and the source slice has that same length.
        unsafe { ptr::copy_nonoverlapping(encoded.as_ptr(), destination, encoded.len()) };

        let mut prior = None;
        loop {
            // SAFETY: context is live and input is a valid owned Dav1dData.
            let result = unsafe { dav1d_send_data(Some(context), Some(NonNull::from(&mut input))) };
            if result.0 == 0 {
                break;
            }
            if result.0 == -libc::EAGAIN {
                let frame = self.receive_one(context)?;
                if frame.is_none() || prior.replace(frame.unwrap()).is_some() {
                    // SAFETY: input remains initialized even when send_data
                    // reports EAGAIN.
                    unsafe { dav1d_data_unref(Some(NonNull::from(&mut input))) };
                    return Err(Error::Invalid(
                        "rav1d input backpressure did not yield exactly one frame".into(),
                    ));
                }
                continue;
            }
            // SAFETY: input remains initialized after an error.
            unsafe { dav1d_data_unref(Some(NonNull::from(&mut input))) };
            return Err(Error::Invalid(format!(
                "rav1d rejected packet with {}",
                result.0
            )));
        }
        // Safe and idempotent after successful consumption (the data ref is
        // empty) and required if the decoder retained a suffix.
        unsafe { dav1d_data_unref(Some(NonNull::from(&mut input))) };

        let current = self.receive_one(context)?;
        match (prior, current) {
            (Some(_), Some(_)) => Err(Error::Invalid(
                "one AV1 temporal unit produced multiple frames".into(),
            )),
            (Some(frame), None) | (None, Some(frame)) => Ok(Some(frame)),
            (None, None) => Ok(None),
        }
    }

    fn receive_one(&self, context: Dav1dContext) -> Result<Option<Vec<u8>>, Error> {
        let mut picture = Dav1dPicture::default();
        // SAFETY: context is live and picture is writable storage.
        let result = unsafe { dav1d_get_picture(Some(context), Some(NonNull::from(&mut picture))) };
        if result.0 == -libc::EAGAIN {
            return Ok(None);
        }
        if result.0 != 0 {
            return Err(Error::Invalid(format!(
                "rav1d output failed with {}",
                result.0
            )));
        }
        let converted = self.picture_to_rgba(&picture);
        // SAFETY: picture was initialized by a successful get_picture and is
        // unreferenced exactly once after all plane reads finish.
        unsafe { dav1d_picture_unref(Some(NonNull::from(&mut picture))) };
        converted.map(Some)
    }

    fn picture_to_rgba(&self, picture: &Dav1dPicture) -> Result<Vec<u8>, Error> {
        if picture.p.w != self.width as i32 || picture.p.h != self.height as i32 {
            return Err(Error::Unsupported(format!(
                "AV1 output is {}x{}, expected {}x{}",
                picture.p.w, picture.p.h, self.width, self.height
            )));
        }
        if picture.p.bpc != 8 {
            return Err(Error::Unsupported(format!(
                "AV1 output is {}-bit, expected 8-bit",
                picture.p.bpc
            )));
        }
        let expected_layout = match self.chroma {
            Chroma::Cs420 => DAV1D_PIXEL_LAYOUT_I420,
            Chroma::Cs444 => DAV1D_PIXEL_LAYOUT_I444,
        };
        if picture.p.layout != expected_layout {
            return Err(Error::Unsupported(format!(
                "AV1 output layout {} does not match {:?}",
                picture.p.layout, self.chroma
            )));
        }
        let y = picture.data[0]
            .ok_or_else(|| Error::Unsupported("AV1 output has no Y plane".into()))?
            .cast::<u8>();
        let u = picture.data[1]
            .ok_or_else(|| Error::Unsupported("AV1 output has no U plane".into()))?
            .cast::<u8>();
        let v = picture.data[2]
            .ok_or_else(|| Error::Unsupported("AV1 output has no V plane".into()))?
            .cast::<u8>();
        let full_range = picture
            .seq_hdr
            .map(|header| {
                // SAFETY: a successful Dav1dPicture owns the referenced
                // immutable sequence header until picture_unref.
                unsafe { header.as_ref().color_range != 0 }
            })
            .unwrap_or(false);

        let mut rgba = vec![0; self.width * self.height * 4];
        for row in 0..self.height {
            for column in 0..self.width {
                let chroma_column = if self.chroma == Chroma::Cs420 {
                    column / 2
                } else {
                    column
                };
                let chroma_row = if self.chroma == Chroma::Cs420 {
                    row / 2
                } else {
                    row
                };
                // SAFETY: rav1d guarantees each plane covers p.w/p.h with the
                // supplied signed strides. Chroma dimensions are derived from
                // the already-validated layout.
                let yy = unsafe {
                    *y.as_ptr()
                        .offset(picture.stride[0] * row as isize + column as isize)
                };
                let uu = unsafe {
                    *u.as_ptr()
                        .offset(picture.stride[1] * chroma_row as isize + chroma_column as isize)
                };
                let vv = unsafe {
                    *v.as_ptr()
                        .offset(picture.stride[1] * chroma_row as isize + chroma_column as isize)
                };
                write_rgba(&mut rgba, self.width, column, row, yy, uu, vv, full_range);
            }
        }
        Ok(rgba)
    }

    fn reset(&mut self) {
        if let Some(context) = self.context {
            // SAFETY: context remains owned by self and is not used
            // concurrently; flush preserves the allocation for reuse.
            unsafe { dav1d_flush(context) };
        }
    }
}

impl Drop for Av1Decoder {
    fn drop(&mut self) {
        if self.context.is_some() {
            // SAFETY: this is the unique owned context token. dav1d_close
            // consumes it and writes None back to prevent a second close.
            unsafe { dav1d_close(Some(NonNull::from(&mut self.context))) };
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_rgba(
    rgba: &mut [u8],
    width: usize,
    x: usize,
    y: usize,
    yy: u8,
    u: u8,
    v: u8,
    full_range: bool,
) {
    let y_value = i32::from(yy);
    let u = i32::from(u) - 128;
    let v = i32::from(v) - 128;
    let (r, g, b) = if full_range {
        (
            y_value + ((403 * v + 128) >> 8),
            y_value - ((48 * u + 120 * v + 128) >> 8),
            y_value + ((475 * u + 128) >> 8),
        )
    } else {
        let y_value = (y_value - 16).max(0);
        (
            (298 * y_value + 459 * v + 128) >> 8,
            (298 * y_value - 55 * u - 136 * v + 128) >> 8,
            (298 * y_value + 541 * u + 128) >> 8,
        )
    };
    let offset = (y * width + x) * 4;
    rgba[offset..offset + 4].copy_from_slice(&[
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
        255,
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    const H264_420_RED: &str = concat!(
        "000000016742c00addec0440000003004000000300a3c489e0",
        "0000000168ce0f2c8000000165888404bc4628000a8bc7000128d8e0002fad80"
    );
    const H264_444_RED: &str = concat!(
        "0000000167f4100a91977b01100000030010000003002840",
        "0000000168ee0f11211000000165888404bffef7847e0535c13bfe659341ebf9965a423d"
    );
    const AV1_420_RED: &str =
        "12000a0a000000019ff9b5f200803214100080000014b9b9ac6e4d145e9136bae9bc64a0";
    const AV1_444_RED: &str =
        "12000a09200000019ff9b5f2043215100080000014b9b9ac6e4d145e91424a8e4eaeebb0";

    #[test]
    fn decodes_all_exact_software_profiles_and_recovers() {
        for (codec, chroma, fixture) in [
            (Codec::H264, Chroma::Cs420, H264_420_RED),
            (Codec::H264, Chroma::Cs444, H264_444_RED),
            (Codec::Av1, Chroma::Cs420, AV1_420_RED),
            (Codec::Av1, Chroma::Cs444, AV1_444_RED),
        ] {
            let encoded = decode_hex(fixture);
            let mut decoder = Decoder::new(codec, chroma, 16, 16).unwrap();
            let first = decoder
                .decode(&encoded, true)
                .unwrap()
                .expect("keyframe must display immediately");
            assert_eq!(first.len(), 16 * 16 * 4);
            assert!(first.chunks_exact(4).all(|pixel| pixel[3] == 255));
            decoder.flush();
            let recovered = decoder
                .decode(&encoded, true)
                .unwrap()
                .expect("keyframe after flush must display immediately");
            assert_eq!(recovered, first);
        }
    }

    #[test]
    fn h264_access_unit_drain_preserves_reference_state() {
        use oxideav_h264::encoder::{EncodedFrameRef, Encoder, EncoderConfig, YuvFrame};

        let width = 16;
        let height = 16;
        let y0 = vec![81; width * height];
        let u0 = vec![90; width * height / 4];
        let v0 = vec![240; width * height / 4];
        let y1 = vec![145; width * height];
        let u1 = vec![54; width * height / 4];
        let v1 = vec![34; width * height / 4];
        let first = YuvFrame {
            width: width as u32,
            height: height as u32,
            y: &y0,
            u: &u0,
            v: &v0,
        };
        let second = YuvFrame {
            width: width as u32,
            height: height as u32,
            y: &y1,
            u: &u1,
            v: &v1,
        };
        let encoder = Encoder::new(EncoderConfig::new(width as u32, height as u32));
        let idr = encoder.encode_idr(&first);
        let delta = encoder.encode_p(&second, &EncodedFrameRef::from(&idr), 1, 2);

        let mut decoder = Decoder::new(Codec::H264, Chroma::Cs420, 16, 16).unwrap();
        let key_rgba = decoder
            .decode(&idr.annex_b, true)
            .unwrap()
            .expect("IDR must display immediately");
        let delta_rgba = decoder
            .decode(&delta.annex_b, false)
            .unwrap()
            .expect("dependent access unit must retain the IDR reference");
        assert_eq!(key_rgba.len(), 16 * 16 * 4);
        assert_eq!(delta_rgba.len(), 16 * 16 * 4);
        assert_ne!(delta_rgba, key_rgba);
    }

    fn decode_hex(input: &str) -> Vec<u8> {
        input
            .as_bytes()
            .chunks_exact(2)
            .map(|digits| u8::from_str_radix(std::str::from_utf8(digits).unwrap(), 16).unwrap())
            .collect()
    }
}
