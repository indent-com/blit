//! Direct NVDEC camera decoder -- no FFmpeg dependency.
//!
//! `libnvcuvid` owns the H.264/AV1 parser and reference-picture state.  Its
//! parser callbacks provide complete, codec-specific `CUVIDPICPARAMS` records,
//! which are passed directly to the hardware decoder.  Decoded NV12 or planar
//! YUV444 surfaces are copied through the CUDA driver API and converted to the
//! RGBA layout consumed by PipeWire.
//!
//! Both driver libraries are loaded at runtime.  A machine without NVIDIA's
//! `libcuda.so.1` and `libnvcuvid.so.1` simply reports this backend unavailable.

#![cfg(target_os = "linux")]
#![allow(non_snake_case)]

use crate::gpu_libs::{self, DynLib};
use std::collections::VecDeque;
use std::ffi::{c_int, c_uint, c_ulong, c_void};
use std::fmt;
use std::ptr;
use std::sync::OnceLock;

const CUDA_SUCCESS: c_int = 0;
const CUDA_VIDEO_CODEC_H264: c_int = 4;
const CUDA_VIDEO_CODEC_AV1: c_int = 11;
const CUDA_VIDEO_CHROMA_420: c_int = 1;
const CUDA_VIDEO_CHROMA_444: c_int = 3;
const CUDA_VIDEO_SURFACE_NV12: c_int = 0;
const CUDA_VIDEO_SURFACE_YUV444: c_int = 2;
const CUVID_PKT_TIMESTAMP: c_ulong = 0x02;
const CUVID_PKT_ENDOFPICTURE: c_ulong = 0x08;
const MAX_ENCODED_BYTES: usize = 4 * 1024 * 1024;
const MAX_DIMENSION: usize = 4096;
const MAX_PENDING_FRAMES: usize = 4;

type CUvideodecoder = *mut c_void;
type CUvideoparser = *mut c_void;
type CUdeviceptr = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NvdecCodec {
    H264,
    Av1,
}

