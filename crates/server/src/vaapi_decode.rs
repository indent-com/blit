//! Direct stateless VA-API H.264/AV1 camera decoder.
//!
//! Codec parsing, decoded-picture-buffer management, and output ordering are
//! provided by `cros-codecs`.  This module supplies its stateless backend
//! traits using blit's existing runtime-loaded `libva` entry points.  No
//! FFmpeg symbols, process, or ABI are involved.

#![allow(non_upper_case_globals)]

use crate::gpu_libs::{
    self, VA_STATUS_SUCCESS, VABufferID, VAConfigID, VAContextID, VADisplay, VASurfaceID,
};
use cros_codecs::codec::av1::parser::{
    BitDepth, FrameHeaderObu, MAX_SEGMENTS, MAX_TILE_COLS, MAX_TILE_ROWS, NUM_REF_FRAMES,
    Profile as Av1Profile, SEG_LVL_MAX, StreamInfo as Av1StreamInfo, TileGroupObu, WarpModelType,
};
use cros_codecs::codec::h264::dpb::{Dpb, DpbEntry};
use cros_codecs::codec::h264::parser::{Pps, Slice, SliceHeader, Sps};
use cros_codecs::codec::h264::picture::{Field, PictureData, Reference};
use cros_codecs::decoder::stateless::av1::{Av1, StatelessAV1DecoderBackend};
use cros_codecs::decoder::stateless::h264::{
    H264, StatelessH264DecoderBackend, get_raster_from_zigzag_4x4, get_raster_from_zigzag_8x8,
};
use cros_codecs::decoder::stateless::{
    DecodeError as CrosDecodeError, NewPictureError, NewPictureResult, StatelessBackendResult,
    StatelessDecoder, StatelessDecoderBackend, StatelessDecoderBackendPicture,
    StatelessVideoDecoder,
};
use cros_codecs::decoder::{BlockingMode, DecodedHandle, DecoderEvent, StreamInfo};
use cros_codecs::video_frame::{ReadMapping, VideoFrame, WriteMapping};
use cros_codecs::{DecodedFormat, Fourcc, Resolution};
use std::ffi::c_void;
use std::fmt;
use std::os::fd::{AsRawFd, OwnedFd};
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;

const VAEntrypointVLD: i32 = 1;
const VAConfigAttribRTFormat: i32 = 0;
const VA_PROGRESSIVE: i32 = 2;
const VAProfileH264ConstrainedBaseline: i32 = 13;
const VAProfileH264Main: i32 = 6;
const VAProfileH264High: i32 = 7;
const VAProfileAV1Profile0: i32 = 32;
const VAProfileAV1Profile1: i32 = 33;
const VA_RT_FORMAT_YUV420: u32 = 0x0000_0001;
const VA_RT_FORMAT_YUV444: u32 = 0x0000_0004;
const VAPictureParameterBufferType: i32 = 0;
const VAIQMatrixBufferType: i32 = 1;
const VASliceParameterBufferType: i32 = 4;
const VASliceDataBufferType: i32 = 5;
const VA_SLICE_DATA_FLAG_ALL: u32 = 0;
const VA_INVALID_SURFACE: u32 = u32::MAX;
const VA_PICTURE_H264_INVALID: u32 = 1;
const VA_PICTURE_H264_TOP_FIELD: u32 = 2;
const VA_PICTURE_H264_BOTTOM_FIELD: u32 = 4;
const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 8;
const VA_PICTURE_H264_LONG_TERM_REFERENCE: u32 = 16;
const VA_FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
const VA_FOURCC_444P: u32 = u32::from_le_bytes(*b"444P");
const MAX_ENCODED_BYTES: usize = 4 * 1024 * 1024;
const MAX_DIMENSION: u32 = 16_384;
const MAX_PIXELS: u64 = 67_108_864;

#[repr(C)]
struct VaConfigAttrib {
    kind: i32,
    value: u32,
}

// VAImage is kept opaque. These offsets are stable public ABI fields and are
// already used by the direct encoders in vaapi_encode.rs.
const VA_IMAGE_SIZE: usize = 120;
const VAIMG_ID_OFF: usize = 0;
const VAIMG_FOURCC_OFF: usize = 4;
const VAIMG_BUF_OFF: usize = 52;
const VAIMG_WIDTH_OFF: usize = 56;
const VAIMG_HEIGHT_OFF: usize = 58;
const VAIMG_DATA_SIZE_OFF: usize = 60;
const VAIMG_NUM_PLANES_OFF: usize = 64;
const VAIMG_PITCHES_OFF: usize = 68;
const VAIMG_OFFSETS_OFF: usize = 80;

fn va_error(operation: &str, status: i32) -> anyhow::Error {
    anyhow::anyhow!("{operation} failed with VAStatus {status}")
}

fn checked_va(operation: &str, status: i32) -> anyhow::Result<()> {
    if status == VA_STATUS_SUCCESS {
        Ok(())
    } else {
        Err(va_error(operation, status))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Codec {
    H264,
    Av1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Chroma {
    Cs420,
    Cs444,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Unavailable(String),
    Invalid(String),
    Resource(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(formatter, "VA-API decode unavailable: {detail}"),
            Self::Invalid(detail) => write!(formatter, "invalid video bitstream: {detail}"),
            Self::Resource(detail) => write!(formatter, "VA-API decode resource failure: {detail}"),
        }
    }
}

impl std::error::Error for Error {}

impl Chroma {
    fn rt_format(self) -> u32 {
        match self {
            Self::Cs420 => VA_RT_FORMAT_YUV420,
            Self::Cs444 => VA_RT_FORMAT_YUV444,
        }
    }

    fn decoded_format(self) -> DecodedFormat {
        match self {
            Self::Cs420 => DecodedFormat::NV12,
            Self::Cs444 => DecodedFormat::I444,
        }
    }
}

#[derive(Clone, Copy)]
struct Crop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

struct SequenceConfig {
    profile: i32,
    chroma: Chroma,
    coded_width: u32,
    coded_height: u32,
    crop: Crop,
    min_frames: usize,
}

/// A token required by the cros-codecs allocation interface. VA surfaces are
/// allocated and owned by [`Surface`] instead; the token deliberately has no
/// mappable storage of its own.
#[derive(Debug)]
pub(crate) struct FrameToken {
    width: u32,
    height: u32,
    chroma: Chroma,
}

impl VideoFrame for FrameToken {
    fn fourcc(&self) -> Fourcc {
        match self.chroma {
            Chroma::Cs420 => Fourcc::from(b"NV12"),
            Chroma::Cs444 => Fourcc::from(b"I444"),
        }
    }

    fn resolution(&self) -> Resolution {
        Resolution::from((self.width, self.height))
    }

    fn get_plane_size(&self) -> Vec<usize> {
        Vec::new()
    }

    fn get_plane_pitch(&self) -> Vec<usize> {
        Vec::new()
    }

    fn map<'a>(&'a self) -> Result<Box<dyn ReadMapping<'a> + 'a>, String> {
        Err("VA frame tokens are mapped through their decoded handle".into())
    }

    fn map_mut<'a>(&'a mut self) -> Result<Box<dyn WriteMapping<'a> + 'a>, String> {
        Err("VA frame tokens are mapped through their decoded handle".into())
    }
}

struct Display {
    va: &'static gpu_libs::VaFns,
    raw: VADisplay,
    _drm_fd: OwnedFd,
}

// libva serializes access to a VADisplay. Decoders live on one worker thread;
// cloned handles only keep resources alive and are never concurrently mapped.
unsafe impl Send for Display {}
unsafe impl Sync for Display {}

impl Display {
    fn open(device: &str) -> anyhow::Result<Arc<Self>> {
        let va = gpu_libs::va().map_err(|error| anyhow::anyhow!("VA-API: {error}"))?;
        let va_drm = gpu_libs::va_drm().map_err(|error| anyhow::anyhow!("VA-DRM: {error}"))?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(device)
            .map_err(|error| anyhow::anyhow!("failed to open {device}: {error}"))?;
        let drm_fd = OwnedFd::from(file);
        // SAFETY: drm_fd is live and retained by Display.
        let raw = unsafe { (va_drm.vaGetDisplayDRM)(drm_fd.as_raw_fd()) };
        if raw.is_null() {
            return Err(anyhow::anyhow!(
                "vaGetDisplayDRM returned null for {device}"
            ));
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: raw is a non-null VA display and output pointers are valid.
        checked_va("vaInitialize", unsafe {
            (va.vaInitialize)(raw, &mut major, &mut minor)
        })?;
        Ok(Arc::new(Self {
            va,
            raw,
            _drm_fd: drm_fd,
        }))
    }

    fn supports_profile(&self, profile: i32) -> anyhow::Result<()> {
        let mut entrypoints = [0_i32; 32];
        let mut count = 0;
        // SAFETY: storage is valid for the driver-advertised entrypoints.
        checked_va("vaQueryConfigEntrypoints", unsafe {
            (self.va.vaQueryConfigEntrypoints)(
                self.raw,
                profile,
                entrypoints.as_mut_ptr(),
                &mut count,
            )
        })?;
        let count = usize::try_from(count).unwrap_or(0).min(entrypoints.len());
        if entrypoints[..count].contains(&VAEntrypointVLD) {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "VAProfile {profile} has no VLD decode entrypoint"
            ))
        }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: Display exclusively owns the initialized handle.
        unsafe {
            (self.va.vaTerminate)(self.raw);
        }
    }
}

struct DecodeContext {
    display: Arc<Display>,
    config: VAConfigID,
    context: VAContextID,
    chroma: Chroma,
    width: u32,
    height: u32,
}

impl DecodeContext {
    fn new(
        display: Arc<Display>,
        profile: i32,
        chroma: Chroma,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Arc<Self>> {
        display.supports_profile(profile)?;
        let va = display.va;
        let mut config = 0;
        let mut rt_format = VaConfigAttrib {
            kind: VAConfigAttribRTFormat,
            value: chroma.rt_format(),
        };
        // Request the exact render-target format in the decoder config. In
        // particular, a profile existing is not evidence that its 4:4:4
        // subset is implemented.
        checked_va("vaCreateConfig(VLD)", unsafe {
            (va.vaCreateConfig)(
                display.raw,
                profile,
                VAEntrypointVLD,
                (&mut rt_format as *mut VaConfigAttrib).cast(),
                1,
                &mut config,
            )
        })?;
        let mut probe_surface = 0;
        let status = unsafe {
            (va.vaCreateSurfaces)(
                display.raw,
                chroma.rt_format(),
                width,
                height,
                &mut probe_surface,
                1,
                ptr::null_mut(),
                0,
            )
        };
        if status != VA_STATUS_SUCCESS {
            unsafe { (va.vaDestroyConfig)(display.raw, config) };
            return Err(va_error("vaCreateSurfaces(exact decode format)", status));
        }
        let mut context = 0;
        let status = unsafe {
            (va.vaCreateContext)(
                display.raw,
                config,
                width as i32,
                height as i32,
                VA_PROGRESSIVE,
                &mut probe_surface,
                1,
                &mut context,
            )
        };
        unsafe { (va.vaDestroySurfaces)(display.raw, &mut probe_surface, 1) };
        if status != VA_STATUS_SUCCESS {
            // SAFETY: config was created above and context was not.
            unsafe { (va.vaDestroyConfig)(display.raw, config) };
            return Err(va_error("vaCreateContext(VLD)", status));
        }
        Ok(Arc::new(Self {
            display,
            config,
            context,
            chroma,
            width,
            height,
        }))
    }
}

impl Drop for DecodeContext {
    fn drop(&mut self) {
        // SAFETY: no Surface/Buffer can outlive this Arc-owned context.
        unsafe {
            (self.display.va.vaDestroyContext)(self.display.raw, self.context);
            (self.display.va.vaDestroyConfig)(self.display.raw, self.config);
        }
    }
}

struct Buffer {
    context: Arc<DecodeContext>,
    id: VABufferID,
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: the ID was created on this live display and is owned here.
        unsafe {
            (self.context.display.va.vaDestroyBuffer)(self.context.display.raw, self.id);
        }
    }
}

struct Surface {
    context: Arc<DecodeContext>,
    id: VASurfaceID,
}

impl Surface {
    fn new(context: Arc<DecodeContext>) -> anyhow::Result<Self> {
        let mut id = 0;
        // SAFETY: output storage is valid and context retains the display.
        checked_va("vaCreateSurfaces", unsafe {
            (context.display.va.vaCreateSurfaces)(
                context.display.raw,
                context.chroma.rt_format(),
                context.width,
                context.height,
                &mut id,
                1,
                ptr::null_mut(),
                0,
            )
        })?;
        Ok(Self { context, id })
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: the surface is exclusively owned and no submission can
        // reference it after the last Arc<VaHandleInner> is dropped.
        unsafe {
            (self.context.display.va.vaDestroySurfaces)(self.context.display.raw, &mut self.id, 1);
        }
    }
}

pub(crate) struct Picture {
    surface: Arc<Surface>,
    buffers: Vec<Buffer>,
    timestamp: u64,
    full_range: bool,
    crop: Crop,
}

#[derive(Clone)]
pub(crate) struct Handle(Arc<HandleInner>);

struct HandleInner {
    surface: Arc<Surface>,
    frame: Arc<FrameToken>,
    timestamp: u64,
    full_range: bool,
    crop: Crop,
}

impl Handle {
    pub(crate) fn rgba(&self) -> anyhow::Result<Vec<u8>> {
        self.sync()?;
        read_surface_rgba(&self.0.surface, self.0.full_range, self.0.crop)
    }

    fn surface_id(&self) -> VASurfaceID {
        self.0.surface.id
    }
}

impl DecodedHandle for Handle {
    type Frame = FrameToken;

    fn video_frame(&self) -> Arc<Self::Frame> {
        self.0.frame.clone()
    }

    fn timestamp(&self) -> u64 {
        self.0.timestamp
    }

    fn coded_resolution(&self) -> Resolution {
        self.0.frame.resolution()
    }

    fn display_resolution(&self) -> Resolution {
        self.0.frame.resolution()
    }

    fn is_ready(&self) -> bool {
        false
    }

    fn sync(&self) -> anyhow::Result<()> {
        checked_va("vaSyncSurface", unsafe {
            (self.0.surface.context.display.va.vaSyncSurface)(
                self.0.surface.context.display.raw,
                self.0.surface.id,
            )
        })
    }
}

pub(crate) struct Backend {
    display: Arc<Display>,
    context: Option<Arc<DecodeContext>>,
    stream_info: Option<StreamInfo>,
    expected_chroma: Chroma,
    expected_width: u32,
    expected_height: u32,
    crop: Crop,
}

impl Backend {
    pub(crate) fn new(
        device: &str,
        expected_chroma: Chroma,
        expected_width: u32,
        expected_height: u32,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            display: Display::open(device)?,
            context: None,
            stream_info: None,
            expected_chroma,
            expected_width,
            expected_height,
            crop: Crop {
                x: 0,
                y: 0,
                width: expected_width,
                height: expected_height,
            },
        })
    }

    fn set_sequence(&mut self, sequence: SequenceConfig) -> anyhow::Result<()> {
        let SequenceConfig {
            profile,
            chroma,
            coded_width: width,
            coded_height: height,
            crop,
            min_frames,
        } = sequence;
        let Crop {
            x: crop_x,
            y: crop_y,
            width: display_width,
            height: display_height,
        } = crop;
        if chroma != self.expected_chroma {
            return Err(anyhow::anyhow!(
                "decoded chroma {chroma:?} does not match negotiated {:?}",
                self.expected_chroma
            ));
        }
        if width == 0
            || height == 0
            || width > MAX_DIMENSION
            || height > MAX_DIMENSION
            || u64::from(width) * u64::from(height) > MAX_PIXELS
        {
            return Err(anyhow::anyhow!(
                "coded dimensions {width}x{height} exceed decoder limits"
            ));
        }
        if display_width != self.expected_width || display_height != self.expected_height {
            return Err(anyhow::anyhow!(
                "decoded display dimensions {display_width}x{display_height} do not match negotiated {}x{}",
                self.expected_width,
                self.expected_height
            ));
        }
        if crop_x
            .checked_add(display_width)
            .is_none_or(|right| right > width)
            || crop_y
                .checked_add(display_height)
                .is_none_or(|bottom| bottom > height)
        {
            return Err(anyhow::anyhow!(
                "decoded crop {crop_x},{crop_y} {display_width}x{display_height} exceeds coded {width}x{height}"
            ));
        }
        self.context = Some(DecodeContext::new(
            self.display.clone(),
            profile,
            chroma,
            width,
            height,
        )?);
        self.stream_info = Some(StreamInfo {
            format: chroma.decoded_format(),
            coded_resolution: Resolution::from((width, height)),
            display_resolution: Resolution::from((display_width, display_height)),
            min_num_frames: min_frames,
        });
        self.crop = crop;
        Ok(())
    }

    fn allocate_picture(&self, timestamp: u64, full_range: bool) -> NewPictureResult<Picture> {
        let context = self.context.as_ref().ok_or_else(|| {
            NewPictureError::BackendError(anyhow::anyhow!("VA decoder has no active sequence"))
        })?;
        Ok(Picture {
            surface: Arc::new(
                Surface::new(context.clone()).map_err(NewPictureError::BackendError)?,
            ),
            buffers: Vec::new(),
            timestamp,
            full_range,
            crop: self.crop,
        })
    }

    fn create_buffer<T>(&self, kind: i32, values: &[T]) -> anyhow::Result<Buffer> {
        let context = self
            .context
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("VA decoder has no active sequence"))?;
        let size = u32::try_from(std::mem::size_of::<T>())?;
        let count = u32::try_from(values.len())?;
        if size == 0 || count == 0 {
            return Err(anyhow::anyhow!("refusing to create an empty VA buffer"));
        }
        let mut id = 0;
        checked_va("vaCreateBuffer", unsafe {
            (context.display.va.vaCreateBuffer)(
                context.display.raw,
                context.context,
                kind,
                size,
                count,
                values.as_ptr().cast_mut().cast(),
                &mut id,
            )
        })?;
        Ok(Buffer {
            context: context.clone(),
            id,
        })
    }

    fn submit(&self, picture: Picture) -> StatelessBackendResult<Handle> {
        let context = picture.surface.context.clone();
        let ids: Vec<_> = picture.buffers.iter().map(|buffer| buffer.id).collect();
        checked_va("vaBeginPicture", unsafe {
            (context.display.va.vaBeginPicture)(
                context.display.raw,
                context.context,
                picture.surface.id,
            )
        })?;
        let render = checked_va("vaRenderPicture", unsafe {
            (context.display.va.vaRenderPicture)(
                context.display.raw,
                context.context,
                ids.as_ptr().cast_mut(),
                i32::try_from(ids.len()).unwrap_or(i32::MAX),
            )
        });
        if let Err(error) = render {
            // Balance a successful begin even if rendering failed.
            unsafe { (context.display.va.vaEndPicture)(context.display.raw, context.context) };
            return Err(error.into());
        }
        checked_va("vaEndPicture", unsafe {
            (context.display.va.vaEndPicture)(context.display.raw, context.context)
        })?;
        let frame = Arc::new(FrameToken {
            width: picture.crop.width,
            height: picture.crop.height,
            chroma: context.chroma,
        });
        Ok(Handle(Arc::new(HandleInner {
            surface: picture.surface,
            frame,
            timestamp: picture.timestamp,
            full_range: picture.full_range,
            crop: picture.crop,
        })))
    }
}

impl StatelessDecoderBackend for Backend {
    type Handle = Handle;

    fn stream_info(&self) -> Option<&StreamInfo> {
        self.stream_info.as_ref()
    }

    fn reset_backend(&mut self) -> anyhow::Result<()> {
        self.context = None;
        self.stream_info = None;
        Ok(())
    }
}

impl StatelessDecoderBackendPicture<H264> for Backend {
    type Picture = Picture;
}

impl StatelessDecoderBackendPicture<Av1> for Backend {
    type Picture = Picture;
}

// ---------------------------------------------------------------------------
// H.264 VA parameter ABI and stateless backend
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct VaPictureH264 {
    picture_id: u32,
    frame_idx: u32,
    flags: u32,
    top_field_order_cnt: i32,
    bottom_field_order_cnt: i32,
    reserved: [u32; 4],
}

impl Default for VaPictureH264 {
    fn default() -> Self {
        Self {
            picture_id: VA_INVALID_SURFACE,
            frame_idx: 0,
            flags: VA_PICTURE_H264_INVALID,
            top_field_order_cnt: 0,
            bottom_field_order_cnt: 0,
            reserved: [0; 4],
        }
    }
}