impl NvdecCodec {
    fn raw(self) -> c_int {
        match self {
            Self::H264 => CUDA_VIDEO_CODEC_H264,
            Self::Av1 => CUDA_VIDEO_CODEC_AV1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Av1 => "AV1",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NvdecChroma {
    Cs420,
    Cs444,
}

impl NvdecChroma {
    fn raw(self) -> c_int {
        match self {
            Self::Cs420 => CUDA_VIDEO_CHROMA_420,
            Self::Cs444 => CUDA_VIDEO_CHROMA_444,
        }
    }

    fn surface_format(self) -> c_int {
        match self {
            Self::Cs420 => CUDA_VIDEO_SURFACE_NV12,
            Self::Cs444 => CUDA_VIDEO_SURFACE_YUV444,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Cs420 => "4:2:0",
            Self::Cs444 => "4:4:4",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NvdecCaps {
    pub(crate) min_width: u16,
    pub(crate) min_height: u16,
    pub(crate) max_width: u32,
    pub(crate) max_height: u32,
    pub(crate) engines: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NvdecError {
    Unavailable(String),
    InvalidInput(String),
    UnsupportedOutput(String),
    Driver(String),
}

impl fmt::Display for NvdecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(f, "NVDEC unavailable: {detail}"),
            Self::InvalidInput(detail) => write!(f, "invalid encoded video: {detail}"),
            Self::UnsupportedOutput(detail) => write!(f, "unsupported decoded video: {detail}"),
            Self::Driver(detail) => write!(f, "NVDEC driver error: {detail}"),
        }
    }
}

impl std::error::Error for NvdecError {}

// The layouts below are the public NVDEC 12.1 ABI from nv-codec-headers.  Only
// records that we populate or inspect are transcribed.  Codec-specific picture
// parameters remain opaque: libnvcuvid's parser creates them and the decoder
// consumes the same pointer synchronously.

#[repr(C)]
#[derive(Clone, Copy)]
struct CuRectI32 {
    left: c_int,
    top: c_int,
    right: c_int,
    bottom: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CuRectI16 {
    left: i16,
    top: i16,
    right: i16,
    bottom: i16,
}

#[repr(C)]
struct CuVideoFormat {
    codec: c_int,
    frame_rate: [c_uint; 2],
    progressive_sequence: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    min_num_decode_surfaces: u8,
    coded_width: c_uint,
    coded_height: c_uint,
    display_area: CuRectI32,
    chroma_format: c_int,
    bitrate: c_uint,
    display_aspect_ratio: [c_int; 2],
    // GCC/Clang allocate the four one-byte video-signal fields at offsets
    // 56..59.  Bit 3 of the first byte is video_full_range_flag.
    video_signal_flags: u8,
    color_primaries: u8,
    transfer_characteristics: u8,
    matrix_coefficients: u8,
    seqhdr_data_length: c_uint,
}

#[repr(C)]
struct CuvidPicParamsPrefix {
    PicWidthInMbs: c_int,
    FrameHeightInMbs: c_int,
    CurrPicIdx: c_int,
    field_pic_flag: c_int,
    bottom_field_flag: c_int,
    second_field: c_int,
    nBitstreamDataLen: c_uint,
    pBitstreamData: *const u8,
    nNumSlices: c_uint,
    pSliceDataOffsets: *const c_uint,
    ref_pic_flag: c_int,
    intra_pic_flag: c_int,
}

#[repr(C)]
struct CuvidParserDispInfo {
    picture_index: c_int,
    progressive_frame: c_int,
    top_field_first: c_int,
    repeat_first_field: c_int,
    timestamp: i64,
}

type SequenceCallback = unsafe extern "C" fn(*mut c_void, *mut CuVideoFormat) -> c_int;
type DecodeCallback = unsafe extern "C" fn(*mut c_void, *mut CuvidPicParamsPrefix) -> c_int;
type DisplayCallback = unsafe extern "C" fn(*mut c_void, *mut CuvidParserDispInfo) -> c_int;
type OperatingPointCallback = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
type SeiCallback = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;

#[repr(C)]
struct CuvidParserParams {
    CodecType: c_int,
    ulMaxNumDecodeSurfaces: c_uint,
    ulClockRate: c_uint,
    ulErrorThreshold: c_uint,
    ulMaxDisplayDelay: c_uint,
    av1_annexb_and_reserved: c_uint,
    uReserved1: [c_uint; 4],
    pUserData: *mut c_void,
    pfnSequenceCallback: Option<SequenceCallback>,
    pfnDecodePicture: Option<DecodeCallback>,
    pfnDisplayPicture: Option<DisplayCallback>,
    pfnGetOperatingPoint: Option<OperatingPointCallback>,
    pfnGetSEIMsg: Option<SeiCallback>,
    pvReserved2: [*mut c_void; 5],
    pExtVideoInfo: *mut c_void,
}

#[repr(C)]
struct CuvidSourceDataPacket {
    flags: c_ulong,
    payload_size: c_ulong,
    payload: *const u8,
    timestamp: i64,
}

#[repr(C)]
struct CuvidDecodeCaps {
    eCodecType: c_int,
    eChromaFormat: c_int,
    nBitDepthMinus8: c_uint,
    reserved1: [c_uint; 3],
    bIsSupported: u8,
    nNumNVDECs: u8,
    nOutputFormatMask: u16,
    nMaxWidth: c_uint,
    nMaxHeight: c_uint,
    nMaxMBCount: c_uint,
    nMinWidth: u16,
    nMinHeight: u16,
    bIsHistogramSupported: u8,
    nCounterBitDepth: u8,
    nMaxHistogramBins: u16,
    reserved3: [c_uint; 10],
}

impl CuvidDecodeCaps {
    fn query(codec: NvdecCodec, chroma: NvdecChroma) -> Self {
        Self {
            eCodecType: codec.raw(),
            eChromaFormat: chroma.raw(),
            nBitDepthMinus8: 0,
            reserved1: [0; 3],
            bIsSupported: 0,
            nNumNVDECs: 0,
            nOutputFormatMask: 0,
            nMaxWidth: 0,
            nMaxHeight: 0,
            nMaxMBCount: 0,
            nMinWidth: 0,
            nMinHeight: 0,
            bIsHistogramSupported: 0,
            nCounterBitDepth: 0,
            nMaxHistogramBins: 0,
            reserved3: [0; 10],
        }
    }
}

#[repr(C)]
struct CuvidDecodeCreateInfo {
    ulWidth: c_ulong,
    ulHeight: c_ulong,
    ulNumDecodeSurfaces: c_ulong,
    CodecType: c_int,
    ChromaFormat: c_int,
    ulCreationFlags: c_ulong,
    bitDepthMinus8: c_ulong,
    ulIntraDecodeOnly: c_ulong,
    ulMaxWidth: c_ulong,
    ulMaxHeight: c_ulong,
    Reserved1: c_ulong,
    display_area: CuRectI16,
    OutputFormat: c_int,
    DeinterlaceMode: c_int,
    ulTargetWidth: c_ulong,
    ulTargetHeight: c_ulong,
    ulNumOutputSurfaces: c_ulong,
    vidLock: *mut c_void,
    target_rect: CuRectI16,
    enableHistogram: c_ulong,
    Reserved2: [c_ulong; 4],
}

#[repr(C)]
struct CuvidProcParams {
    progressive_frame: c_int,
    second_field: c_int,
    top_field_first: c_int,
    unpaired_field: c_int,
    reserved_flags: c_uint,
    reserved_zero: c_uint,
    raw_input_dptr: u64,
    raw_input_pitch: c_uint,
    raw_input_format: c_uint,
    raw_output_dptr: u64,
    raw_output_pitch: c_uint,
    Reserved1: c_uint,
    output_stream: *mut c_void,
    Reserved: [c_uint; 46],
    histogram_dptr: *mut u64,
    Reserved2: [*mut c_void; 1],
}

impl CuvidProcParams {
    fn for_picture(display: &CuvidParserDispInfo) -> Self {
        Self {
            progressive_frame: display.progressive_frame,
            second_field: 0,
            top_field_first: display.top_field_first,
            unpaired_field: c_int::from(display.repeat_first_field < 0),
            reserved_flags: 0,
            reserved_zero: 0,
            raw_input_dptr: 0,
            raw_input_pitch: 0,
            raw_input_format: 0,
            raw_output_dptr: 0,
            raw_output_pitch: 0,
            Reserved1: 0,
            output_stream: ptr::null_mut(),
            Reserved: [0; 46],
            histogram_dptr: ptr::null_mut(),
            Reserved2: [ptr::null_mut()],
        }
    }
}

type CuvidGetDecoderCaps = unsafe extern "C" fn(*mut CuvidDecodeCaps) -> c_int;
type CuvidCreateDecoder =
    unsafe extern "C" fn(*mut CUvideodecoder, *mut CuvidDecodeCreateInfo) -> c_int;
type CuvidDestroyDecoder = unsafe extern "C" fn(CUvideodecoder) -> c_int;
type CuvidDecodePicture = unsafe extern "C" fn(CUvideodecoder, *mut CuvidPicParamsPrefix) -> c_int;
type CuvidMapVideoFrame64 = unsafe extern "C" fn(
    CUvideodecoder,
    c_int,
    *mut CUdeviceptr,
    *mut c_uint,
    *mut CuvidProcParams,
) -> c_int;
type CuvidUnmapVideoFrame64 = unsafe extern "C" fn(CUvideodecoder, CUdeviceptr) -> c_int;
type CuvidCreateVideoParser =
    unsafe extern "C" fn(*mut CUvideoparser, *mut CuvidParserParams) -> c_int;
type CuvidParseVideoData = unsafe extern "C" fn(CUvideoparser, *mut CuvidSourceDataPacket) -> c_int;
type CuvidDestroyVideoParser = unsafe extern "C" fn(CUvideoparser) -> c_int;
type CuMemcpyDtoH = unsafe extern "C" fn(*mut c_void, CUdeviceptr, usize) -> c_int;

struct NvdecFns {
    cuvidGetDecoderCaps: CuvidGetDecoderCaps,
    cuvidCreateDecoder: CuvidCreateDecoder,
    cuvidDestroyDecoder: CuvidDestroyDecoder,
    cuvidDecodePicture: CuvidDecodePicture,
    cuvidMapVideoFrame64: CuvidMapVideoFrame64,
    cuvidUnmapVideoFrame64: CuvidUnmapVideoFrame64,
    cuvidCreateVideoParser: CuvidCreateVideoParser,
    cuvidParseVideoData: CuvidParseVideoData,
    cuvidDestroyVideoParser: CuvidDestroyVideoParser,
    cuMemcpyDtoH_v2: CuMemcpyDtoH,
    _nvcuvid: DynLib,
    _cuda: DynLib,
}

impl NvdecFns {
    fn load() -> Result<Self, String> {
        let nvcuvid = DynLib::open(&["libnvcuvid.so.1", "libnvcuvid.so"])?;
        let cuda = DynLib::open(&["libcuda.so.1", "libcuda.so"])?;
        // SAFETY: every function type is transcribed from nv-codec-headers
        // 12.1 / cuda.h and both DynLib handles remain owned by this record.
        unsafe {
            Ok(Self {
                cuvidGetDecoderCaps: nvcuvid.sym("cuvidGetDecoderCaps")?,
                cuvidCreateDecoder: nvcuvid.sym("cuvidCreateDecoder")?,
                cuvidDestroyDecoder: nvcuvid.sym("cuvidDestroyDecoder")?,
                cuvidDecodePicture: nvcuvid.sym("cuvidDecodePicture")?,
                cuvidMapVideoFrame64: nvcuvid.sym("cuvidMapVideoFrame64")?,
                cuvidUnmapVideoFrame64: nvcuvid.sym("cuvidUnmapVideoFrame64")?,
                cuvidCreateVideoParser: nvcuvid.sym("cuvidCreateVideoParser")?,
                cuvidParseVideoData: nvcuvid.sym("cuvidParseVideoData")?,
                cuvidDestroyVideoParser: nvcuvid.sym("cuvidDestroyVideoParser")?,
                cuMemcpyDtoH_v2: cuda.sym("cuMemcpyDtoH_v2")?,
                _nvcuvid: nvcuvid,
                _cuda: cuda,
            })
        }
    }
}

fn nvdec_fns() -> Result<&'static NvdecFns, NvdecError> {
    static FNS: OnceLock<Result<NvdecFns, String>> = OnceLock::new();
    FNS.get_or_init(NvdecFns::load)
        .as_ref()
        .map_err(|error| NvdecError::Unavailable(error.clone()))
}

struct OwnedCudaContext {
    cuda: &'static gpu_libs::CudaFns,
    raw: gpu_libs::CUcontext,
}

impl OwnedCudaContext {
    fn new() -> Result<Self, NvdecError> {
        let cuda =
            gpu_libs::cuda().map_err(|error| NvdecError::Unavailable(format!("CUDA: {error}")))?;
        cuda_status(unsafe { (cuda.cuInit)(0) }, "cuInit")?;
        let ordinal = std::env::var("BLIT_CUDA_DEVICE")
            .ok()
            .and_then(|value| value.parse::<c_int>().ok())
            .unwrap_or(0);
        let mut device = 0;
        cuda_status(
            unsafe { (cuda.cuDeviceGet)(&mut device, ordinal) },
            &format!("cuDeviceGet({ordinal})"),
        )?;
        let mut raw = ptr::null_mut();
        cuda_status(
            unsafe { (cuda.cuCtxCreate_v2)(&mut raw, 0, device) },
            "cuCtxCreate_v2",
        )?;
        let mut popped = ptr::null_mut();
        if let Err(error) = cuda_status(
            unsafe { (cuda.cuCtxPopCurrent_v2)(&mut popped) },
            "cuCtxPopCurrent_v2 after create",
        ) {
            unsafe { (cuda.cuCtxDestroy_v2)(raw) };
            return Err(error);
        }
        if popped != raw {
            unsafe { (cuda.cuCtxDestroy_v2)(raw) };
            return Err(NvdecError::Driver(
                "CUDA popped a different context after creation".into(),
            ));
        }
        Ok(Self { cuda, raw })
    }

    fn push(&self) -> Result<CurrentCudaContext, NvdecError> {
        cuda_status(
            unsafe { (self.cuda.cuCtxPushCurrent_v2)(self.raw) },
            "cuCtxPushCurrent_v2",
        )?;
        Ok(CurrentCudaContext { cuda: self.cuda })
    }
}

impl Drop for OwnedCudaContext {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { (self.cuda.cuCtxDestroy_v2)(self.raw) };
        }
    }
}

struct CurrentCudaContext {
    cuda: &'static gpu_libs::CudaFns,
}

impl Drop for CurrentCudaContext {
    fn drop(&mut self) {
        let mut popped = ptr::null_mut();
        unsafe { (self.cuda.cuCtxPopCurrent_v2)(&mut popped) };
    }
}

fn cuda_status(status: c_int, operation: &str) -> Result<(), NvdecError> {
    if status == CUDA_SUCCESS {
        Ok(())
    } else {
        Err(NvdecError::Driver(format!(
            "{operation} failed with CUDA status {status}"
        )))
    }
}

fn query_caps(
    fns: &NvdecFns,
    codec: NvdecCodec,
    chroma: NvdecChroma,
) -> Result<(NvdecCaps, CuvidDecodeCaps), NvdecError> {
    let mut raw = CuvidDecodeCaps::query(codec, chroma);
    cuda_status(
        unsafe { (fns.cuvidGetDecoderCaps)(&mut raw) },
        "cuvidGetDecoderCaps",
    )?;
    let required_format = 1_u16 << chroma.surface_format();
    if raw.bIsSupported == 0 || raw.nOutputFormatMask & required_format == 0 {
        return Err(NvdecError::Unavailable(format!(
            "{} {} 8-bit output is not supported by this GPU",
            codec.label(),
            chroma.label()
        )));
    }
    Ok((
        NvdecCaps {
            min_width: raw.nMinWidth,
            min_height: raw.nMinHeight,
            max_width: raw.nMaxWidth,
            max_height: raw.nMaxHeight,
            engines: raw.nNumNVDECs,
        },
        raw,
    ))
}

#[cfg(test)]
pub(crate) fn probe(codec: NvdecCodec, chroma: NvdecChroma) -> Result<NvdecCaps, NvdecError> {
    let fns = nvdec_fns()?;
    let context = OwnedCudaContext::new()?;
    let _current = context.push()?;
    query_caps(fns, codec, chroma).map(|(caps, _)| caps)
}

struct Inner {
    fns: &'static NvdecFns,
    context: OwnedCudaContext,
    parser: CUvideoparser,
    decoder: CUvideodecoder,
    codec: NvdecCodec,
    chroma: NvdecChroma,
    width: usize,
    height: usize,
    decode_surfaces: c_ulong,
    full_range: bool,
    expect_intra: bool,
    saw_picture: bool,
    callback_error: Option<NvdecError>,
    frames: VecDeque<Vec<u8>>,
}

impl Inner {
    fn on_sequence(&mut self, format: &CuVideoFormat) -> Result<c_int, NvdecError> {
        if format.codec != self.codec.raw() {
            return Err(NvdecError::UnsupportedOutput(format!(
                "parser reported codec {}, expected {}",
                format.codec,
                self.codec.label()
            )));
        }
        if format.chroma_format != self.chroma.raw() {
            return Err(NvdecError::UnsupportedOutput(format!(
                "parser reported chroma {}, expected {}",
                format.chroma_format,
                self.chroma.label()
            )));
        }
        if format.bit_depth_luma_minus8 != 0 || format.bit_depth_chroma_minus8 != 0 {
            return Err(NvdecError::UnsupportedOutput(format!(
                "parser reported {}-bit luma / {}-bit chroma, expected 8-bit",
                format.bit_depth_luma_minus8.saturating_add(8),
                format.bit_depth_chroma_minus8.saturating_add(8)
            )));
        }
        if format.progressive_sequence == 0 {
            return Err(NvdecError::UnsupportedOutput(
                "interlaced camera video is not supported".into(),
            ));
        }
        let display_width = format
            .display_area
            .right
            .checked_sub(format.display_area.left)
            .and_then(|value| usize::try_from(value).ok());
        let display_height = format
            .display_area
            .bottom
            .checked_sub(format.display_area.top)
            .and_then(|value| usize::try_from(value).ok());
        if display_width != Some(self.width) || display_height != Some(self.height) {
            return Err(NvdecError::UnsupportedOutput(format!(
                "display area is {}x{}, expected {}x{}",
                display_width.unwrap_or(0),
                display_height.unwrap_or(0),
                self.width,
                self.height
            )));
        }
        if format.coded_width == 0 || format.coded_height == 0 {
            return Err(NvdecError::InvalidInput(
                "parser reported zero coded dimensions".into(),
            ));
        }
        self.full_range = format.video_signal_flags & 0x08 != 0;
        let surfaces = c_ulong::from(format.min_num_decode_surfaces.max(1));
        if !self.decoder.is_null() {
            if self.decode_surfaces == surfaces {
                return Ok(c_int::from(format.min_num_decode_surfaces.max(1)));
            }
            return Err(NvdecError::UnsupportedOutput(
                "mid-stream decoder configuration change".into(),
            ));
        }

        let (_, caps) = query_caps(self.fns, self.codec, self.chroma)?;
        validate_cap_dimensions(&caps, format.coded_width, format.coded_height)?;
        let mut create = CuvidDecodeCreateInfo {
            ulWidth: c_ulong::from(format.coded_width),
            ulHeight: c_ulong::from(format.coded_height),
            ulNumDecodeSurfaces: surfaces,
            CodecType: self.codec.raw(),
            ChromaFormat: self.chroma.raw(),
            ulCreationFlags: 0,
            bitDepthMinus8: 0,
            ulIntraDecodeOnly: 0,
            ulMaxWidth: c_ulong::from(format.coded_width),
            ulMaxHeight: c_ulong::from(format.coded_height),
            Reserved1: 0,
            display_area: CuRectI16 {
                left: checked_i16(format.display_area.left, "display left")?,
                top: checked_i16(format.display_area.top, "display top")?,
                right: checked_i16(format.display_area.right, "display right")?,
                bottom: checked_i16(format.display_area.bottom, "display bottom")?,
            },
            OutputFormat: self.chroma.surface_format(),
            DeinterlaceMode: 0,
            ulTargetWidth: self.width as c_ulong,
            ulTargetHeight: self.height as c_ulong,
            ulNumOutputSurfaces: 2,
            vidLock: ptr::null_mut(),
            target_rect: CuRectI16 {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            enableHistogram: 0,
            Reserved2: [0; 4],
        };
        let mut decoder = ptr::null_mut();
        cuda_status(
            unsafe { (self.fns.cuvidCreateDecoder)(&mut decoder, &mut create) },
            "cuvidCreateDecoder",
        )?;
        self.decoder = decoder;
        self.decode_surfaces = surfaces;
        Ok(c_int::from(format.min_num_decode_surfaces.max(1)))
    }

    fn on_decode(&mut self, picture: *mut CuvidPicParamsPrefix) -> Result<(), NvdecError> {
        if self.decoder.is_null() {
            return Err(NvdecError::Driver(
                "decode callback preceded sequence callback".into(),
            ));
        }
        if picture.is_null() {
            return Err(NvdecError::InvalidInput(
                "parser supplied a null picture".into(),
            ));
        }
        // SAFETY: libnvcuvid owns this record for the synchronous callback.
        let prefix = unsafe { &*picture };
        if self.expect_intra && prefix.intra_pic_flag == 0 {
            return Err(NvdecError::InvalidInput(
                "packet marked as a recovery point is not intra-coded".into(),
            ));
        }
        self.expect_intra = false;
        self.saw_picture = true;
        cuda_status(
            unsafe { (self.fns.cuvidDecodePicture)(self.decoder, picture) },
            "cuvidDecodePicture",
        )
    }

    fn on_display(&mut self, display: &CuvidParserDispInfo) -> Result<(), NvdecError> {
        if self.frames.len() >= MAX_PENDING_FRAMES {
            return Err(NvdecError::Driver(
                "parser produced too many pending display frames".into(),
            ));
        }
        if display.progressive_frame == 0 {
            return Err(NvdecError::UnsupportedOutput(
                "interlaced decoded frame".into(),
            ));
        }
        let mut proc = CuvidProcParams::for_picture(display);
        let mut device_ptr = 0_u64;
        let mut pitch = 0_u32;
        cuda_status(
            unsafe {
                (self.fns.cuvidMapVideoFrame64)(
                    self.decoder,
                    display.picture_index,
                    &mut device_ptr,
                    &mut pitch,
                    &mut proc,
                )
            },
            "cuvidMapVideoFrame64",
        )?;
        let mapped = MappedFrame {
            fns: self.fns,
            decoder: self.decoder,
            device_ptr,
        };
        let pitch = usize::try_from(pitch)
            .map_err(|_| NvdecError::Driver("mapped pitch overflow".into()))?;
        if pitch < self.width {
            return Err(NvdecError::Driver(format!(
                "mapped pitch {pitch} is shorter than width {}",
                self.width
            )));
        }
        let y = self.copy_plane(device_ptr, pitch, self.width, self.height)?;
        let rgba = match self.chroma {
            NvdecChroma::Cs420 => {
                let uv_base = plane_base(device_ptr, pitch, self.height, 1)?;
                let uv = self.copy_plane(uv_base, pitch, self.width, self.height / 2)?;
                convert_nv12_to_rgba(&y, &uv, self.width, self.height, self.full_range)
            }
            NvdecChroma::Cs444 => {
                let u_base = plane_base(device_ptr, pitch, self.height, 1)?;
                let v_base = plane_base(device_ptr, pitch, self.height, 2)?;
                let u = self.copy_plane(u_base, pitch, self.width, self.height)?;
                let v = self.copy_plane(v_base, pitch, self.width, self.height)?;
                convert_yuv444_to_rgba(&y, &u, &v, self.width, self.height, self.full_range)
            }
        };
        drop(mapped);
        self.frames.push_back(rgba);
        Ok(())
    }

    fn copy_plane(
        &self,
        base: CUdeviceptr,
        pitch: usize,
        width: usize,
        height: usize,
    ) -> Result<Vec<u8>, NvdecError> {
        let bytes = width
            .checked_mul(height)
            .ok_or_else(|| NvdecError::Driver("host plane size overflow".into()))?;
        let mut host = vec![0_u8; bytes];
        for row in 0..height {
            let source = base
                .checked_add(
                    u64::try_from(
                        row.checked_mul(pitch).ok_or_else(|| {
                            NvdecError::Driver("device row offset overflow".into())
                        })?,
                    )
                    .map_err(|_| NvdecError::Driver("device row offset overflow".into()))?,
                )
                .ok_or_else(|| NvdecError::Driver("device address overflow".into()))?;
            let destination = unsafe { host.as_mut_ptr().add(row * width) }.cast();
            cuda_status(
                unsafe { (self.fns.cuMemcpyDtoH_v2)(destination, source, width) },
                "cuMemcpyDtoH_v2",
            )?;
        }
        Ok(host)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Ok(_current) = self.context.push() {
            if !self.parser.is_null() {
                unsafe { (self.fns.cuvidDestroyVideoParser)(self.parser) };
                self.parser = ptr::null_mut();
            }
            if !self.decoder.is_null() {
                unsafe { (self.fns.cuvidDestroyDecoder)(self.decoder) };
                self.decoder = ptr::null_mut();
            }
        }
    }
}

struct MappedFrame {
    fns: &'static NvdecFns,
    decoder: CUvideodecoder,
    device_ptr: CUdeviceptr,
}

impl Drop for MappedFrame {
    fn drop(&mut self) {
        unsafe { (self.fns.cuvidUnmapVideoFrame64)(self.decoder, self.device_ptr) };
    }
}

/// Stateful direct NVDEC decoder.  It is owned by one camera worker thread and
/// may move with that worker, but calls must never be concurrent.
pub(crate) struct NvdecDecoder {
    inner: Box<Inner>,
    timestamp: i64,
    needs_keyframe: bool,
}

unsafe impl Send for NvdecDecoder {}

impl NvdecDecoder {
    pub(crate) fn new(
        codec: NvdecCodec,
        chroma: NvdecChroma,
        width: u16,
        height: u16,
    ) -> Result<Self, NvdecError> {
        validate_dimensions(chroma, usize::from(width), usize::from(height))?;
        let fns = nvdec_fns()?;
        let context = OwnedCudaContext::new()?;
        {
            let _current = context.push()?;
            let (_, raw_caps) = query_caps(fns, codec, chroma)?;
            validate_cap_dimensions(&raw_caps, c_uint::from(width), c_uint::from(height))?;
        }
        let mut result = Self {
            inner: Box::new(Inner {
                fns,
                context,
                parser: ptr::null_mut(),
                decoder: ptr::null_mut(),
                codec,
                chroma,
                width: usize::from(width),
                height: usize::from(height),
                decode_surfaces: 0,
                full_range: false,
                expect_intra: false,
                saw_picture: false,
                callback_error: None,
                frames: VecDeque::new(),
            }),
            timestamp: 0,
            needs_keyframe: true,
        };
        result.create_parser()?;
        Ok(result)
    }

    fn create_parser(&mut self) -> Result<(), NvdecError> {
        let _current = self.inner.context.push()?;
        let user_data = (&mut *self.inner as *mut Inner).cast();
        let mut params = CuvidParserParams {
            CodecType: self.inner.codec.raw(),
            ulMaxNumDecodeSurfaces: 1,
            ulClockRate: 10_000_000,
            ulErrorThreshold: 0,
            ulMaxDisplayDelay: 0,
            // AV1 camera chunks use low-overhead OBUs, not AV1 Annex-B.
            av1_annexb_and_reserved: 0,
            uReserved1: [0; 4],
            pUserData: user_data,
            pfnSequenceCallback: Some(sequence_callback),
            pfnDecodePicture: Some(decode_callback),
            pfnDisplayPicture: Some(display_callback),
            pfnGetOperatingPoint: None,
            pfnGetSEIMsg: None,
            pvReserved2: [ptr::null_mut(); 5],
            pExtVideoInfo: ptr::null_mut(),
        };
        let mut parser = ptr::null_mut();
        cuda_status(
            unsafe { (self.inner.fns.cuvidCreateVideoParser)(&mut parser, &mut params) },
            "cuvidCreateVideoParser",
        )?;
        self.inner.parser = parser;
        Ok(())
    }

    /// Drop all parser, DPB, and display state after a transport gap.  The
    /// next call to [`Self::decode`] must be marked as a recovery keyframe.
    pub(crate) fn reset(&mut self) -> Result<(), NvdecError> {
        {
            let _current = self.inner.context.push()?;
            if !self.inner.parser.is_null() {
                cuda_status(
                    unsafe { (self.inner.fns.cuvidDestroyVideoParser)(self.inner.parser) },
                    "cuvidDestroyVideoParser",
                )?;
                self.inner.parser = ptr::null_mut();
            }
            if !self.inner.decoder.is_null() {
                cuda_status(
                    unsafe { (self.inner.fns.cuvidDestroyDecoder)(self.inner.decoder) },
                    "cuvidDestroyDecoder",
                )?;
                self.inner.decoder = ptr::null_mut();
            }
        }
        self.inner.decode_surfaces = 0;
        self.inner.callback_error = None;
        self.inner.frames.clear();
        self.inner.expect_intra = false;
        self.inner.saw_picture = false;
        self.timestamp = 0;
        self.needs_keyframe = true;
        self.create_parser()
    }

    /// Decode one complete H.264 Annex-B access unit or AV1 low-overhead
    /// temporal unit and return its displayable RGBA frame.
    pub(crate) fn decode(&mut self, encoded: &[u8], keyframe: bool) -> Result<Vec<u8>, NvdecError> {
        if encoded.is_empty() {
            return Err(NvdecError::InvalidInput("empty packet".into()));
        }
        if encoded.len() > MAX_ENCODED_BYTES {
            return Err(NvdecError::InvalidInput(format!(
                "packet is {} bytes (maximum {MAX_ENCODED_BYTES})",
                encoded.len()
            )));
        }
        if self.needs_keyframe && !keyframe {
            return Err(NvdecError::InvalidInput(
                "decoder needs a recovery keyframe".into(),
            ));
        }
        self.inner.callback_error = None;
        self.inner.expect_intra = keyframe;
        self.inner.saw_picture = false;
        let _current = self.inner.context.push()?;
        let mut packet = CuvidSourceDataPacket {
            flags: CUVID_PKT_TIMESTAMP | CUVID_PKT_ENDOFPICTURE,
            payload_size: encoded.len() as c_ulong,
            payload: encoded.as_ptr(),
            timestamp: self.timestamp,
        };
        let status =
            unsafe { (self.inner.fns.cuvidParseVideoData)(self.inner.parser, &mut packet) };
        self.timestamp = self.timestamp.saturating_add(1);
        if let Some(error) = self.inner.callback_error.take() {
            return Err(error);
        }
        cuda_status(status, "cuvidParseVideoData")?;
        if keyframe && !self.inner.saw_picture {
            return Err(NvdecError::InvalidInput(
                "recovery packet contained no decodable picture".into(),
            ));
        }
        let frame = self
            .inner
            .frames
            .pop_front()
            .ok_or_else(|| NvdecError::Driver("packet produced no displayable frame".into()))?;
        self.needs_keyframe = false;
        Ok(frame)
    }
}

unsafe extern "C" fn sequence_callback(
    user_data: *mut c_void,
    format: *mut CuVideoFormat,
) -> c_int {
    callback_result(user_data, || {
        if format.is_null() {
            return Err(NvdecError::Driver(
                "sequence callback received null format".into(),
            ));
        }
        // SAFETY: libnvcuvid owns the format for this synchronous callback.
        unsafe { callback_inner(user_data)?.on_sequence(&*format) }
    })
}

unsafe extern "C" fn decode_callback(
    user_data: *mut c_void,
    picture: *mut CuvidPicParamsPrefix,
) -> c_int {
    callback_result(user_data, || {
        // SAFETY: the parser invokes callbacks with the pUserData supplied at
        // creation; the boxed Inner remains at that address for its lifetime.
        unsafe { callback_inner(user_data)?.on_decode(picture) }?;
        Ok(1)
    })
}

unsafe extern "C" fn display_callback(
    user_data: *mut c_void,
    display: *mut CuvidParserDispInfo,
) -> c_int {
    callback_result(user_data, || {
        if display.is_null() {
            return Ok(1);
        }
        // SAFETY: libnvcuvid owns the display record for this callback.
        unsafe { callback_inner(user_data)?.on_display(&*display) }?;
        Ok(1)
    })
}

unsafe fn callback_inner<'a>(user_data: *mut c_void) -> Result<&'a mut Inner, NvdecError> {
    if user_data.is_null() {
        return Err(NvdecError::Driver("null parser callback state".into()));
    }
    // SAFETY: caller is one of the synchronous parser callbacks and the
    // pointer was obtained from a live Box<Inner>.
    Ok(unsafe { &mut *user_data.cast::<Inner>() })
}

fn callback_result<F>(user_data: *mut c_void, callback: F) -> c_int
where
    F: FnOnce() -> Result<c_int, NvdecError>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)) {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            store_callback_error(user_data, error);
            0
        }
        Err(_) => {
            store_callback_error(
                user_data,
                NvdecError::Driver("panic in NVDEC parser callback".into()),
            );
            0
        }
    }
}