#[repr(C)]
struct VaPictureParameterH264 {
    current: VaPictureH264,
    references: [VaPictureH264; 16],
    picture_width_in_mbs_minus1: u16,
    picture_height_in_mbs_minus1: u16,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    num_ref_frames: u8,
    seq_fields: u32,
    num_slice_groups_minus1: u8,
    slice_group_map_type: u8,
    slice_group_change_rate_minus1: u16,
    pic_init_qp_minus26: i8,
    pic_init_qs_minus26: i8,
    chroma_qp_index_offset: i8,
    second_chroma_qp_index_offset: i8,
    pic_fields: u32,
    frame_num: u16,
    reserved: [u32; 8],
}

#[repr(C)]
struct VaIqMatrixH264 {
    scaling_4x4: [[u8; 16]; 6],
    scaling_8x8: [[u8; 64]; 2],
    reserved: [u32; 4],
}

#[repr(C)]
struct VaSliceParameterH264 {
    slice_data_size: u32,
    slice_data_offset: u32,
    slice_data_flag: u32,
    slice_data_bit_offset: u16,
    first_mb_in_slice: u16,
    slice_type: u8,
    direct_spatial_mv_pred_flag: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    cabac_init_idc: u8,
    slice_qp_delta: i8,
    disable_deblocking_filter_idc: u8,
    slice_alpha_c0_offset_div2: i8,
    slice_beta_offset_div2: i8,
    ref_pic_list0: [VaPictureH264; 32],
    ref_pic_list1: [VaPictureH264; 32],
    luma_log2_weight_denom: u8,
    chroma_log2_weight_denom: u8,
    luma_weight_l0_flag: u8,
    luma_weight_l0: [i16; 32],
    luma_offset_l0: [i16; 32],
    chroma_weight_l0_flag: u8,
    chroma_weight_l0: [[i16; 2]; 32],
    chroma_offset_l0: [[i16; 2]; 32],
    luma_weight_l1_flag: u8,
    luma_weight_l1: [i16; 32],
    luma_offset_l1: [i16; 32],
    chroma_weight_l1_flag: u8,
    chroma_weight_l1: [[i16; 2]; 32],
    chroma_offset_l1: [[i16; 2]; 32],
    reserved: [u32; 4],
}

fn h264_profile(sps: &Sps) -> anyhow::Result<i32> {
    match sps.profile_idc {
        66 if sps.constraint_set0_flag => Ok(VAProfileH264ConstrainedBaseline),
        66 => Err(anyhow::anyhow!(
            "unconstrained H.264 Baseline is not a VA constrained-baseline stream"
        )),
        77 => Ok(VAProfileH264Main),
        88 if sps.constraint_set1_flag => Ok(VAProfileH264Main),
        88 => Err(anyhow::anyhow!(
            "unsupported unconstrained H.264 Extended profile"
        )),
        100 => Ok(VAProfileH264High),
        // libva 2.x has no H.264 High 4:4:4 Predictive profile. Mapping
        // profile_idc 244 to VAProfileH264High would falsely claim an exact
        // profile match and is known to be driver-dependent.
        244 => Err(anyhow::anyhow!(
            "H.264 High 4:4:4 Predictive has no exact VA-API profile"
        )),
        110 | 122 => Err(anyhow::anyhow!(
            "10-bit/4:2:2 H.264 profiles are not camera formats"
        )),
        profile => Err(anyhow::anyhow!("unsupported H.264 profile_idc {profile}")),
    }
}

fn h264_chroma(sps: &Sps) -> anyhow::Result<Chroma> {
    if sps.bit_depth_luma_minus8 != 0 || sps.bit_depth_chroma_minus8 != 0 {
        return Err(anyhow::anyhow!(
            "only 8-bit H.264 camera streams are supported"
        ));
    }
    match (sps.chroma_format_idc, sps.separate_colour_plane_flag) {
        (1, false) => Ok(Chroma::Cs420),
        (3, false) => Ok(Chroma::Cs444),
        (format, separate) => Err(anyhow::anyhow!(
            "unsupported H.264 chroma_format_idc={format}, separate_colour_plane={separate}"
        )),
    }
}

fn h264_full_range(sps: &Sps) -> bool {
    sps.vui_parameters_present_flag
        && sps.vui_parameters.video_signal_type_present_flag
        && sps.vui_parameters.video_full_range_flag
}

fn va_h264_picture(
    picture: &PictureData,
    surface_id: VASurfaceID,
    merge_other_field: bool,
) -> VaPictureH264 {
    let mut flags = match picture.reference() {
        Reference::LongTerm => VA_PICTURE_H264_LONG_TERM_REFERENCE,
        Reference::ShortTerm => VA_PICTURE_H264_SHORT_TERM_REFERENCE,
        Reference::None => 0,
    };
    let frame_idx = if *picture.reference() == Reference::LongTerm {
        picture.long_term_frame_idx
    } else {
        picture.frame_num
    };
    let (mut top, mut bottom) = (picture.top_field_order_cnt, picture.bottom_field_order_cnt);
    match picture.field {
        Field::Frame => {}
        Field::Top => {
            if merge_other_field {
                if let Some(other) = picture.other_field() {
                    bottom = other.borrow().bottom_field_order_cnt;
                } else {
                    flags |= VA_PICTURE_H264_TOP_FIELD;
                    bottom = 0;
                }
            } else {
                flags |= VA_PICTURE_H264_TOP_FIELD;
                bottom = 0;
            }
        }
        Field::Bottom => {
            if merge_other_field {
                if let Some(other) = picture.other_field() {
                    top = other.borrow().top_field_order_cnt;
                } else {
                    flags |= VA_PICTURE_H264_BOTTOM_FIELD;
                    top = 0;
                }
            } else {
                flags |= VA_PICTURE_H264_BOTTOM_FIELD;
                top = 0;
            }
        }
    }
    VaPictureH264 {
        picture_id: surface_id,
        frame_idx,
        flags,
        top_field_order_cnt: top,
        bottom_field_order_cnt: bottom,
        reserved: [0; 4],
    }
}

fn h264_picture_parameter(
    header: &SliceHeader,
    current_picture: &PictureData,
    current_surface: VASurfaceID,
    dpb: &Dpb<Handle>,
    sps: &Sps,
    pps: &Pps,
) -> VaPictureParameterH264 {
    let mut references = [VaPictureH264::default(); 16];
    let mut index = 0;
    // libva's canonical ordering is all short-term references followed by
    // long-term references; the slice reference lists carry prediction order.
    for entry in dpb.short_term_refs_iter().chain(dpb.long_term_refs_iter()) {
        if index == references.len() {
            break;
        }
        let pic = entry.pic.borrow();
        if pic.nonexisting || pic.is_second_field() || !pic.is_ref() {
            continue;
        }
        if let Some(handle) = entry.reference.as_ref() {
            references[index] = va_h264_picture(&pic, handle.surface_id(), true);
            index += 1;
        }
    }
    let mut seq_fields = u32::from(sps.chroma_format_idc);
    seq_fields |= u32::from(sps.separate_colour_plane_flag) << 2;
    seq_fields |= u32::from(sps.gaps_in_frame_num_value_allowed_flag) << 3;
    seq_fields |= u32::from(sps.frame_mbs_only_flag) << 4;
    seq_fields |= u32::from(sps.mb_adaptive_frame_field_flag) << 5;
    seq_fields |= u32::from(sps.direct_8x8_inference_flag) << 6;
    seq_fields |= u32::from((sps.level_idc as u8) >= 31) << 7;
    seq_fields |= u32::from(sps.log2_max_frame_num_minus4) << 8;
    seq_fields |= u32::from(sps.pic_order_cnt_type) << 12;
    seq_fields |= u32::from(sps.log2_max_pic_order_cnt_lsb_minus4) << 14;
    seq_fields |= u32::from(sps.delta_pic_order_always_zero_flag) << 18;

    let mut pic_fields = u32::from(pps.entropy_coding_mode_flag);
    pic_fields |= u32::from(pps.weighted_pred_flag) << 1;
    pic_fields |= u32::from(pps.weighted_bipred_idc) << 2;
    pic_fields |= u32::from(pps.transform_8x8_mode_flag) << 4;
    pic_fields |= u32::from(header.field_pic_flag) << 5;
    pic_fields |= u32::from(pps.constrained_intra_pred_flag) << 6;
    pic_fields |= u32::from(pps.bottom_field_pic_order_in_frame_present_flag) << 7;
    pic_fields |= u32::from(pps.deblocking_filter_control_present_flag) << 8;
    pic_fields |= u32::from(pps.redundant_pic_cnt_present_flag) << 9;
    pic_fields |= u32::from(current_picture.nal_ref_idc != 0) << 10;

    let field_factor = u32::from(!sps.frame_mbs_only_flag);
    let picture_height_in_mbs_minus1 =
        ((sps.pic_height_in_map_units_minus1 + 1) << field_factor) - 1;
    VaPictureParameterH264 {
        current: va_h264_picture(current_picture, current_surface, false),
        references,
        picture_width_in_mbs_minus1: sps.pic_width_in_mbs_minus1,
        picture_height_in_mbs_minus1,
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        num_ref_frames: sps.max_num_ref_frames,
        seq_fields,
        num_slice_groups_minus1: 0,
        slice_group_map_type: 0,
        slice_group_change_rate_minus1: 0,
        pic_init_qp_minus26: pps.pic_init_qp_minus26,
        pic_init_qs_minus26: pps.pic_init_qs_minus26,
        chroma_qp_index_offset: pps.chroma_qp_index_offset,
        second_chroma_qp_index_offset: pps.second_chroma_qp_index_offset,
        pic_fields,
        frame_num: header.frame_num,
        reserved: [0; 8],
    }
}

fn h264_iq_matrix(pps: &Pps) -> VaIqMatrixH264 {
    let mut scaling_4x4 = [[0; 16]; 6];
    let mut scaling_8x8 = [[0; 64]; 2];
    for (index, output) in scaling_4x4.iter_mut().enumerate() {
        get_raster_from_zigzag_4x4(pps.scaling_lists_4x4[index], output);
    }
    for (index, output) in scaling_8x8.iter_mut().enumerate() {
        get_raster_from_zigzag_8x8(pps.scaling_lists_8x8[index], output);
    }
    VaIqMatrixH264 {
        scaling_4x4,
        scaling_8x8,
        reserved: [0; 4],
    }
}

fn h264_ref_list(list: &[&DpbEntry<Handle>]) -> [VaPictureH264; 32] {
    let mut output = [VaPictureH264::default(); 32];
    for (slot, entry) in output.iter_mut().zip(list) {
        if let Some(handle) = entry.reference.as_ref() {
            let pic = entry.pic.borrow();
            *slot = va_h264_picture(&pic, handle.surface_id(), pic.field == Field::Frame);
        }
    }
    output
}

fn h264_slice_parameter(
    slice: &Slice,
    sps: &Sps,
    pps: &Pps,
    ref_list0: &[&DpbEntry<Handle>],
    ref_list1: &[&DpbEntry<Handle>],
) -> VaSliceParameterH264 {
    let header = &slice.header;
    let weights = &header.pred_weight_table;
    let fill_l0 = (pps.weighted_pred_flag
        && (header.slice_type.is_p() || header.slice_type.is_sp()))
        || (pps.weighted_bipred_idc == 1 && header.slice_type.is_b());
    let fill_l1 = pps.weighted_bipred_idc == 1 && header.slice_type.is_b();
    let chroma_l0 = fill_l0 && sps.chroma_array_type() != 0;
    let chroma_l1 = fill_l1 && sps.chroma_array_type() != 0;
    VaSliceParameterH264 {
        slice_data_size: slice.nalu.size as u32,
        slice_data_offset: 0,
        slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
        slice_data_bit_offset: header.header_bit_size as u16,
        first_mb_in_slice: header.first_mb_in_slice as u16,
        slice_type: header.slice_type as u8,
        direct_spatial_mv_pred_flag: u8::from(header.direct_spatial_mv_pred_flag),
        num_ref_idx_l0_active_minus1: header.num_ref_idx_l0_active_minus1,
        num_ref_idx_l1_active_minus1: header.num_ref_idx_l1_active_minus1,
        cabac_init_idc: header.cabac_init_idc,
        slice_qp_delta: header.slice_qp_delta,
        disable_deblocking_filter_idc: header.disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2: header.slice_alpha_c0_offset_div2,
        slice_beta_offset_div2: header.slice_beta_offset_div2,
        ref_pic_list0: h264_ref_list(ref_list0),
        ref_pic_list1: h264_ref_list(ref_list1),
        luma_log2_weight_denom: weights.luma_log2_weight_denom,
        chroma_log2_weight_denom: weights.chroma_log2_weight_denom,
        luma_weight_l0_flag: u8::from(fill_l0),
        luma_weight_l0: weights.luma_weight_l0,
        luma_offset_l0: weights.luma_offset_l0.map(i16::from),
        chroma_weight_l0_flag: u8::from(chroma_l0),
        chroma_weight_l0: weights.chroma_weight_l0,
        chroma_offset_l0: weights.chroma_offset_l0.map(|pair| pair.map(i16::from)),
        luma_weight_l1_flag: u8::from(fill_l1),
        luma_weight_l1: weights.luma_weight_l1,
        luma_offset_l1: weights.luma_offset_l1.map(i16::from),
        chroma_weight_l1_flag: u8::from(chroma_l1),
        chroma_weight_l1: weights.chroma_weight_l1,
        chroma_offset_l1: weights.chroma_offset_l1.map(|pair| pair.map(i16::from)),
        reserved: [0; 4],
    }
}

impl StatelessH264DecoderBackend for Backend {
    fn new_sequence(&mut self, sps: &Rc<Sps>) -> StatelessBackendResult<()> {
        if !sps.frame_mbs_only_flag {
            return Err(anyhow::anyhow!("interlaced H.264 camera input is unsupported").into());
        }
        let chroma = h264_chroma(sps)?;
        let visible = sps.visible_rectangle();
        self.set_sequence(SequenceConfig {
            profile: h264_profile(sps)?,
            chroma,
            coded_width: sps.width(),
            coded_height: sps.height(),
            crop: Crop {
                x: visible.min.x,
                y: visible.min.y,
                width: visible.max.x - visible.min.x,
                height: visible.max.y - visible.min.y,
            },
            min_frames: sps.max_dpb_frames() + 4,
        })?;
        Ok(())
    }

    fn new_picture(
        &mut self,
        timestamp: u64,
        alloc_cb: &mut dyn FnMut() -> Option<FrameToken>,
    ) -> NewPictureResult<Picture> {
        let _ = alloc_cb().ok_or(NewPictureError::OutOfOutputBuffers)?;
        let full_range = false;
        self.allocate_picture(timestamp, full_range)
    }

    fn new_field_picture(
        &mut self,
        timestamp: u64,
        first_field: &Handle,
    ) -> NewPictureResult<Picture> {
        Ok(Picture {
            surface: first_field.0.surface.clone(),
            buffers: Vec::new(),
            timestamp,
            full_range: first_field.0.full_range,
            crop: first_field.0.crop,
        })
    }

    fn start_picture(
        &mut self,
        picture: &mut Picture,
        picture_data: &PictureData,
        sps: &Sps,
        pps: &Pps,
        dpb: &Dpb<Handle>,
        header: &SliceHeader,
    ) -> StatelessBackendResult<()> {
        picture.full_range = h264_full_range(sps);
        let parameter =
            h264_picture_parameter(header, picture_data, picture.surface.id, dpb, sps, pps);
        picture.buffers.push(self.create_buffer(
            VAPictureParameterBufferType,
            std::slice::from_ref(&parameter),
        )?);
        let iq = h264_iq_matrix(pps);
        picture
            .buffers
            .push(self.create_buffer(VAIQMatrixBufferType, std::slice::from_ref(&iq))?);
        Ok(())
    }

    fn decode_slice(
        &mut self,
        picture: &mut Picture,
        slice: &Slice,
        sps: &Sps,
        pps: &Pps,
        ref_pic_list0: &[&DpbEntry<Handle>],
        ref_pic_list1: &[&DpbEntry<Handle>],
    ) -> StatelessBackendResult<()> {
        let parameter = h264_slice_parameter(slice, sps, pps, ref_pic_list0, ref_pic_list1);
        picture.buffers.push(
            self.create_buffer(VASliceParameterBufferType, std::slice::from_ref(&parameter))?,
        );
        picture
            .buffers
            .push(self.create_buffer(VASliceDataBufferType, slice.nalu.as_ref())?);
        Ok(())
    }

    fn submit_picture(&mut self, picture: Picture) -> StatelessBackendResult<Handle> {
        self.submit(picture)
    }
}

// ---------------------------------------------------------------------------
// AV1 VA parameter ABI and stateless backend
// ---------------------------------------------------------------------------

#[repr(C)]
struct VaSegmentationAv1 {
    fields: u32,
    feature_data: [[i16; SEG_LVL_MAX]; MAX_SEGMENTS],
    feature_mask: [u8; MAX_SEGMENTS],
    reserved: [u32; 4],
}