fn store_callback_error(user_data: *mut c_void, error: NvdecError) {
    if !user_data.is_null() {
        // SAFETY: this runs synchronously inside a parser callback with the
        // Box<Inner> pointer supplied when that parser was created.
        unsafe { (*user_data.cast::<Inner>()).callback_error = Some(error) };
    }
}

fn checked_i16(value: c_int, name: &str) -> Result<i16, NvdecError> {
    i16::try_from(value)
        .map_err(|_| NvdecError::UnsupportedOutput(format!("{name} exceeds NVDEC range")))
}

fn validate_dimensions(chroma: NvdecChroma, width: usize, height: usize) -> Result<(), NvdecError> {
    if width == 0 || height == 0 {
        return Err(NvdecError::UnsupportedOutput(
            "dimensions must be non-zero".into(),
        ));
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(NvdecError::UnsupportedOutput(format!(
            "dimensions {width}x{height} exceed {MAX_DIMENSION}x{MAX_DIMENSION}"
        )));
    }
    if chroma == NvdecChroma::Cs420 && (!width.is_multiple_of(2) || !height.is_multiple_of(2)) {
        return Err(NvdecError::UnsupportedOutput(
            "4:2:0 dimensions must be even".into(),
        ));
    }
    width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| NvdecError::UnsupportedOutput("RGBA size overflow".into()))?;
    Ok(())
}