#[repr(C)]
struct VaFilmGrainAv1 {
    fields: u32,
    grain_seed: u16,
    num_y_points: u8,
    point_y_value: [u8; 14],
    point_y_scaling: [u8; 14],
    num_cb_points: u8,
    point_cb_value: [u8; 10],
    point_cb_scaling: [u8; 10],
    num_cr_points: u8,
    point_cr_value: [u8; 10],
    point_cr_scaling: [u8; 10],
    ar_coeffs_y: [i8; 24],
    ar_coeffs_cb: [i8; 25],
    ar_coeffs_cr: [i8; 25],
    cb_mult: u8,
    cb_luma_mult: u8,
    cb_offset: u16,
    cr_mult: u8,
    cr_luma_mult: u8,
    cr_offset: u16,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaWarpedMotionAv1 {
    wmtype: i32,
    wmmat: [i32; 8],
    invalid: u8,
    reserved: [u32; 4],
}

#[repr(C)]
struct VaPictureParameterAv1 {
    profile: u8,
    order_hint_bits_minus_1: u8,
    bit_depth_idx: u8,
    matrix_coefficients: u8,
    seq_info_fields: u32,
    current_frame: VASurfaceID,
    current_display_picture: VASurfaceID,
    anchor_frames_num: u8,
    anchor_frames_list: *mut VASurfaceID,
    frame_width_minus1: u16,
    frame_height_minus1: u16,
    output_frame_width_in_tiles_minus_1: u16,
    output_frame_height_in_tiles_minus_1: u16,
    ref_frame_map: [VASurfaceID; NUM_REF_FRAMES],
    ref_frame_idx: [u8; 7],
    primary_ref_frame: u8,
    order_hint: u8,
    seg_info: VaSegmentationAv1,
    film_grain_info: VaFilmGrainAv1,
    tile_cols: u8,
    tile_rows: u8,
    width_in_sbs_minus_1: [u16; MAX_TILE_COLS - 1],
    height_in_sbs_minus_1: [u16; MAX_TILE_ROWS - 1],
    tile_count_minus_1: u16,
    context_update_tile_id: u16,
    pic_info_fields: u32,
    superres_scale_denominator: u8,
    interp_filter: u8,
    filter_level: [u8; 2],
    filter_level_u: u8,
    filter_level_v: u8,
    loop_filter_info_fields: u8,
    ref_deltas: [i8; 8],
    mode_deltas: [i8; 2],
    base_qindex: u8,
    y_dc_delta_q: i8,
    u_dc_delta_q: i8,
    u_ac_delta_q: i8,
    v_dc_delta_q: i8,
    v_ac_delta_q: i8,
    qmatrix_fields: u16,
    mode_control_fields: u32,
    cdef_damping_minus_3: u8,
    cdef_bits: u8,
    cdef_y_strengths: [u8; 8],
    cdef_uv_strengths: [u8; 8],
    loop_restoration_fields: u16,
    wm: [VaWarpedMotionAv1; 7],
    reserved: [u32; 8],
}

#[repr(C)]
struct VaSliceParameterAv1 {
    slice_data_size: u32,
    slice_data_offset: u32,
    slice_data_flag: u32,
    tile_row: u16,
    tile_column: u16,
    tg_start: u16,
    tg_end: u16,
    anchor_frame_idx: u8,
    tile_idx_in_tile_list: u16,
    reserved: [u32; 4],
}

fn av1_profile_and_chroma(stream_info: &Av1StreamInfo) -> anyhow::Result<(i32, Chroma)> {
    let seq = &stream_info.seq_header;
    if seq.bit_depth != BitDepth::Depth8 {
        return Err(anyhow::anyhow!(
            "only 8-bit AV1 camera streams are supported"
        ));
    }
    if seq.color_config.mono_chrome {
        return Err(anyhow::anyhow!(
            "monochrome AV1 is not a negotiated camera format"
        ));
    }
    match (
        seq.seq_profile,
        seq.color_config.subsampling_x,
        seq.color_config.subsampling_y,
    ) {
        (Av1Profile::Profile0, true, true) => Ok((VAProfileAV1Profile0, Chroma::Cs420)),
        (Av1Profile::Profile1, false, false) => Ok((VAProfileAV1Profile1, Chroma::Cs444)),
        (profile, x, y) => Err(anyhow::anyhow!(
            "unsupported AV1 profile/chroma combination {profile:?}, subsampling_x={x}, subsampling_y={y}"
        )),
    }
}

fn av1_segmentation(hdr: &FrameHeaderObu) -> VaSegmentationAv1 {
    let seg = &hdr.segmentation_params;
    let fields = u32::from(seg.segmentation_enabled)
        | (u32::from(seg.segmentation_update_map) << 1)
        | (u32::from(seg.segmentation_temporal_update) << 2)
        | (u32::from(seg.segmentation_update_data) << 3);
    let mut feature_mask = [0; MAX_SEGMENTS];
    for (mask, enabled) in feature_mask.iter_mut().zip(&seg.feature_enabled) {
        for (index, enabled) in enabled.iter().enumerate() {
            *mask |= u8::from(*enabled) << index;
        }
    }
    VaSegmentationAv1 {
        fields,
        feature_data: seg.feature_data,
        feature_mask,
        reserved: [0; 4],
    }
}

fn av1_film_grain(hdr: &FrameHeaderObu) -> anyhow::Result<VaFilmGrainAv1> {
    let grain = &hdr.film_grain_params;
    // libva requires a second display surface when apply_grain is set. Camera
    // transport does not negotiate film grain and silently returning the
    // pre-grain reference surface would be incorrect.
    if grain.apply_grain {
        return Err(anyhow::anyhow!(
            "AV1 film-grain synthesis is unsupported by the direct VA camera path"
        ));
    }
    // The ABI requires every other field to be zero when apply_grain is zero.
    Ok(VaFilmGrainAv1 {
        fields: 0,
        grain_seed: 0,
        num_y_points: 0,
        point_y_value: [0; 14],
        point_y_scaling: [0; 14],
        num_cb_points: 0,
        point_cb_value: [0; 10],
        point_cb_scaling: [0; 10],
        num_cr_points: 0,
        point_cr_value: [0; 10],
        point_cr_scaling: [0; 10],
        ar_coeffs_y: [0; 24],
        ar_coeffs_cb: [0; 25],
        ar_coeffs_cr: [0; 25],
        cb_mult: 0,
        cb_luma_mult: 0,
        cb_offset: 0,
        cr_mult: 0,
        cr_luma_mult: 0,
        cr_offset: 0,
        reserved: [0; 4],
    })
}

fn av1_warped_motion(hdr: &FrameHeaderObu) -> [VaWarpedMotionAv1; 7] {
    let global = &hdr.global_motion_params;
    std::array::from_fn(|slot| {
        let reference = slot + 1;
        let wmtype = match global.gm_type[reference] {
            WarpModelType::Identity => 0,
            WarpModelType::Translation => 1,
            WarpModelType::RotZoom => 2,
            WarpModelType::Affine => 3,
        };
        let mut wmmat = [0; 8];
        wmmat[..6].copy_from_slice(&global.gm_params[reference]);
        VaWarpedMotionAv1 {
            wmtype,
            wmmat,
            invalid: u8::from(!global.warp_valid[reference]),
            reserved: [0; 4],
        }
    })
}

fn av1_cdef_strengths(hdr: &FrameHeaderObu) -> anyhow::Result<([u8; 8], [u8; 8])> {
    let cdef = &hdr.cdef_params;
    let count = 1_usize
        .checked_shl(cdef.cdef_bits)
        .ok_or_else(|| anyhow::anyhow!("invalid AV1 cdef_bits {}", cdef.cdef_bits))?;
    if count > 8 {
        return Err(anyhow::anyhow!("invalid AV1 CDEF strength count {count}"));
    }
    let mut y = [0; 8];
    let mut uv = [0; 8];
    for index in 0..count {
        let y_secondary = if cdef.cdef_y_sec_strength[index] == 4 {
            3
        } else {
            cdef.cdef_y_sec_strength[index]
        };
        let uv_secondary = if cdef.cdef_uv_sec_strength[index] == 4 {
            3
        } else {
            cdef.cdef_uv_sec_strength[index]
        };
        y[index] =
            u8::try_from(((cdef.cdef_y_pri_strength[index] & 0xf) << 2) | (y_secondary & 3))?;
        uv[index] =
            u8::try_from(((cdef.cdef_uv_pri_strength[index] & 0xf) << 2) | (uv_secondary & 3))?;
    }
    Ok((y, uv))
}

fn av1_picture_parameter(
    picture: &Picture,
    stream_info: &Av1StreamInfo,
    hdr: &FrameHeaderObu,
    reference_frames: &[Option<Handle>; NUM_REF_FRAMES],
) -> anyhow::Result<VaPictureParameterAv1> {
    let seq = &stream_info.seq_header;
    let (_, chroma) = av1_profile_and_chroma(stream_info)?;
    let seq_info_fields = u32::from(seq.still_picture)
        | (u32::from(seq.use_128x128_superblock) << 1)
        | (u32::from(seq.enable_filter_intra) << 2)
        | (u32::from(seq.enable_intra_edge_filter) << 3)
        | (u32::from(seq.enable_interintra_compound) << 4)
        | (u32::from(seq.enable_masked_compound) << 5)
        | (u32::from(seq.enable_dual_filter) << 6)
        | (u32::from(seq.enable_order_hint) << 7)
        | (u32::from(seq.enable_jnt_comp) << 8)
        | (u32::from(seq.enable_cdef) << 9)
        | (u32::from(seq.color_config.mono_chrome) << 10)
        | (u32::from(seq.color_config.color_range) << 11)
        | (u32::from(seq.color_config.subsampling_x) << 12)
        | (u32::from(seq.color_config.subsampling_y) << 13)
        | ((seq.color_config.chroma_sample_position as u32) << 14)
        | (u32::from(seq.film_grain_params_present) << 15);
    let ref_frame_map = std::array::from_fn(|index| {
        reference_frames[index]
            .as_ref()
            .map_or(VA_INVALID_SURFACE, Handle::surface_id)
    });
    let mut width_in_sbs_minus_1 = [0; MAX_TILE_COLS - 1];
    for (output, input) in width_in_sbs_minus_1
        .iter_mut()
        .zip(&hdr.tile_info.width_in_sbs_minus_1)
    {
        *output = u16::try_from(*input)?;
    }
    let mut height_in_sbs_minus_1 = [0; MAX_TILE_ROWS - 1];
    for (output, input) in height_in_sbs_minus_1
        .iter_mut()
        .zip(&hdr.tile_info.height_in_sbs_minus_1)
    {
        *output = u16::try_from(*input)?;
    }
    let pic_info_fields = hdr.frame_type as u32
        | (u32::from(hdr.show_frame) << 2)
        | (u32::from(hdr.showable_frame) << 3)
        | (u32::from(hdr.error_resilient_mode) << 4)
        | (u32::from(hdr.disable_cdf_update) << 5)
        | (hdr.allow_screen_content_tools << 6)
        | (hdr.force_integer_mv << 7)
        | (u32::from(hdr.allow_intrabc) << 8)
        | (u32::from(hdr.use_superres) << 9)
        | (u32::from(hdr.allow_high_precision_mv) << 10)
        | (u32::from(hdr.is_motion_mode_switchable) << 11)
        | (u32::from(hdr.use_ref_frame_mvs) << 12)
        | (u32::from(hdr.disable_frame_end_update_cdf) << 13)
        | (u32::from(hdr.tile_info.uniform_tile_spacing_flag) << 14)
        | (u32::from(hdr.allow_warped_motion) << 15);
    let loop_filter = &hdr.loop_filter_params;
    let loop_filter_info_fields = loop_filter.loop_filter_sharpness
        | (u8::from(loop_filter.loop_filter_delta_enabled) << 3)
        | (u8::from(loop_filter.loop_filter_delta_update) << 4);
    let quant = &hdr.quantization_params;
    let qmatrix_fields = u16::from(quant.using_qmatrix)
        | (u16::try_from(quant.qm_y)? << 1)
        | (u16::try_from(quant.qm_u)? << 5)
        | (u16::try_from(quant.qm_v)? << 9);
    let mode_control_fields = u32::from(quant.delta_q_present)
        | (quant.delta_q_res << 1)
        | (u32::from(loop_filter.delta_lf_present) << 3)
        | (u32::from(loop_filter.delta_lf_res) << 4)
        | (u32::from(loop_filter.delta_lf_multi) << 6)
        | ((hdr.tx_mode as u32) << 7)
        | (u32::from(hdr.reference_select) << 9)
        | (u32::from(hdr.reduced_tx_set) << 10)
        | (u32::from(hdr.skip_mode_present) << 11);
    let restoration = &hdr.loop_restoration_params;
    let loop_restoration_fields = restoration.frame_restoration_type[0] as u16
        | ((restoration.frame_restoration_type[1] as u16) << 2)
        | ((restoration.frame_restoration_type[2] as u16) << 4)
        | (u16::from(restoration.lr_unit_shift) << 6)
        | (u16::from(restoration.lr_uv_shift) << 8);
    let (cdef_y_strengths, cdef_uv_strengths) = av1_cdef_strengths(hdr)?;
    let profile = match seq.seq_profile {
        Av1Profile::Profile0 => 0,
        Av1Profile::Profile1 => 1,
        Av1Profile::Profile2 => 2,
    };
    let _ = chroma;
    Ok(VaPictureParameterAv1 {
        profile,
        order_hint_bits_minus_1: u8::try_from(seq.order_hint_bits_minus_1.max(0))?,
        bit_depth_idx: 0,
        matrix_coefficients: seq.color_config.matrix_coefficients as u8,
        seq_info_fields,
        current_frame: picture.surface.id,
        current_display_picture: VA_INVALID_SURFACE,
        anchor_frames_num: 0,
        anchor_frames_list: ptr::null_mut(),
        frame_width_minus1: u16::try_from(
            hdr.upscaled_width
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("invalid zero AV1 upscaled width"))?,
        )?,
        frame_height_minus1: u16::try_from(
            hdr.frame_height
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("invalid zero AV1 frame height"))?,
        )?,
        output_frame_width_in_tiles_minus_1: 0,
        output_frame_height_in_tiles_minus_1: 0,
        ref_frame_map,
        ref_frame_idx: hdr.ref_frame_idx,
        primary_ref_frame: u8::try_from(hdr.primary_ref_frame)?,
        order_hint: u8::try_from(hdr.order_hint)?,
        seg_info: av1_segmentation(hdr),
        film_grain_info: av1_film_grain(hdr)?,
        tile_cols: u8::try_from(hdr.tile_info.tile_cols)?,
        tile_rows: u8::try_from(hdr.tile_info.tile_rows)?,
        width_in_sbs_minus_1,
        height_in_sbs_minus_1,
        tile_count_minus_1: 0,
        context_update_tile_id: u16::try_from(hdr.tile_info.context_update_tile_id)?,
        pic_info_fields,
        superres_scale_denominator: u8::try_from(hdr.superres_denom)?,
        interp_filter: hdr.interpolation_filter as u8,
        filter_level: [
            loop_filter.loop_filter_level[0],
            loop_filter.loop_filter_level[1],
        ],
        filter_level_u: loop_filter.loop_filter_level[2],
        filter_level_v: loop_filter.loop_filter_level[3],
        loop_filter_info_fields,
        ref_deltas: loop_filter.loop_filter_ref_deltas,
        mode_deltas: loop_filter.loop_filter_mode_deltas,
        base_qindex: u8::try_from(quant.base_q_idx)?,
        y_dc_delta_q: i8::try_from(quant.delta_q_y_dc)?,
        u_dc_delta_q: i8::try_from(quant.delta_q_u_dc)?,
        u_ac_delta_q: i8::try_from(quant.delta_q_u_ac)?,
        v_dc_delta_q: i8::try_from(quant.delta_q_v_dc)?,
        v_ac_delta_q: i8::try_from(quant.delta_q_v_ac)?,
        qmatrix_fields,
        mode_control_fields,
        cdef_damping_minus_3: u8::try_from(
            hdr.cdef_params.cdef_damping.checked_sub(3).ok_or_else(|| {
                anyhow::anyhow!("invalid AV1 cdef_damping {}", hdr.cdef_params.cdef_damping)
            })?,
        )?,
        cdef_bits: u8::try_from(hdr.cdef_params.cdef_bits)?,
        cdef_y_strengths,
        cdef_uv_strengths,
        loop_restoration_fields,
        wm: av1_warped_motion(hdr),
        reserved: [0; 8],
    })
}