fn validate_cap_dimensions(
    caps: &CuvidDecodeCaps,
    width: c_uint,
    height: c_uint,
) -> Result<(), NvdecError> {
    if width < c_uint::from(caps.nMinWidth)
        || height < c_uint::from(caps.nMinHeight)
        || width > caps.nMaxWidth
        || height > caps.nMaxHeight
    {
        return Err(NvdecError::Unavailable(format!(
            "{width}x{height} is outside NVDEC's {}x{}-{}x{} range",
            caps.nMinWidth, caps.nMinHeight, caps.nMaxWidth, caps.nMaxHeight
        )));
    }
    let macroblocks = width.div_ceil(16).saturating_mul(height.div_ceil(16));
    if caps.nMaxMBCount != 0 && macroblocks > caps.nMaxMBCount {
        return Err(NvdecError::Unavailable(format!(
            "{width}x{height} needs {macroblocks} macroblocks; NVDEC permits {}",
            caps.nMaxMBCount
        )));
    }
    Ok(())
}

fn plane_base(
    base: CUdeviceptr,
    pitch: usize,
    height: usize,
    plane: usize,
) -> Result<CUdeviceptr, NvdecError> {
    let offset = pitch
        .checked_mul(height)
        .and_then(|bytes| bytes.checked_mul(plane))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or_else(|| NvdecError::Driver("mapped plane offset overflow".into()))?;
    base.checked_add(offset)
        .ok_or_else(|| NvdecError::Driver("mapped plane address overflow".into()))
}