fn av1_slice_parameters(tile_group: &TileGroupObu<'_>) -> anyhow::Result<Vec<VaSliceParameterAv1>> {
    tile_group
        .tiles
        .iter()
        .map(|tile| {
            Ok(VaSliceParameterAv1 {
                slice_data_size: tile.tile_size,
                slice_data_offset: tile.tile_offset,
                slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
                tile_row: u16::try_from(tile.tile_row)?,
                tile_column: u16::try_from(tile.tile_col)?,
                tg_start: u16::try_from(tile_group.tg_start)?,
                tg_end: u16::try_from(tile_group.tg_end)?,
                anchor_frame_idx: 0,
                tile_idx_in_tile_list: 0,
                reserved: [0; 4],
            })
        })
        .collect()
}

impl StatelessAV1DecoderBackend for Backend {
    fn change_stream_info(&mut self, stream_info: &Av1StreamInfo) -> StatelessBackendResult<()> {
        let (profile, chroma) = av1_profile_and_chroma(stream_info)?;
        let seq = &stream_info.seq_header;
        let coded_width = u32::from(seq.max_frame_width_minus_1) + 1;
        let coded_height = u32::from(seq.max_frame_height_minus_1) + 1;
        if coded_width != self.expected_width || coded_height != self.expected_height {
            return Err(anyhow::anyhow!(
                "AV1 sequence maximum {coded_width}x{coded_height} differs from fixed camera size {}x{}",
                self.expected_width,
                self.expected_height
            )
            .into());
        }
        self.set_sequence(SequenceConfig {
            profile,
            chroma,
            coded_width,
            coded_height,
            crop: Crop {
                x: 0,
                y: 0,
                width: stream_info.render_width,
                height: stream_info.render_height,
            },
            min_frames: 16,
        })?;
        Ok(())
    }

    fn new_picture(
        &mut self,
        _hdr: &FrameHeaderObu,
        timestamp: u64,
        alloc_cb: &mut dyn FnMut() -> Option<FrameToken>,
    ) -> NewPictureResult<Picture> {
        let _ = alloc_cb().ok_or(NewPictureError::OutOfOutputBuffers)?;
        self.allocate_picture(timestamp, false)
    }

    fn begin_picture(
        &mut self,
        picture: &mut Picture,
        stream_info: &Av1StreamInfo,
        hdr: &FrameHeaderObu,
        reference_frames: &[Option<Handle>; NUM_REF_FRAMES],
    ) -> StatelessBackendResult<()> {
        if hdr.render_width != self.expected_width || hdr.render_height != self.expected_height {
            return Err(anyhow::anyhow!(
                "AV1 frame render size {}x{} changed from {}x{}",
                hdr.render_width,
                hdr.render_height,
                self.expected_width,
                self.expected_height
            )
            .into());
        }
        if hdr.upscaled_width != self.expected_width || hdr.frame_height != self.expected_height {
            return Err(anyhow::anyhow!(
                "AV1 frame output size {}x{} changed from {}x{}",
                hdr.upscaled_width,
                hdr.frame_height,
                self.expected_width,
                self.expected_height
            )
            .into());
        }
        picture.full_range = stream_info.seq_header.color_config.color_range;
        let parameter = av1_picture_parameter(picture, stream_info, hdr, reference_frames)?;
        picture.buffers.push(self.create_buffer(
            VAPictureParameterBufferType,
            std::slice::from_ref(&parameter),
        )?);
        Ok(())
    }

    fn decode_tile_group(
        &mut self,
        picture: &mut Picture,
        tile_group: TileGroupObu<'_>,
    ) -> StatelessBackendResult<()> {
        let parameters = av1_slice_parameters(&tile_group)?;
        picture
            .buffers
            .push(self.create_buffer(VASliceParameterBufferType, &parameters)?);
        picture
            .buffers
            .push(self.create_buffer(VASliceDataBufferType, tile_group.obu.as_ref())?);
        Ok(())
    }

    fn submit_picture(&mut self, picture: Picture) -> StatelessBackendResult<Handle> {
        self.submit(picture)
    }
}

type H264Decoder = StatelessDecoder<H264, Backend>;
type Av1Decoder = StatelessDecoder<Av1, Backend>;

enum Inner {
    H264(Box<H264Decoder>),
    Av1(Box<Av1Decoder>),
}

pub(crate) struct Decoder {
    inner: Inner,
    width: u32,
    height: u32,
    chroma: Chroma,
    timestamp: u64,
}

impl Decoder {
    pub(crate) fn new(
        codec: Codec,
        chroma: Chroma,
        width: u16,
        height: u16,
    ) -> Result<Self, Error> {
        let width = u32::from(width);
        let height = u32::from(height);
        validate_dimensions(chroma, width, height)?;
        let device = configured_device();
        let backend = Backend::new(&device, chroma, width, height)
            .map_err(|error| Error::Unavailable(error.to_string()))?;
        probe_display(&backend.display, codec, chroma, width, height)
            .map_err(|error| Error::Unavailable(error.to_string()))?;
        let inner = match codec {
            Codec::H264 => Inner::H264(Box::new(
                StatelessDecoder::new(backend, BlockingMode::Blocking)
                    .map_err(|error| Error::Resource(error.to_string()))?,
            )),
            Codec::Av1 => Inner::Av1(Box::new(
                StatelessDecoder::new(backend, BlockingMode::Blocking)
                    .map_err(|error| Error::Resource(error.to_string()))?,
            )),
        };
        Ok(Self {
            inner,
            width,
            height,
            chroma,
            timestamp: 0,
        })
    }

    pub(crate) fn decode(&mut self, encoded: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        if encoded.is_empty() || encoded.len() > MAX_ENCODED_BYTES {
            return Err(Error::Invalid(format!(
                "encoded packet length {}",
                encoded.len()
            )));
        }
        self.timestamp = self.timestamp.wrapping_add(1);
        let mut allocate = || {
            Some(FrameToken {
                width: self.width,
                height: self.height,
                chroma: self.chroma,
            })
        };
        match &mut self.inner {
            Inner::H264(decoder) => {
                decode_h264_access_unit(decoder.as_mut(), self.timestamp, encoded, &mut allocate)
            }
            Inner::Av1(decoder) => {
                decode_all(decoder.as_mut(), self.timestamp, encoded, &mut allocate)
            }
        }
    }

    pub(crate) fn flush(&mut self) {
        match &mut self.inner {
            Inner::H264(decoder) => discard_flush(decoder.as_mut()),
            Inner::Av1(decoder) => discard_flush(decoder.as_mut()),
        }
    }
}

fn configured_device() -> String {
    std::env::var("BLIT_VAAPI_DEVICE").unwrap_or_else(|_| "/dev/dri/renderD128".into())
}

fn validate_dimensions(chroma: Chroma, width: u32, height: u32) -> Result<(), Error> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(Error::Invalid(format!(
            "unsupported dimensions {width}x{height}"
        )));
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(Error::Resource(format!(
            "dimensions {width}x{height} exceed pixel limit"
        )));
    }
    if chroma == Chroma::Cs420 && (!width.is_multiple_of(2) || !height.is_multiple_of(2)) {
        return Err(Error::Invalid("4:2:0 dimensions must be even".into()));
    }
    Ok(())
}

fn probe_display(
    display: &Arc<Display>,
    codec: Codec,
    chroma: Chroma,
    width: u32,
    height: u32,
) -> anyhow::Result<()> {
    if codec == Codec::H264 && chroma == Chroma::Cs444 {
        return Err(anyhow::anyhow!(
            "H.264 High 4:4:4 Predictive has no exact libva 2.x decode profile"
        ));
    }
    let profiles: &[i32] = match (codec, chroma) {
        (Codec::H264, Chroma::Cs420) => &[
            VAProfileH264High,
            VAProfileH264Main,
            VAProfileH264ConstrainedBaseline,
        ],
        (Codec::H264, Chroma::Cs444) => unreachable!("rejected above"),
        (Codec::Av1, Chroma::Cs420) => &[VAProfileAV1Profile0],
        (Codec::Av1, Chroma::Cs444) => &[VAProfileAV1Profile1],
    };
    let mut errors = Vec::new();
    for profile in profiles {
        match DecodeContext::new(display.clone(), *profile, chroma, width, height) {
            Ok(context) => {
                drop(context);
                return Ok(());
            }
            Err(error) => errors.push(format!("profile {profile}: {error}")),
        }
    }
    Err(anyhow::anyhow!(errors.join("; ")))
}