fn convert_nv12_to_rgba(
    y_plane: &[u8],
    uv_plane: &[u8],
    width: usize,
    height: usize,
    full_range: bool,
) -> Vec<u8> {
    let mut rgba = vec![0_u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let yv = y_plane[y * width + x];
            let chroma = (y / 2) * width + (x / 2) * 2;
            let (r, g, b) = yuv709_to_rgb(yv, uv_plane[chroma], uv_plane[chroma + 1], full_range);
            let output = (y * width + x) * 4;
            rgba[output..output + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    rgba
}

fn convert_yuv444_to_rgba(
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    width: usize,
    height: usize,
    full_range: bool,
) -> Vec<u8> {
    let mut rgba = vec![0_u8; width * height * 4];
    for index in 0..width * height {
        let (r, g, b) = yuv709_to_rgb(y_plane[index], u_plane[index], v_plane[index], full_range);
        rgba[index * 4..index * 4 + 4].copy_from_slice(&[r, g, b, 255]);
    }
    rgba
}

fn yuv709_to_rgb(y: u8, u: u8, v: u8, full_range: bool) -> (u8, u8, u8) {
    let y = i32::from(y);
    let u = i32::from(u) - 128;
    let v = i32::from(v) - 128;
    let (r, g, b) = if full_range {
        (
            y + ((403 * v + 128) >> 8),
            y - ((48 * u + 120 * v + 128) >> 8),
            y + ((475 * u + 128) >> 8),
        )
    } else {
        let y = (y - 16).max(0);
        (
            (298 * y + 459 * v + 128) >> 8,
            (298 * y - 55 * u - 136 * v + 128) >> 8,
            (298 * y + 541 * u + 128) >> 8,
        )
    };
    (
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvcodec_12_1_public_layouts_match() {
        assert_eq!(std::mem::size_of::<CuVideoFormat>(), 64);
        assert_eq!(std::mem::size_of::<CuvidPicParamsPrefix>(), 64);
        assert_eq!(std::mem::size_of::<CuvidParserDispInfo>(), 24);
        assert_eq!(std::mem::size_of::<CuvidParserParams>(), 136);
        assert_eq!(std::mem::size_of::<CuvidSourceDataPacket>(), 32);
        assert_eq!(std::mem::size_of::<CuvidDecodeCaps>(), 88);
        assert_eq!(std::mem::size_of::<CuvidDecodeCreateInfo>(), 176);
        assert_eq!(std::mem::size_of::<CuvidProcParams>(), 264);
        assert_eq!(std::mem::offset_of!(CuVideoFormat, video_signal_flags), 56);
        assert_eq!(std::mem::offset_of!(CuvidParserParams, pUserData), 40);
        assert_eq!(
            std::mem::offset_of!(CuvidDecodeCreateInfo, OutputFormat),
            88
        );
        assert_eq!(std::mem::offset_of!(CuvidProcParams, output_stream), 56);
    }

    #[test]
    fn exact_chroma_dimension_validation() {
        assert!(validate_dimensions(NvdecChroma::Cs420, 256, 256).is_ok());
        assert!(validate_dimensions(NvdecChroma::Cs420, 255, 256).is_err());
        assert!(validate_dimensions(NvdecChroma::Cs444, 255, 255).is_ok());
    }

    #[test]
    fn yuv_conversions_are_opaque_rgba() {
        let nv12 = convert_nv12_to_rgba(&[16; 4], &[128, 128], 2, 2, false);
        assert_eq!(nv12, [0, 0, 0, 255].repeat(4));
        let yuv444 = convert_yuv444_to_rgba(&[235], &[128], &[128], 1, 1, false);
        assert_eq!(yuv444, vec![255, 255, 255, 255]);
    }

    /// Real-driver coverage is opt-in because most CI hosts have no NVIDIA
    /// device.  The fixture must be a single 256x256 recovery access/temporal
    /// unit in the matching wire format.
    #[test]
    fn opt_in_real_nvdec_decode() {
        let cases = [
            (
                "BLIT_TEST_DIRECT_NVDEC_H264_420",
                NvdecCodec::H264,
                NvdecChroma::Cs420,
            ),
            (
                "BLIT_TEST_DIRECT_NVDEC_AV1_420",
                NvdecCodec::Av1,
                NvdecChroma::Cs420,
            ),
            (
                "BLIT_TEST_DIRECT_NVDEC_H264_444",
                NvdecCodec::H264,
                NvdecChroma::Cs444,
            ),
            (
                "BLIT_TEST_DIRECT_NVDEC_AV1_444",
                NvdecCodec::Av1,
                NvdecChroma::Cs444,
            ),
        ];
        let mut tested = 0;
        for (variable, codec, chroma) in cases {
            let Some(path) = std::env::var_os(variable) else {
                continue;
            };
            tested += 1;
            let encoded = std::fs::read(path).expect("read direct NVDEC fixture");
            let mut decoder =
                NvdecDecoder::new(codec, chroma, 256, 256).expect("create direct NVDEC decoder");
            let rgba = decoder.decode(&encoded, true).expect("direct NVDEC decode");
            assert_eq!(rgba.len(), 256 * 256 * 4);
            decoder.reset().expect("reset direct NVDEC decoder");
            let recovered = decoder
                .decode(&encoded, true)
                .expect("direct NVDEC decode after reset");
            assert_eq!(recovered.len(), 256 * 256 * 4);
        }
        if tested == 0 {
            eprintln!("direct NVDEC fixture variables not set; skipping real-driver assertions");
        }
    }

    #[test]
    fn opt_in_real_nvdec_rejects_unsupported_444() {
        if std::env::var_os("BLIT_TEST_DIRECT_NVDEC_EXPECT_444_UNAVAILABLE").is_none() {
            eprintln!("direct NVDEC 4:4:4 capability assertion not requested");
            return;
        }
        for codec in [NvdecCodec::H264, NvdecCodec::Av1] {
            assert!(
                matches!(
                    probe(codec, NvdecChroma::Cs444),
                    Err(NvdecError::Unavailable(_))
                ),
                "this test host unexpectedly supports direct NVDEC {} 4:4:4",
                codec.label()
            );
        }
    }
}