fn map_cros_error(error: CrosDecodeError) -> Error {
    match error {
        CrosDecodeError::ParseFrameError(detail) => Error::Invalid(detail),
        CrosDecodeError::DecoderError(error) => Error::Invalid(error.to_string()),
        CrosDecodeError::BackendError(error) => Error::Resource(error.to_string()),
        CrosDecodeError::NotEnoughOutputBuffers(count) => {
            Error::Resource(format!("decoder needs {count} additional output buffers"))
        }
        CrosDecodeError::CheckEvents => {
            Error::Resource("decoder requested unprocessed events".into())
        }
    }
}

fn decode_all<D>(
    decoder: &mut D,
    timestamp: u64,
    encoded: &[u8],
    allocate: &mut dyn FnMut() -> Option<FrameToken>,
) -> Result<Option<Vec<u8>>, Error>
where
    D: StatelessVideoDecoder<Handle = Handle>,
{
    feed_all(decoder, timestamp, encoded, allocate)?;
    collect_output(decoder)
}

fn decode_h264_access_unit(
    decoder: &mut H264Decoder,
    timestamp: u64,
    encoded: &[u8],
    allocate: &mut dyn FnMut() -> Option<FrameToken>,
) -> Result<Option<Vec<u8>>, Error> {
    feed_all(decoder, timestamp, encoded, allocate)?;
    decoder.end_access_unit().map_err(map_cros_error)?;
    collect_output(decoder)
}

fn feed_all<D>(
    decoder: &mut D,
    timestamp: u64,
    encoded: &[u8],
    allocate: &mut dyn FnMut() -> Option<FrameToken>,
) -> Result<(), Error>
where
    D: StatelessVideoDecoder<Handle = Handle>,
{
    let mut offset = 0;
    while offset < encoded.len() {
        match decoder.decode(timestamp, &encoded[offset..], allocate) {
            Ok(0) => return Err(Error::Invalid("codec parser made no progress".into())),
            Ok(consumed) => offset += consumed,
            Err(CrosDecodeError::CheckEvents) => drain_format_events(decoder)?,
            Err(error) => return Err(map_cros_error(error)),
        }
    }
    Ok(())
}

fn collect_output<D>(decoder: &mut D) -> Result<Option<Vec<u8>>, Error>
where
    D: StatelessVideoDecoder<Handle = Handle>,
{
    let mut output = None;
    while let Some(event) = decoder.next_event() {
        match event {
            DecoderEvent::FormatChanged => {}
            DecoderEvent::FrameReady(handle) => {
                let rgba = handle
                    .rgba()
                    .map_err(|error| Error::Resource(error.to_string()))?;
                if output.replace(rgba).is_some() {
                    return Err(Error::Invalid(
                        "one camera packet produced multiple decoded frames".into(),
                    ));
                }
            }
        }
    }
    Ok(output)
}

fn drain_format_events<D>(decoder: &mut D) -> Result<(), Error>
where
    D: StatelessVideoDecoder<Handle = Handle>,
{
    let mut changed = false;
    while let Some(event) = decoder.next_event() {
        match event {
            DecoderEvent::FormatChanged => changed = true,
            DecoderEvent::FrameReady(_) => {
                return Err(Error::Resource(
                    "frame arrived while processing a format change".into(),
                ));
            }
        }
    }
    if changed {
        Ok(())
    } else {
        Err(Error::Resource(
            "decoder requested event processing without an event".into(),
        ))
    }
}

fn discard_flush<D>(decoder: &mut D)
where
    D: StatelessVideoDecoder<Handle = Handle>,
{
    let _ = decoder.flush();
    while decoder.next_event().is_some() {}
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("fixed VAImage field"),
    )
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_ne_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("fixed VAImage field"),
    )
}

#[derive(Clone, Copy)]
struct ImageLayout {
    width: usize,
    height: usize,
    pitches: [usize; 3],
    offsets: [usize; 3],
    data_size: usize,
}

fn read_surface_rgba(surface: &Surface, full_range: bool, crop: Crop) -> anyhow::Result<Vec<u8>> {
    let context = &surface.context;
    let mut image = [0_u8; VA_IMAGE_SIZE];
    checked_va("vaDeriveImage", unsafe {
        (context.display.va.vaDeriveImage)(
            context.display.raw,
            surface.id,
            image.as_mut_ptr().cast(),
        )
    })?;
    let image_id = read_u32(&image, VAIMG_ID_OFF);
    let buffer_id = read_u32(&image, VAIMG_BUF_OFF);
    let fourcc = read_u32(&image, VAIMG_FOURCC_OFF);
    let image_width = u32::from(read_u16(&image, VAIMG_WIDTH_OFF));
    let image_height = u32::from(read_u16(&image, VAIMG_HEIGHT_OFF));
    let data_size = read_u32(&image, VAIMG_DATA_SIZE_OFF) as usize;
    let num_planes = read_u32(&image, VAIMG_NUM_PLANES_OFF);
    if image_width < context.width || image_height < context.height {
        unsafe { (context.display.va.vaDestroyImage)(context.display.raw, image_id) };
        return Err(anyhow::anyhow!(
            "VA derived image {image_width}x{image_height} is smaller than decode surface {}x{}",
            context.width,
            context.height
        ));
    }
    let pitches = [
        read_u32(&image, VAIMG_PITCHES_OFF) as usize,
        read_u32(&image, VAIMG_PITCHES_OFF + 4) as usize,
        read_u32(&image, VAIMG_PITCHES_OFF + 8) as usize,
    ];
    let offsets = [
        read_u32(&image, VAIMG_OFFSETS_OFF) as usize,
        read_u32(&image, VAIMG_OFFSETS_OFF + 4) as usize,
        read_u32(&image, VAIMG_OFFSETS_OFF + 8) as usize,
    ];
    let layout = ImageLayout {
        width: context.width as usize,
        height: context.height as usize,
        pitches,
        offsets,
        data_size,
    };
    let mut mapped: *mut c_void = ptr::null_mut();
    let map_status =
        unsafe { (context.display.va.vaMapBuffer)(context.display.raw, buffer_id, &mut mapped) };
    if map_status != VA_STATUS_SUCCESS || mapped.is_null() {
        unsafe { (context.display.va.vaDestroyImage)(context.display.raw, image_id) };
        return Err(va_error("vaMapBuffer(decoded image)", map_status));
    }
    let result = match (context.chroma, fourcc) {
        (Chroma::Cs420, VA_FOURCC_NV12) if num_planes >= 2 => {
            convert_nv12(mapped.cast(), layout, full_range, crop)
        }
        (Chroma::Cs444, VA_FOURCC_444P) if num_planes >= 3 => {
            convert_444p(mapped.cast(), layout, full_range, crop)
        }
        _ => Err(anyhow::anyhow!(
            "VA derived image fourcc {:?} does not match negotiated {:?}",
            fourcc.to_le_bytes(),
            context.chroma
        )),
    };
    unsafe {
        (context.display.va.vaUnmapBuffer)(context.display.raw, buffer_id);
        (context.display.va.vaDestroyImage)(context.display.raw, image_id);
    }
    result
}

fn convert_nv12(
    base: *const u8,
    layout: ImageLayout,
    full_range: bool,
    crop: Crop,
) -> anyhow::Result<Vec<u8>> {
    let ImageLayout {
        width,
        height,
        pitches,
        offsets,
        data_size,
    } = layout;
    if pitches[0] < width
        || pitches[1] < width
        || !plane_fits(offsets[0], pitches[0], width, height, data_size)
        || !plane_fits(offsets[1], pitches[1], width, height.div_ceil(2), data_size)
    {
        return Err(anyhow::anyhow!("invalid NV12 pitch returned by VA driver"));
    }
    let crop_x = crop.x as usize;
    let crop_y = crop.y as usize;
    let output_width = crop.width as usize;
    let output_height = crop.height as usize;
    if !crop_x.is_multiple_of(2) || !crop_y.is_multiple_of(2) {
        return Err(anyhow::anyhow!("NV12 crop origin must be chroma aligned"));
    }
    let mut rgba = vec![0; output_width * output_height * 4];
    for y in 0..output_height {
        for x in 0..output_width {
            let source_x = crop_x + x;
            let source_y = crop_y + y;
            // SAFETY: VAImage pitches/offsets describe the mapped image and
            // were validated for the accessed row widths above.
            let yy = unsafe { *base.add(offsets[0] + source_y * pitches[0] + source_x) };
            let uv = offsets[1] + (source_y / 2) * pitches[1] + (source_x / 2) * 2;
            let u = unsafe { *base.add(uv) };
            let v = unsafe { *base.add(uv + 1) };
            write_rgba(&mut rgba, output_width, x, y, yy, u, v, full_range);
        }
    }
    Ok(rgba)
}

fn convert_444p(
    base: *const u8,
    layout: ImageLayout,
    full_range: bool,
    crop: Crop,
) -> anyhow::Result<Vec<u8>> {
    let ImageLayout {
        width,
        height,
        pitches,
        offsets,
        data_size,
    } = layout;
    if pitches.iter().any(|pitch| *pitch < width)
        || offsets
            .iter()
            .zip(pitches)
            .any(|(offset, pitch)| !plane_fits(*offset, pitch, width, height, data_size))
    {
        return Err(anyhow::anyhow!("invalid 444P pitch returned by VA driver"));
    }
    let crop_x = crop.x as usize;
    let crop_y = crop.y as usize;
    let output_width = crop.width as usize;
    let output_height = crop.height as usize;
    let mut rgba = vec![0; output_width * output_height * 4];
    for y in 0..output_height {
        for x in 0..output_width {
            let source_x = crop_x + x;
            let source_y = crop_y + y;
            let yy = unsafe { *base.add(offsets[0] + source_y * pitches[0] + source_x) };
            let u = unsafe { *base.add(offsets[1] + source_y * pitches[1] + source_x) };
            let v = unsafe { *base.add(offsets[2] + source_y * pitches[2] + source_x) };
            write_rgba(&mut rgba, output_width, x, y, yy, u, v, full_range);
        }
    }
    Ok(rgba)
}

fn plane_fits(
    offset: usize,
    pitch: usize,
    row_bytes: usize,
    rows: usize,
    data_size: usize,
) -> bool {
    if rows == 0 {
        return true;
    }
    offset
        .checked_add((rows - 1).saturating_mul(pitch))
        .and_then(|last_row| last_row.checked_add(row_bytes))
        .is_some_and(|end| end <= data_size)
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
    let yf = if full_range {
        yy as f32
    } else {
        ((yy as f32 - 16.0) * (255.0 / 219.0)).max(0.0)
    };
    let uf = u as f32 - 128.0;
    let vf = v as f32 - 128.0;
    let scale = if full_range { 1.0 } else { 255.0 / 224.0 };
    let r = (yf + 1.5748 * vf * scale).round().clamp(0.0, 255.0) as u8;
    let g = (yf - 0.1873 * uf * scale - 0.4681 * vf * scale)
        .round()
        .clamp(0.0, 255.0) as u8;
    let b = (yf + 1.8556 * uf * scale).round().clamp(0.0, 255.0) as u8;
    let offset = (y * width + x) * 4;
    rgba[offset..offset + 4].copy_from_slice(&[r, g, b, 255]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn h264_va_abi_matches_libva_2() {
        assert_eq!(size_of::<VaPictureH264>(), 36);
        assert_eq!(size_of::<VaPictureParameterH264>(), 672);
        assert_eq!(offset_of!(VaPictureParameterH264, seq_fields), 620);
        assert_eq!(offset_of!(VaPictureParameterH264, pic_fields), 632);
        assert_eq!(size_of::<VaIqMatrixH264>(), 240);
        assert_eq!(size_of::<VaSliceParameterH264>(), 3128);
        assert_eq!(offset_of!(VaSliceParameterH264, ref_pic_list0), 28);
        assert_eq!(offset_of!(VaSliceParameterH264, reserved), 3112);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn av1_va_abi_matches_libva_2() {
        assert_eq!(size_of::<VaSegmentationAv1>(), 156);
        assert_eq!(size_of::<VaFilmGrainAv1>(), 176);
        assert_eq!(size_of::<VaWarpedMotionAv1>(), 56);
        assert_eq!(size_of::<VaPictureParameterAv1>(), 1160);
        assert_eq!(offset_of!(VaPictureParameterAv1, anchor_frames_list), 24);
        assert_eq!(offset_of!(VaPictureParameterAv1, seg_info), 84);
        assert_eq!(offset_of!(VaPictureParameterAv1, film_grain_info), 240);
        assert_eq!(offset_of!(VaPictureParameterAv1, tile_cols), 416);
        assert_eq!(offset_of!(VaPictureParameterAv1, pic_info_fields), 676);
        assert_eq!(offset_of!(VaPictureParameterAv1, wm), 732);
        assert_eq!(offset_of!(VaPictureParameterAv1, reserved), 1124);
        assert_eq!(size_of::<VaSliceParameterAv1>(), 40);
        assert_eq!(offset_of!(VaSliceParameterAv1, tile_idx_in_tile_list), 22);
    }

    #[test]
    fn nv12_crop_uses_visible_h264_rectangle() {
        // Four coded luma rows, two interleaved chroma rows. Crop off the
        // padded bottom half and ensure output has negotiated dimensions.
        let data = [
            16, 235, // Y row 0
            16, 235, // Y row 1
            81, 81, // padded Y row 2
            81, 81, // padded Y row 3
            128, 128, // UV row 0
            128, 128, // UV row 1
        ];
        let rgba = convert_nv12(
            data.as_ptr(),
            ImageLayout {
                width: 2,
                height: 4,
                pitches: [2, 2, 0],
                offsets: [0, 8, 0],
                data_size: data.len(),
            },
            false,
            Crop {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
        )
        .unwrap();
        assert_eq!(rgba.len(), 2 * 2 * 4);
        assert_eq!(&rgba[..4], &[0, 0, 0, 255]);
        assert_eq!(&rgba[4..8], &[255, 255, 255, 255]);
    }

    #[test]
    fn driver_plane_bounds_are_checked() {
        assert!(plane_fits(4, 8, 4, 2, 16));
        assert!(!plane_fits(4, 8, 5, 2, 16));
        assert!(!plane_fits(usize::MAX, 8, 4, 2, usize::MAX));
    }

    #[test]
    fn dimension_limits_are_exact() {
        assert!(validate_dimensions(Chroma::Cs420, 1920, 1080).is_ok());
        assert!(validate_dimensions(Chroma::Cs420, 1919, 1080).is_err());
        assert!(validate_dimensions(Chroma::Cs444, 1919, 1079).is_ok());
        assert!(validate_dimensions(Chroma::Cs444, 0, 1).is_err());
    }

    #[test]
    fn h264_high_444_is_not_aliased_to_va_high() {
        let mut sps = Sps::default();
        sps.profile_idc = 244;
        sps.chroma_format_idc = 3;
        assert_eq!(h264_chroma(&sps).unwrap(), Chroma::Cs444);
        assert!(h264_profile(&sps).is_err());
    }

    fn annexb_access_units(stream: &[u8]) -> Vec<&[u8]> {
        let mut nal_units = Vec::new();
        let mut cursor = 0;
        while cursor + 3 < stream.len() {
            let start_code = if stream[cursor..].starts_with(&[0, 0, 0, 1]) {
                4
            } else if stream[cursor..].starts_with(&[0, 0, 1]) {
                3
            } else {
                cursor += 1;
                continue;
            };
            let start = cursor;
            let payload = cursor + start_code;
            cursor = payload;
            while cursor + 3 < stream.len()
                && !stream[cursor..].starts_with(&[0, 0, 1])
                && !stream[cursor..].starts_with(&[0, 0, 0, 1])
            {
                cursor += 1;
            }
            let end = if cursor + 3 >= stream.len() {
                stream.len()
            } else {
                cursor
            };
            if payload < end {
                nal_units.push((start, end, payload));
            }
        }

        let mut boundaries = vec![0];
        let mut saw_vcl = false;
        for &(start, end, payload) in &nal_units {
            let nal_type = stream[payload] & 0x1f;
            if matches!(nal_type, 1..=5) {
                // first_mb_in_slice is the first unsigned Exp-Golomb value in
                // the RBSP. Its zero encoding is the single high bit.
                let first_mb_is_zero = payload + 1 < end && stream[payload + 1] & 0x80 != 0;
                if saw_vcl && first_mb_is_zero {
                    boundaries.push(start);
                }
                saw_vcl = true;
            }
        }
        boundaries.push(stream.len());
        boundaries
            .windows(2)
            .filter_map(|range| (range[0] < range[1]).then_some(&stream[range[0]..range[1]]))
            .collect()
    }

    fn ivf_frames(stream: &[u8]) -> Vec<&[u8]> {
        assert!(stream.len() >= 32 && &stream[..4] == b"DKIF");
        let mut frames = Vec::new();
        let mut offset = 32;
        while offset + 12 <= stream.len() {
            let size = u32::from_le_bytes(stream[offset..offset + 4].try_into().unwrap()) as usize;
            let start = offset + 12;
            let Some(end) = start.checked_add(size) else {
                break;
            };
            if end > stream.len() {
                break;
            }
            frames.push(&stream[start..end]);
            offset = end;
        }
        frames
    }

    fn real_fixture(variable: &str, bundled: &[u8]) -> Vec<u8> {
        std::env::var_os(variable)
            .map(std::fs::read)
            .transpose()
            .unwrap()
            .unwrap_or_else(|| bundled.to_vec())
    }

    fn assert_real_rgba(frame: &[u8], width: usize, height: usize) {
        assert_eq!(frame.len(), width * height * 4);
        assert!(frame.chunks_exact(4).all(|pixel| pixel[3] == 255));
        let first = &frame[..3];
        assert!(frame.chunks_exact(4).any(|pixel| &pixel[..3] != first));
    }

    #[test]
    #[ignore = "requires BLIT_VAAPI_REAL_TEST=1 and a VA render device"]
    fn real_device_h264_av1_420_and_exact_444_probe() {
        if std::env::var("BLIT_VAAPI_REAL_TEST").as_deref() != Ok("1") {
            eprintln!("set BLIT_VAAPI_REAL_TEST=1 to run the VA hardware test");
            return;
        }

        let h264 = real_fixture(
            "BLIT_VAAPI_TEST_H264",
            include_bytes!("../../../vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"),
        );
        let access_units = annexb_access_units(&h264);
        assert!(access_units.len() >= 2);
        let mut h264_decoder = Decoder::new(Codec::H264, Chroma::Cs420, 320, 240).unwrap();
        let mut h264_outputs = Vec::new();
        for access_unit in access_units.iter().take(12) {
            if let Some(frame) = h264_decoder.decode(access_unit).unwrap() {
                assert_real_rgba(&frame, 320, 240);
                h264_outputs.push(frame);
                if h264_outputs.len() == 2 {
                    break;
                }
            }
        }
        assert_eq!(
            h264_outputs.len(),
            2,
            "H.264 key/delta AUs did not produce two frames"
        );

        let av1 = real_fixture(
            "BLIT_VAAPI_TEST_AV1",
            include_bytes!(
                "../../../vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1"
            ),
        );
        let frames = ivf_frames(&av1);
        assert!(frames.len() >= 2);
        let mut av1_decoder = Decoder::new(Codec::Av1, Chroma::Cs420, 320, 240).unwrap();
        for frame in frames.iter().take(2) {
            let rgba = av1_decoder
                .decode(frame)
                .unwrap()
                .expect("AV1 key/delta frame produced no output");
            assert_real_rgba(&rgba, 320, 240);
        }

        for codec in [Codec::H264, Codec::Av1] {
            match Decoder::new(codec, Chroma::Cs444, 320, 240) {
                Ok(_) => eprintln!("{codec:?} 4:4:4 VA decode is supported"),
                Err(Error::Unavailable(detail)) => {
                    assert!(
                        !detail.is_empty(),
                        "4:4:4 rejection must explain the probe failure"
                    )
                }
                Err(error) => panic!("4:4:4 probe returned the wrong error class: {error}"),
            }
        }
    }
}
