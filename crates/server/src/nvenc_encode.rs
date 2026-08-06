//! Direct NVENC encoder — no ffmpeg dependency.
//!
//! Uses the NVIDIA Video Codec SDK via `dlopen("libnvidia-encode.so")`.
//! The CUDA context is created via `dlopen("libcuda.so")`.
//!
//! The encoder accepts BGRA input directly (`NV_ENC_BUFFER_FORMAT_ARGB`),
//! so no CPU-side colorspace conversion is needed.  NVENC handles the
//! BGRA→YUV conversion internally on the GPU.

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

use crate::gpu_libs;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;

// ---------------------------------------------------------------------------
// NVENC API constants
// ---------------------------------------------------------------------------

const NV_ENC_SUCCESS: u32 = 0;
/// `NVENCSTATUS` ordinal, counted from nvEncodeAPI.h — 10 is
/// `NV_ENC_ERR_OUT_OF_MEMORY`, which this was set to.  The encode paths
/// treat this status as "no output for this frame yet", so an encoder that
/// ran out of memory reported nothing at all, while a genuine request for
/// more input was raised as a hard failure.
const NV_ENC_ERR_NEED_MORE_INPUT: u32 = 17;

// API version whose struct layouts we target.  Must match a version the
// driver is backward-compatible with.  We use 12.1 — matching the widely
// deployed nv-codec-headers (used by ffmpeg/gstreamer), so this is the
// ABI version most drivers are tested against.
const NVENCAPI_MAJOR_VERSION: u32 = 12;
const NVENCAPI_MINOR_VERSION: u32 = 1;

/// NVENCAPI_VERSION = major | (minor << 24)
const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);

/// NVENCAPI_STRUCT_VERSION(v) = NVENCAPI_VERSION | (v << 16) | (0x7 << 28)
const fn nvencapi_struct_version(typ_ver: u32) -> u32 {
    NVENCAPI_VERSION | (typ_ver << 16) | (0x7 << 28)
}

// Struct version tags (nv-codec-headers 12.1.14.0).
// Some structs set bit 31 to signal extended feature support.
const NV_ENC_OPEN_ENCODE_SESSION_EX_VER: u32 = nvencapi_struct_version(1);
const NV_ENC_INITIALIZE_PARAMS_VER: u32 = nvencapi_struct_version(6) | (1 << 31);
const NV_ENC_PRESET_CONFIG_VER: u32 = nvencapi_struct_version(4) | (1 << 31);
const NV_ENC_CONFIG_VER: u32 = nvencapi_struct_version(8) | (1 << 31);
const NV_ENC_CREATE_INPUT_BUFFER_VER: u32 = nvencapi_struct_version(1);
const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = nvencapi_struct_version(1);
const NV_ENC_PIC_PARAMS_VER: u32 = nvencapi_struct_version(6) | (1 << 31);
const NV_ENC_LOCK_BITSTREAM_VER: u32 = nvencapi_struct_version(1) | (1 << 31);
const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = nvencapi_struct_version(1) | (1 << 31);

// Buffer formats (from nv-codec-headers 12.1)
const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x00000001;
const NV_ENC_BUFFER_FORMAT_ARGB: u32 = 0x01000000; // B8G8R8A8 in memory (DRM ARGB8888)
const NV_ENC_BUFFER_FORMAT_ABGR: u32 = 0x10000000; // R8G8B8A8 in memory (DRM ABGR8888)

// Encoder capability query.  The values are ordinals into `NV_ENC_CAPS`
// (nvEncodeAPI.h) — count the enum, don't guess: `SUPPORT_YUV444_ENCODE`
// was long spelled 15 here, which is `SEPARATE_COLOUR_PLANE`.  It happened
// to answer the same way on the GPUs we had, so nothing caught it.
const NV_ENC_CAPS_PARAM_VER: u32 = nvencapi_struct_version(1);
const NV_ENC_CAPS_PARAM_SIZE: usize = 256;
const NV_ENC_CAPS_WIDTH_MAX: u32 = 16;
const NV_ENC_CAPS_HEIGHT_MAX: u32 = 17;
const NV_ENC_CAPS_SUPPORT_YUV444_ENCODE: u32 = 33;
const NV_ENC_CAPS_WIDTH_MIN: u32 = 45;
const NV_ENC_CAPS_HEIGHT_MIN: u32 = 46;

// Resource types for nvEncRegisterResource
const NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR: u32 = 0x01;
const NV_ENC_REGISTER_RESOURCE_VER: u32 = nvencapi_struct_version(4);
const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = nvencapi_struct_version(4);

// NV_ENC_REGISTER_RESOURCE struct size (must cover all fields + reserved[245] + reserved2[61])
const NVENC_REGISTER_RESOURCE_SIZE: usize = 2048;
// NV_ENC_MAP_INPUT_RESOURCE struct size (includes reserved fields)
const NVENC_MAP_INPUT_RESOURCE_SIZE: usize = 2048;

// Codec GUIDs (H.264 and AV1)
const NV_ENC_CODEC_H264_GUID: NvGuid = NvGuid(
    0x6BC82762,
    0x4E63,
    0x4CA4,
    [0xAA, 0x85, 0x1E, 0x50, 0xF3, 0x21, 0xF6, 0xBF],
);
const NV_ENC_CODEC_AV1_GUID: NvGuid = NvGuid(
    0x0A352289,
    0x0AA7,
    0x4759,
    [0x86, 0x2D, 0x5D, 0x15, 0xCD, 0x16, 0xD2, 0x54],
);

// Preset GUIDs P1 (fastest) … P7 (slowest), from nvEncodeAPI.h.
const NV_ENC_PRESET_GUIDS: [NvGuid; 7] = [
    NvGuid(
        0xFC0A8D3E,
        0x45F8,
        0x4CF8,
        [0x80, 0xC7, 0x29, 0x88, 0x71, 0x59, 0x0E, 0xBF],
    ),
    NvGuid(
        0xF581CFB8,
        0x88D6,
        0x4381,
        [0x93, 0xF0, 0xDF, 0x13, 0xF9, 0xC2, 0x7D, 0xAB],
    ),
    NvGuid(
        0x36850110,
        0x3A07,
        0x441F,
        [0x94, 0xD5, 0x36, 0x70, 0x63, 0x1F, 0x91, 0xF6],
    ),
    NvGuid(
        0x90A7B826,
        0xDF06,
        0x4862,
        [0xB9, 0xD2, 0xCD, 0x6D, 0x73, 0xA0, 0x86, 0x81],
    ),
    NvGuid(
        0x21C6E6B4,
        0x297A,
        0x4CBA,
        [0x99, 0x8F, 0xB6, 0xCB, 0xDE, 0x72, 0xAD, 0xE3],
    ),
    NvGuid(
        0x8E75C279,
        0x6299,
        0x4AB6,
        [0x83, 0x02, 0x0B, 0x21, 0x5A, 0x33, 0x5C, 0xF5],
    ),
    NvGuid(
        0x84848C12,
        0x6F71,
        0x4C13,
        [0x93, 0x1B, 0x53, 0xE2, 0x83, 0xF5, 0x79, 0x74],
    ),
];

/// `preset` is 1 (P1, fastest) … 7 (P7, slowest); out-of-range clamps to P1.
fn preset_guid(preset: u8) -> NvGuid {
    NV_ENC_PRESET_GUIDS[(preset.clamp(1, 7) - 1) as usize]
}

// H.264 profile GUID — High 4:4:4 Predictive (from nvEncodeAPI.h)
const NV_ENC_H264_PROFILE_HIGH_444_GUID: NvGuid = NvGuid(
    0x7AC663CB,
    0xA598,
    0x49D8,
    [0xB1, 0x0E, 0x10, 0x38, 0x6E, 0x79, 0xCB, 0x1B],
);

// Tuning info (NV_ENC_TUNING_INFO enum from nv-codec-headers 12.1)
// 1 = HIGH_QUALITY, 2 = LOW_LATENCY, 3 = ULTRA_LOW_LATENCY
const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;

// Picture types (from nvEncodeAPI.h NV_ENC_PIC_TYPE / NV_ENC_PIC_FLAG)
const NV_ENC_PIC_TYPE_I: u32 = 2;
const NV_ENC_PIC_TYPE_IDR: u32 = 3;
const NV_ENC_PIC_FLAGS_FORCEIDR: u32 = 2;

// Rate control modes
const NV_ENC_PARAMS_RC_CONSTQP: u32 = 0;

// ---------------------------------------------------------------------------
// NVENC API types
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct NvGuid(u32, u16, u16, [u8; 8]);

/// The NVENC function pointer table.  We only declare the functions we use.
/// The full table has ~30 entries but we only need ~10.
///
/// The struct layout must match NV_ENCODE_API_FUNCTION_LIST exactly — unused
/// entries are `*const c_void` placeholders.
#[repr(C)]
struct NvEncFunctionList {
    version: u32,
    _reserved: u32,
    nvEncOpenEncodeSession: *const c_void,
    nvEncGetEncodeGUIDCount: *const c_void,
    // Order matters: the driver fills this table positionally, so a field
    // holds whichever entry the SDK puts at that index regardless of what we
    // named it.  The profile-GUID pair precedes nvEncGetEncodeGUIDs in
    // nvEncodeAPI.h — see NV_ENCODE_API_FUNCTION_LIST.
    nvEncGetEncodeProfileGUIDCount: *const c_void,
    nvEncGetEncodeProfileGUIDs: *const c_void,
    nvEncGetEncodeGUIDs: *const c_void,
    nvEncGetInputFormatCount: *const c_void,
    nvEncGetInputFormats: *const c_void,
    nvEncGetEncodeCaps: unsafe extern "C" fn(
        encoder: *mut c_void,
        encode_guid: NvGuid,
        caps_param: *mut c_void,
        caps_val: *mut i32,
    ) -> u32,
    nvEncGetEncodePresetCount: *const c_void,
    nvEncGetEncodePresetGUIDs: *const c_void,
    nvEncGetEncodePresetConfig: *const c_void,
    nvEncInitializeEncoder: unsafe extern "C" fn(encoder: *mut c_void, params: *mut c_void) -> u32,
    nvEncCreateInputBuffer: unsafe extern "C" fn(encoder: *mut c_void, params: *mut c_void) -> u32,
    nvEncDestroyInputBuffer: unsafe extern "C" fn(encoder: *mut c_void, buffer: *mut c_void) -> u32,
    nvEncCreateBitstreamBuffer:
        unsafe extern "C" fn(encoder: *mut c_void, params: *mut c_void) -> u32,
    nvEncDestroyBitstreamBuffer:
        unsafe extern "C" fn(encoder: *mut c_void, buffer: *mut c_void) -> u32,
    nvEncEncodePicture: unsafe extern "C" fn(encoder: *mut c_void, params: *mut c_void) -> u32,
    nvEncLockBitstream: unsafe extern "C" fn(encoder: *mut c_void, params: *mut c_void) -> u32,
    nvEncUnlockBitstream: unsafe extern "C" fn(encoder: *mut c_void, buffer: *mut c_void) -> u32,
    nvEncLockInputBuffer: unsafe extern "C" fn(encoder: *mut c_void, params: *mut c_void) -> u32,
    nvEncUnlockInputBuffer: unsafe extern "C" fn(encoder: *mut c_void, buffer: *mut c_void) -> u32,
    nvEncGetEncodeStats: *const c_void,
    nvEncGetSequenceParams: *const c_void,
    nvEncRegisterAsyncEvent: *const c_void,
    nvEncUnregisterAsyncEvent: *const c_void,
    nvEncMapInputResource: unsafe extern "C" fn(encoder: *mut c_void, params: *mut c_void) -> u32,
    nvEncUnmapInputResource:
        unsafe extern "C" fn(encoder: *mut c_void, resource: *mut c_void) -> u32,
    nvEncDestroyEncoder: unsafe extern "C" fn(encoder: *mut c_void) -> u32,
    nvEncInvalidateRefFrames: *const c_void,
    nvEncOpenEncodeSessionEx:
        unsafe extern "C" fn(params: *mut c_void, encoder: *mut *mut c_void) -> u32,
    nvEncRegisterResource: unsafe extern "C" fn(encoder: *mut c_void, params: *mut c_void) -> u32,
    nvEncUnregisterResource:
        unsafe extern "C" fn(encoder: *mut c_void, resource: *mut c_void) -> u32,
    nvEncReconfigureEncoder: unsafe extern "C" fn(encoder: *mut c_void, params: *mut c_void) -> u32,
    _reserved1: *const c_void,
    nvEncCreateMVBuffer: *const c_void,
    nvEncDestroyMVBuffer: *const c_void,
    nvEncRunMotionEstimationOnly: *const c_void,
    nvEncGetLastErrorString: *const c_void,
    nvEncSetIOCudaStreams: *const c_void,
    nvEncGetEncodePresetConfigEx: unsafe extern "C" fn(
        encoder: *mut c_void,
        encode_guid: NvGuid,
        preset_guid: NvGuid,
        tuning_info: u32,
        preset_config: *mut c_void,
    ) -> u32,
    nvEncGetSequenceParamEx: *const c_void,
    nvEncRestoreEncoderState: *const c_void,
    nvEncLookaheadPicture: *const c_void,
    // NV_ENCODE_API_FUNCTION_LIST::reserved2, which the SDK declares as
    // `void* reserved2[275]` and documents as "[in]: Reserved and must be set
    // to NULL".  It is the caller's job to supply that storage: the driver is
    // entitled to read it, and a shorter struct would have it reading our
    // stack.  Sizing it exactly as the header does also leaves room for
    // entries a future SDK appends, which is what the padding here was
    // originally for.
    reserved2: [*const c_void; 275],
}

// NvEncodeAPICreateInstance fills this table positionally from the driver's
// own layout, so an entry declared in the wrong slot silently aliases a
// different function — a mistake that surfaces as an unrelated call
// misbehaving at runtime, not as a load error.  Pin the offsets against
// nv-codec-headers 12.1 (taken from `offsetof`, not counted by hand) so that
// reordering or dropping an entry fails to compile instead.
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(offset_of!(NvEncFunctionList, nvEncGetEncodeGUIDs) == 40);
    assert!(offset_of!(NvEncFunctionList, nvEncGetEncodeCaps) == 64);
    assert!(offset_of!(NvEncFunctionList, nvEncInitializeEncoder) == 96);
    assert!(offset_of!(NvEncFunctionList, nvEncEncodePicture) == 136);
    assert!(offset_of!(NvEncFunctionList, nvEncLockBitstream) == 144);
    assert!(offset_of!(NvEncFunctionList, nvEncMapInputResource) == 208);
    assert!(offset_of!(NvEncFunctionList, nvEncDestroyEncoder) == 224);
    assert!(offset_of!(NvEncFunctionList, nvEncOpenEncodeSessionEx) == 240);
    assert!(offset_of!(NvEncFunctionList, nvEncRegisterResource) == 248);
    assert!(offset_of!(NvEncFunctionList, nvEncReconfigureEncoder) == 264);
    assert!(offset_of!(NvEncFunctionList, nvEncGetLastErrorString) == 304);
    assert!(offset_of!(NvEncFunctionList, nvEncGetEncodePresetConfigEx) == 320);
    assert!(offset_of!(NvEncFunctionList, nvEncRestoreEncoderState) == 336);
    assert!(offset_of!(NvEncFunctionList, nvEncLookaheadPicture) == 344);
    assert!(offset_of!(NvEncFunctionList, reserved2) == 352);
    assert!(size_of::<NvEncFunctionList>() == 2552);
};

// SAFETY: NvEncFunctionList is a C function-pointer table loaded once via
// dlopen.  The raw `*const c_void` fields are either unused placeholders or
// function pointers that are safe to share across threads (they point into
// read-only driver code).  The table is never mutated after initialization.
unsafe impl Send for NvEncFunctionList {}
unsafe impl Sync for NvEncFunctionList {}

// ---------------------------------------------------------------------------
// NVENC structs — opaque byte arrays sized to match nv-codec-headers 12.1.
// Fields are accessed at verified offsets (like vaapi_encode.rs) rather than
// fragile #[repr(C)] struct translation.
// ---------------------------------------------------------------------------

// Sizes from nv-codec-headers 12.1.14.0 (verified via sizeof/offsetof).
const NVENC_OPEN_ENCODE_SESSION_EX_SIZE: usize = 1552;
const NVENC_CONFIG_SIZE: usize = 3584;
const NVENC_PRESET_CONFIG_SIZE: usize = 5128;
const NVENC_INITIALIZE_PARAMS_SIZE: usize = 1808;
// NV_ENC_RECONFIGURE_PARAMS: u32 version, 4 bytes of alignment padding, an
// embedded NV_ENC_INITIALIZE_PARAMS, then the resetEncoder/forceIDR bitfield.
const NVENC_RECONFIGURE_PARAMS_SIZE: usize = 1824;
const NVENC_RECONFIGURE_INIT_PARAMS_OFFSET: usize = 8;
const NVENC_RECONFIGURE_FLAGS_OFFSET: usize = 1816;
const NVENC_CREATE_INPUT_BUFFER_SIZE: usize = 776;
const NVENC_CREATE_BITSTREAM_BUFFER_SIZE: usize = 776;
const NVENC_PIC_PARAMS_SIZE: usize = 3360;
const NVENC_LOCK_BITSTREAM_SIZE: usize = 1552;
/// `NV_ENC_CONFIG.encodeCodecConfig.h264Config.chromaFormatIDC`, as a byte
/// offset from the start of NV_ENC_CONFIG (encodeCodecConfig itself sits at
/// 168).  1 = yuv420, 3 = yuv444.
const NVENC_H264_CHROMA_FORMAT_IDC_OFFSET: usize = 360;

fn w32(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_ne_bytes());
}
fn w64(buf: &mut [u8], off: usize, val: u64) {
    buf[off..off + 8].copy_from_slice(&val.to_ne_bytes());
}
fn wptr(buf: &mut [u8], off: usize, val: *mut c_void) {
    buf[off..off + 8].copy_from_slice(&(val as u64).to_ne_bytes());
}
fn wguid(buf: &mut [u8], off: usize, g: NvGuid) {
    w32(buf, off, g.0);
    buf[off + 4..off + 6].copy_from_slice(&g.1.to_ne_bytes());
    buf[off + 6..off + 8].copy_from_slice(&g.2.to_ne_bytes());
    buf[off + 8..off + 16].copy_from_slice(&g.3);
}
/// Write `NV_ENC_RC_PARAMS::constQP` (qpInterP / qpInterB / qpIntra) inside
/// an `NV_ENC_CONFIG` buffer.
fn write_const_qp(config_buf: &mut [u8], qp: u32) {
    w32(config_buf, 48, qp); // constQP.qpInterP
    w32(config_buf, 52, qp); // constQP.qpInterB
    w32(config_buf, 56, qp); // constQP.qpIntra
}

fn r32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
}
fn rptr(buf: &[u8], off: usize) -> *mut c_void {
    u64::from_ne_bytes(buf[off..off + 8].try_into().unwrap()) as *mut c_void
}

// ---------------------------------------------------------------------------
// NvencDirectEncoder
// ---------------------------------------------------------------------------

pub struct NvencDirectEncoder {
    encoder: *mut c_void,
    input_buffer: *mut c_void, // fallback NV_ENC input buffer (unused with CUDA path)
    output_buffer: *mut c_void,
    width: u32,
    height: u32,
    frame_idx: u32,
    force_idr: bool,
    codec_flag: u8, // SURFACE_FRAME_CODEC_* for the wire protocol
    fns: &'static NvEncFunctionList,
    cuda_ctx: gpu_libs::CUcontext,
    // CUDA-accelerated input path: device memory + registered NVENC resource.
    // BGRA (ARGB) buffer — used for BGRA input and the legacy write_input_bgra path.
    cuda_devptr: gpu_libs::CUdeviceptr,
    cuda_registered: *mut c_void, // NV_ENC registered resource handle (ARGB format)
    cuda_pitch: u32,              // pitch in bytes (width * 4 for ARGB)
    pinned_host: *mut u8,         // page-locked staging buffer
    pinned_size: usize,
    // RGBA (ABGR) buffer — separate device allocation registered as ABGR format,
    // so NVENC handles RGBA→YUV conversion on the GPU without CPU R/B swaps.
    cuda_devptr_abgr: gpu_libs::CUdeviceptr,
    cuda_registered_abgr: *mut c_void,
    // NV12 buffer — semi-planar YUV, height * 1.5 bytes, different size from RGB.
    cuda_devptr_nv12: gpu_libs::CUdeviceptr,
    cuda_registered_nv12: *mut c_void,
    nv12_pitch: u32,
    verbose: bool,
    /// Cached SPS+PPS NAL units (Annex B with start codes) from the first
    /// IDR frame.  Prepended to subsequent IDR frames that NVENC emits
    /// without SPS/PPS (the default unless repeatSPSPPS is set, which
    /// requires fragile struct-offset manipulation).
    h264_sps_pps: Vec<u8>,
    /// `NV_ENC_INITIALIZE_PARAMS` and `NV_ENC_CONFIG` as initialized.
    /// Retained because `nvEncReconfigureEncoder` wants a complete
    /// `NV_ENC_INITIALIZE_PARAMS` again, and the driver forbids changing
    /// anything but rate control across a reconfigure — so the way to
    /// change only the QP is to re-submit these bytes with the QP edited.
    init_params: Vec<u8>,
    encode_config: Vec<u8>,
}

// NVENC encoder handle and CUDA context are thread-safe with proper push/pop.
unsafe impl Send for NvencDirectEncoder {}

/// Unwinds what `try_new` has acquired when it bails out partway.
///
/// `try_new` has a dozen early-error paths after `cuCtxCreate_v2`, and
/// every one of them used to return without releasing the context — so
/// each rejected configuration leaked one.  That is not a slow drip:
/// the server retries encoder creation per surface, per client, per
/// tick, so a host that refuses the first configuration tried (4:4:4,
/// say) burns through device memory until *every* encoder fails to
/// initialize and the whole pipeline silently falls back to CPU
/// encoding.
///
/// Destroying the context is enough to reclaim the pitched device
/// allocations and pinned host memory made after it, since those belong
/// to it; the encode session is torn down explicitly first because it
/// is owned by NVENC rather than by the context.
struct NvencInitGuard<'a> {
    cuda: &'a gpu_libs::CudaFns,
    fns: Option<&'a NvEncFunctionList>,
    ctx: gpu_libs::CUcontext,
    encoder: *mut c_void,
}

impl NvencInitGuard<'_> {
    /// Hand ownership of the context and session to the encoder being
    /// returned, so they outlive this guard.
    fn disarm(mut self) {
        self.ctx = ptr::null_mut();
        self.encoder = ptr::null_mut();
    }
}

impl Drop for NvencInitGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            if let Some(fns) = self.fns
                && !self.encoder.is_null()
            {
                (fns.nvEncDestroyEncoder)(self.encoder);
            }
            if !self.ctx.is_null() {
                (self.cuda.cuCtxDestroy_v2)(self.ctx);
            }
        }
    }
}

/// What this host's NVENC engine will accept for one codec.
///
/// Every field is a property of the device and the driver, which is what
/// makes it worth answering once and keeping — unlike the failure to build
/// one particular encoder, which usually says something about the frame.
/// Keeping those apart is the point: a 256x54 dock thumbnail is under
/// `min_height` for AV1, and reading its refusal as "this host has no NVENC"
/// took hardware encoding away from every viewer until the server restarted.
#[derive(Clone, Copy, Debug)]
pub struct NvencCaps {
    pub min_width: u32,
    pub min_height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub yuv444: bool,
}

impl NvencCaps {
    /// Why this engine will not take a `width`x`height` frame — `None` if it
    /// will.  The caller is expected to fall down the encoder chain rather
    /// than write the backend off: this is a verdict on the frame.
    fn refuse(&self, width: u32, height: u32) -> Option<String> {
        (width < self.min_width
            || height < self.min_height
            || width > self.max_width
            || height > self.max_height)
            .then(|| {
                format!(
                    "{width}x{height} is outside NVENC's {}x{}–{}x{} range",
                    self.min_width, self.min_height, self.max_width, self.max_height,
                )
            })
    }
}

/// Read one `NV_ENC_CAPS` ordinal.  Failures report as `None`, which the
/// caller turns into a conservative answer rather than a hard error — a
/// driver that cannot answer a caps query can still encode.
fn encode_cap(
    fns: &NvEncFunctionList,
    encoder: *mut c_void,
    codec_guid: NvGuid,
    cap: u32,
) -> Option<u32> {
    let mut caps_param = vec![0u8; NV_ENC_CAPS_PARAM_SIZE];
    w32(&mut caps_param, 0, NV_ENC_CAPS_PARAM_VER);
    w32(&mut caps_param, 4, cap);
    let mut value: i32 = 0;
    // SAFETY: `encoder` is an open session, and `caps_param` is a
    // NV_ENC_CAPS_PARAM of the declared version.
    let status = unsafe {
        (fns.nvEncGetEncodeCaps)(
            encoder,
            codec_guid,
            caps_param.as_mut_ptr() as *mut c_void,
            &mut value,
        )
    };
    (status == NV_ENC_SUCCESS && value > 0).then_some(value as u32)
}

/// Codec GUID and wire codec flag for a codec name.
fn nvenc_codec(codec: &str) -> Result<(NvGuid, u8), String> {
    match codec {
        "h264" => Ok((
            NV_ENC_CODEC_H264_GUID,
            blit_remote::SURFACE_FRAME_CODEC_H264,
        )),
        "av1" => Ok((NV_ENC_CODEC_AV1_GUID, blit_remote::SURFACE_FRAME_CODEC_AV1)),
        _ => Err(format!("unsupported NVENC codec: {codec}")),
    }
}

/// What this host's NVENC will take for `codec`, asked once per process.
///
/// The query needs a session of its own, so it costs one open/close the first
/// time and nothing after that.  Both the answer *and* the reason there isn't
/// one are cached: "no CUDA on this box" is as durable a fact as the maximum
/// frame size, and re-running `cuInit` on every surface resize is what the
/// cache is for.
pub fn caps(codec: &str, verbose: bool) -> Result<NvencCaps, String> {
    static CAPS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Result<NvencCaps, String>>>> =
        std::sync::OnceLock::new();
    let cache = CAPS.get_or_init(Default::default);
    if let Ok(map) = cache.lock()
        && let Some(hit) = map.get(codec)
    {
        return hit.clone();
    }

    let answer = (|| {
        let (codec_guid, _) = nvenc_codec(codec)?;
        let cuda = gpu_libs::cuda().map_err(|e| format!("CUDA: {e}"))?;
        let (fns, ctx, encoder) = open_session(cuda)?;
        // Releases both when this scope ends — the session existed only to
        // answer the query.
        let guard = NvencInitGuard {
            cuda,
            fns: Some(fns),
            ctx,
            encoder,
        };
        let caps = NvencCaps {
            // A driver that will not name a minimum is saying it has none.
            min_width: encode_cap(fns, encoder, codec_guid, NV_ENC_CAPS_WIDTH_MIN).unwrap_or(1),
            min_height: encode_cap(fns, encoder, codec_guid, NV_ENC_CAPS_HEIGHT_MIN).unwrap_or(1),
            // …and one that will not name a maximum gets the largest frame
            // any AV1 or H.264 level admits, so the chain's own ceilings
            // stay the binding constraint rather than this fallback.
            max_width: encode_cap(fns, encoder, codec_guid, NV_ENC_CAPS_WIDTH_MAX)
                .unwrap_or(u16::MAX as u32),
            max_height: encode_cap(fns, encoder, codec_guid, NV_ENC_CAPS_HEIGHT_MAX)
                .unwrap_or(u16::MAX as u32),
            yuv444: encode_cap(fns, encoder, codec_guid, NV_ENC_CAPS_SUPPORT_YUV444_ENCODE)
                .is_some(),
        };
        drop(guard);
        Ok(caps)
    })();

    if verbose {
        match &answer {
            Ok(c) => eprintln!(
                "[nvenc] {codec}: {}x{}–{}x{}, 4:4:4 {}",
                c.min_width,
                c.min_height,
                c.max_width,
                c.max_height,
                if c.yuv444 { "yes" } else { "no" },
            ),
            Err(e) => eprintln!("[nvenc] {codec}: unavailable — {e}"),
        }
    }
    if let Ok(mut map) = cache.lock() {
        map.insert(codec.to_string(), answer.clone());
    }
    answer
}

/// Open a CUDA context and an NVENC session on it.  Both belong to the
/// caller, who must hand them to an [`NvencInitGuard`] or an encoder.
fn open_session(
    cuda: &'static gpu_libs::CudaFns,
) -> Result<(&'static NvEncFunctionList, gpu_libs::CUcontext, *mut c_void), String> {
    let nvenc_fns = gpu_libs::nvenc().map_err(|e| format!("NVENC: {e}"))?;

    let mut status = unsafe { (cuda.cuInit)(0) };
    if status != 0 {
        return Err(format!("cuInit failed: {status}"));
    }

    let cuda_device_idx: i32 = std::env::var("BLIT_CUDA_DEVICE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut device: gpu_libs::CUdevice = 0;
    status = unsafe { (cuda.cuDeviceGet)(&mut device, cuda_device_idx) };
    if status != 0 {
        return Err(format!("cuDeviceGet({cuda_device_idx}) failed: {status}"));
    }

    let mut ctx: gpu_libs::CUcontext = ptr::null_mut();
    status = unsafe { (cuda.cuCtxCreate_v2)(&mut ctx, 0, device) };
    if status != 0 {
        return Err(format!("cuCtxCreate failed: {status}"));
    }

    // NVENC function table — initialized once, reused across all sessions.
    static NVENC_FN_LIST: std::sync::OnceLock<Result<NvEncFunctionList, String>> =
        std::sync::OnceLock::new();
    let result = NVENC_FN_LIST.get_or_init(|| {
        let fn_list_ver = nvencapi_struct_version(2);
        let mut fl = std::mem::MaybeUninit::<NvEncFunctionList>::zeroed();
        // SAFETY: version is the first field (offset 0) in the repr(C) struct.
        unsafe { (*fl.as_mut_ptr()).version = fn_list_ver };
        let nv_status = unsafe { (nvenc_fns.NvEncodeAPICreateInstance)(fl.as_mut_ptr().cast()) };
        // SAFETY: NvEncodeAPICreateInstance fills all function pointers.
        let fl = unsafe { fl.assume_init() };
        if nv_status != NV_ENC_SUCCESS {
            return Err(format!("NvEncodeAPICreateInstance failed: {nv_status}"));
        }
        Ok(fl)
    });
    let fns = match result {
        Ok(fl) => fl,
        Err(e) => {
            unsafe { (cuda.cuCtxDestroy_v2)(ctx) };
            return Err(e.clone());
        }
    };
    let fns: &'static NvEncFunctionList =
        // SAFETY: OnceLock guarantees the value lives for 'static.
        unsafe { &*(fns as *const NvEncFunctionList) };

    let mut open_buf = vec![0u8; NVENC_OPEN_ENCODE_SESSION_EX_SIZE];
    w32(&mut open_buf, 0, NV_ENC_OPEN_ENCODE_SESSION_EX_VER); // version @ 0
    w32(&mut open_buf, 4, 1); // deviceType = CUDA @ 4
    wptr(&mut open_buf, 8, ctx); // device @ 8
    // _reserved ptr @ 16 = NULL
    w32(&mut open_buf, 24, NVENCAPI_VERSION); // apiVersion @ 24

    let mut encoder: *mut c_void = ptr::null_mut();
    let nv_status = unsafe {
        (fns.nvEncOpenEncodeSessionEx)(open_buf.as_mut_ptr() as *mut c_void, &mut encoder)
    };
    if nv_status != NV_ENC_SUCCESS {
        unsafe { (cuda.cuCtxDestroy_v2)(ctx) };
        return Err(format!("nvEncOpenEncodeSessionEx failed: {nv_status}"));
    }
    Ok((fns, ctx, encoder))
}

impl NvencDirectEncoder {
    /// Try to create an NVENC encoder for the given codec and dimensions.
    ///
    /// `codec` should be `"h264"` or `"av1"`.
    /// `qp` is the constant QP value (0–51 for H.264, 0–255 for AV1).
    /// `preset` is the NVENC preset index, 1 (P1, fastest) … 7 (P7, slowest).
    pub fn try_new(
        codec: &str,
        width: u32,
        height: u32,
        qp: u32,
        preset: u8,
        verbose: bool,
        chroma: crate::surface_encoder::ChromaSubsampling,
    ) -> Result<Self, String> {
        let (codec_guid, codec_flag) = nvenc_codec(codec)?;

        // Ask the device what it takes before building anything.  Both
        // answers below are settled here rather than by watching an
        // `nvEncInitializeEncoder` fail: a frame outside the engine's range
        // — a 256x54 dock thumbnail, say — comes back as a plain refusal
        // that costs no session and says nothing about the host.
        let caps = caps(codec, verbose)?;
        if chroma.is_444() && !caps.yuv444 {
            return Err(format!(
                "NVENC {codec} does not support 4:4:4 encoding on this GPU"
            ));
        }
        if let Some(refusal) = caps.refuse(width, height) {
            return Err(refusal);
        }

        let cuda = gpu_libs::cuda().map_err(|e| format!("CUDA: {e}"))?;
        let (fns, ctx, encoder) = open_session(cuda)?;
        // From here on every `return Err` must release the context and the
        // session; the guard does it so new early-exits cannot reintroduce
        // the leak.
        let guard = NvencInitGuard {
            cuda,
            fns: Some(fns),
            ctx,
            encoder,
        };
        let mut status;

        // Get preset config — uses exact SDK struct sizes to avoid version
        // mismatch (the driver validates struct size via the version tag).
        let mut preset_buf = vec![0u8; NVENC_PRESET_CONFIG_SIZE];
        w32(&mut preset_buf, 0, NV_ENC_PRESET_CONFIG_VER); // version @ 0
        w32(&mut preset_buf, 8, NV_ENC_CONFIG_VER); // presetCfg.version @ 8

        let nv_status = unsafe {
            (fns.nvEncGetEncodePresetConfigEx)(
                encoder,
                codec_guid,
                preset_guid(preset),
                NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                preset_buf.as_mut_ptr() as *mut c_void,
            )
        };
        if nv_status != NV_ENC_SUCCESS {
            return Err(format!("nvEncGetEncodePresetConfigEx failed: {nv_status}"));
        }

        // Extract the preset's NV_ENC_CONFIG (starts at offset 8 in preset_buf)
        // and apply our overrides.
        let mut config_buf = vec![0u8; NVENC_CONFIG_SIZE];
        config_buf.copy_from_slice(&preset_buf[8..8 + NVENC_CONFIG_SIZE]);
        // gopLength @ 20, frameIntervalP @ 24
        w32(&mut config_buf, 20, 120); // gop_length
        w32(&mut config_buf, 24, 1); // frame_interval_p (no B-frames)
        // rcParams starts at config offset 40 (after version=0, profileGUID=4,
        // gopLength=20, frameIntervalP=24, monoChromeEncoding=28,
        // frameFieldMode=32, mvPrecision=36).  NV_ENC_RC_PARAMS itself opens
        // with its own u32 version, which the preset config already filled —
        // so rateControlMode is at 44 and constQP (qpInterP/qpInterB/qpIntra)
        // at 48/52/56.
        w32(&mut config_buf, 44, NV_ENC_PARAMS_RC_CONSTQP);
        write_const_qp(&mut config_buf, qp);

        // Set 4:4:4 profile when requested.  For H.264 this is the High 4:4:4
        // Predictive profile; for AV1 the SDK auto-selects the right profile
        // based on chromaFormatIDC in the codec config.
        if chroma.is_444() && codec == "h264" {
            // profileGUID @ offset 4 in NV_ENC_CONFIG
            wguid(&mut config_buf, 4, NV_ENC_H264_PROFILE_HIGH_444_GUID);
            // The profile GUID alone is not enough: NV_ENC_CONFIG_H264
            // carries its own chromaFormatIDC, which the preset left at 1
            // (yuv420).  A High 4:4:4 profile against a 4:2:0 codec config
            // is contradictory, and nvEncInitializeEncoder rejects the pair
            // with NV_ENC_ERR_INVALID_PARAM.
            w32(&mut config_buf, NVENC_H264_CHROMA_FORMAT_IDC_OFFSET, 3);
        }

        // Initialize encoder
        let mut init_buf = vec![0u8; NVENC_INITIALIZE_PARAMS_SIZE];
        w32(&mut init_buf, 0, NV_ENC_INITIALIZE_PARAMS_VER);
        wguid(&mut init_buf, 4, codec_guid); // encodeGUID @ 4
        wguid(&mut init_buf, 20, preset_guid(preset)); // presetGUID @ 20
        w32(&mut init_buf, 36, width); // encodeWidth @ 36
        w32(&mut init_buf, 40, height); // encodeHeight @ 40
        w32(&mut init_buf, 44, width); // darWidth @ 44
        w32(&mut init_buf, 48, height); // darHeight @ 48
        w32(&mut init_buf, 52, 60); // frameRateNum @ 52
        w32(&mut init_buf, 56, 1); // frameRateDen @ 56
        w32(&mut init_buf, 64, 1); // enablePTD @ 64
        wptr(&mut init_buf, 88, config_buf.as_mut_ptr() as *mut c_void); // encodeConfig ptr @ 88
        w32(&mut init_buf, 96, width); // maxEncodeWidth @ 96
        w32(&mut init_buf, 100, height); // maxEncodeHeight @ 100
        w32(&mut init_buf, 136, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY); // tuningInfo @ 136

        let nv_status =
            unsafe { (fns.nvEncInitializeEncoder)(encoder, init_buf.as_mut_ptr() as *mut c_void) };
        if nv_status != NV_ENC_SUCCESS {
            return Err(format!("nvEncInitializeEncoder failed: {nv_status}"));
        }

        // Create input buffer (BGRA)
        let mut input_buf = vec![0u8; NVENC_CREATE_INPUT_BUFFER_SIZE];
        w32(&mut input_buf, 0, NV_ENC_CREATE_INPUT_BUFFER_VER);
        w32(&mut input_buf, 4, width); // width @ 4
        w32(&mut input_buf, 8, height); // height @ 8
        w32(&mut input_buf, 16, NV_ENC_BUFFER_FORMAT_ARGB); // bufferFmt @ 16

        let nv_status =
            unsafe { (fns.nvEncCreateInputBuffer)(encoder, input_buf.as_mut_ptr() as *mut c_void) };
        if nv_status != NV_ENC_SUCCESS {
            return Err(format!("nvEncCreateInputBuffer failed: {nv_status}"));
        }
        let input_buffer_ptr = rptr(&input_buf, 24); // inputBuffer @ 24

        // Create bitstream (output) buffer
        let mut output_buf = vec![0u8; NVENC_CREATE_BITSTREAM_BUFFER_SIZE];
        w32(&mut output_buf, 0, NV_ENC_CREATE_BITSTREAM_BUFFER_VER);

        let nv_status = unsafe {
            (fns.nvEncCreateBitstreamBuffer)(encoder, output_buf.as_mut_ptr() as *mut c_void)
        };
        if nv_status != NV_ENC_SUCCESS {
            return Err(format!("nvEncCreateBitstreamBuffer failed: {nv_status}"));
        }
        let output_buffer_ptr = rptr(&output_buf, 16); // bitstreamBuffer @ 16

        // Allocate CUDA device memory for input frames.  Using cuMemcpyHtoD
        // to upload BGRA data is ~100× faster than writing through the PCIe
        // BAR via nvEncLockInputBuffer (DMA engine vs uncached CPU writes).
        //
        // Use cuMemAllocPitch to get a pitch aligned to the GPU's preferred
        // alignment (typically 256 or 512 bytes).  NVENC's DMA engine reads
        // entire pitch-aligned rows; an unaligned pitch from cuMemAlloc can
        // cause the video engine to read garbage bytes at row boundaries.
        let mut cuda_devptr: gpu_libs::CUdeviceptr = 0;
        let mut cuda_pitch_bytes: usize = 0;
        status = unsafe {
            (cuda.cuMemAllocPitch_v2)(
                &mut cuda_devptr,
                &mut cuda_pitch_bytes,
                (width * 4) as usize, // width in bytes (ARGB = 4 bpp)
                height as usize,
                16, // element size hint (4 bytes per pixel, but 16 aligns rows)
            )
        };
        if status != 0 {
            return Err(format!("cuMemAllocPitch failed: {status}"));
        }
        let cuda_pitch = cuda_pitch_bytes as u32;
        let frame_size = cuda_pitch_bytes * height as usize;

        // Allocate page-locked (pinned) host memory for staging.
        // cuMemcpyHtoD from pinned memory uses DMA at full PCIe bandwidth;
        // from pageable memory the driver must pin pages on every call (~60ms
        // overhead at 1920×1080).
        //
        // Size to the pitch-aligned frame so we can write at the aligned
        // stride directly into pinned memory before the DMA transfer.
        let mut pinned_host: *mut c_void = ptr::null_mut();
        status = unsafe { (cuda.cuMemAllocHost_v2)(&mut pinned_host, frame_size) };
        if status != 0 {
            unsafe { (cuda.cuMemFree_v2)(cuda_devptr) };
            return Err(format!("cuMemAllocHost failed: {status}"));
        }

        // Register the CUDA device memory with NVENC.
        // NV_ENC_REGISTER_RESOURCE offsets (12.1):
        //   version=0, resourceType=4, width=8, height=12,
        //   pitch=16, subResourceIndex=20, resourceToRegister=24(ptr),
        //   registeredResource=32(ptr), bufferFormat=40, bufferUsage=44
        let mut reg_buf = vec![0u8; NVENC_REGISTER_RESOURCE_SIZE];
        w32(&mut reg_buf, 0, NV_ENC_REGISTER_RESOURCE_VER);
        w32(&mut reg_buf, 4, NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR);
        w32(&mut reg_buf, 8, width);
        w32(&mut reg_buf, 12, height);
        w32(&mut reg_buf, 16, cuda_pitch);
        // resourceToRegister is a CUdeviceptr (u64) written as a pointer-sized value
        wptr(&mut reg_buf, 24, cuda_devptr as *mut c_void);
        w32(&mut reg_buf, 40, NV_ENC_BUFFER_FORMAT_ARGB);

        let nv_status =
            unsafe { (fns.nvEncRegisterResource)(encoder, reg_buf.as_mut_ptr() as *mut c_void) };
        if nv_status != NV_ENC_SUCCESS {
            unsafe { (cuda.cuMemFree_v2)(cuda_devptr) };
            return Err(format!("nvEncRegisterResource failed: {nv_status}"));
        }
        let cuda_registered = rptr(&reg_buf, 32); // registeredResource @ 32

        // --- ABGR (RGBA-in-memory) buffer ---
        // Same pixel size as ARGB, same pitch-aligned allocation.
        let mut cuda_devptr_abgr: gpu_libs::CUdeviceptr = 0;
        let mut abgr_pitch_bytes: usize = 0;
        status = unsafe {
            (cuda.cuMemAllocPitch_v2)(
                &mut cuda_devptr_abgr,
                &mut abgr_pitch_bytes,
                (width * 4) as usize,
                height as usize,
                16,
            )
        };
        if status != 0 {
            return Err(format!("cuMemAllocPitch (ABGR) failed: {status}"));
        }
        // Pitch should match the ARGB buffer (same width and element size).
        debug_assert_eq!(abgr_pitch_bytes, cuda_pitch_bytes);
        let mut reg_abgr = vec![0u8; NVENC_REGISTER_RESOURCE_SIZE];
        w32(&mut reg_abgr, 0, NV_ENC_REGISTER_RESOURCE_VER);
        w32(&mut reg_abgr, 4, NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR);
        w32(&mut reg_abgr, 8, width);
        w32(&mut reg_abgr, 12, height);
        w32(&mut reg_abgr, 16, cuda_pitch);
        wptr(&mut reg_abgr, 24, cuda_devptr_abgr as *mut c_void);
        w32(&mut reg_abgr, 40, NV_ENC_BUFFER_FORMAT_ABGR);
        let nv_status =
            unsafe { (fns.nvEncRegisterResource)(encoder, reg_abgr.as_mut_ptr() as *mut c_void) };
        if nv_status != NV_ENC_SUCCESS {
            unsafe { (cuda.cuMemFree_v2)(cuda_devptr_abgr) };
            return Err(format!("nvEncRegisterResource (ABGR) failed: {nv_status}"));
        }
        let cuda_registered_abgr = rptr(&reg_abgr, 32);

        // --- NV12 buffer ---
        // Semi-planar: Y plane (width × height) + UV plane (width × height/2).
        // Use cuMemAllocPitch for aligned NV12 pitch (1 byte per Y sample).
        let mut cuda_devptr_nv12: gpu_libs::CUdeviceptr = 0;
        let mut nv12_pitch_bytes: usize = 0;
        // Allocate for 1.5× height (Y + UV) so the whole NV12 frame fits.
        // The pitch is determined by the Y plane width.
        let nv12_alloc_h = height + height / 2;
        status = unsafe {
            (cuda.cuMemAllocPitch_v2)(
                &mut cuda_devptr_nv12,
                &mut nv12_pitch_bytes,
                width as usize, // 1 byte per Y sample
                nv12_alloc_h as usize,
                16,
            )
        };
        if status != 0 {
            return Err(format!("cuMemAllocPitch (NV12) failed: {status}"));
        }
        let nv12_pitch = nv12_pitch_bytes as u32;
        let mut reg_nv12 = vec![0u8; NVENC_REGISTER_RESOURCE_SIZE];
        w32(&mut reg_nv12, 0, NV_ENC_REGISTER_RESOURCE_VER);
        w32(&mut reg_nv12, 4, NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR);
        w32(&mut reg_nv12, 8, width);
        w32(&mut reg_nv12, 12, height);
        w32(&mut reg_nv12, 16, nv12_pitch);
        wptr(&mut reg_nv12, 24, cuda_devptr_nv12 as *mut c_void);
        w32(&mut reg_nv12, 40, NV_ENC_BUFFER_FORMAT_NV12);
        let nv_status =
            unsafe { (fns.nvEncRegisterResource)(encoder, reg_nv12.as_mut_ptr() as *mut c_void) };
        if nv_status != NV_ENC_SUCCESS {
            unsafe { (cuda.cuMemFree_v2)(cuda_devptr_nv12) };
            return Err(format!("nvEncRegisterResource (NV12) failed: {nv_status}"));
        }
        let cuda_registered_nv12 = rptr(&reg_nv12, 32);

        if verbose {
            eprintln!(
                "[nvenc-direct] initialized {codec} encoder for {width}x{height} pitch={cuda_pitch} nv12_pitch={nv12_pitch} (CUDA upload)"
            );
        }

        // Construction succeeded — the encoder below owns the context and
        // session now, and frees them in its own Drop.
        guard.disarm();

        Ok(Self {
            encoder,
            input_buffer: input_buffer_ptr,
            output_buffer: output_buffer_ptr,
            width,
            height,
            frame_idx: 0,
            force_idr: false,
            codec_flag,
            fns,
            cuda_ctx: ctx,
            cuda_devptr,
            cuda_registered,
            cuda_pitch,
            pinned_host: pinned_host as *mut u8,
            pinned_size: frame_size,
            cuda_devptr_abgr,
            cuda_registered_abgr,
            cuda_devptr_nv12,
            cuda_registered_nv12,
            nv12_pitch,
            verbose,
            h264_sps_pps: Vec::new(),
            init_params: init_buf,
            encode_config: config_buf,
        })
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    /// Move the constant QP without tearing the session down.
    ///
    /// `resetEncoder` stays 0: resetting rate-control state also forces an
    /// IDR when `enablePTD` is set (it is), and a keyframe is the last thing
    /// wanted when the reason for the change is congestion.  Returns false
    /// if the driver rejects the reconfigure, leaving the encoder at its
    /// current QP so the caller can decide whether a rebuild is worth it.
    pub fn set_qp(&mut self, qp: u32) -> bool {
        let cuda = match gpu_libs::cuda() {
            Ok(c) => c,
            Err(_) => return false,
        };
        write_const_qp(&mut self.encode_config, qp);
        let mut params = vec![0u8; NVENC_RECONFIGURE_PARAMS_SIZE];
        w32(&mut params, 0, NV_ENC_RECONFIGURE_PARAMS_VER);
        params[NVENC_RECONFIGURE_INIT_PARAMS_OFFSET
            ..NVENC_RECONFIGURE_INIT_PARAMS_OFFSET + NVENC_INITIALIZE_PARAMS_SIZE]
            .copy_from_slice(&self.init_params);
        // The retained init params carry a pointer to the config buffer; it
        // is still valid (a Vec's allocation does not move with the struct)
        // but re-point it anyway rather than depend on that.
        wptr(
            &mut params,
            NVENC_RECONFIGURE_INIT_PARAMS_OFFSET + 88,
            self.encode_config.as_mut_ptr() as *mut c_void,
        );
        w32(&mut params, NVENC_RECONFIGURE_FLAGS_OFFSET, 0);

        unsafe { (cuda.cuCtxPushCurrent_v2)(self.cuda_ctx) };
        let status = unsafe {
            (self.fns.nvEncReconfigureEncoder)(self.encoder, params.as_mut_ptr() as *mut c_void)
        };
        let mut dummy: gpu_libs::CUcontext = std::ptr::null_mut();
        unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };

        if status != NV_ENC_SUCCESS {
            if self.verbose {
                eprintln!("[nvenc] nvEncReconfigureEncoder(qp={qp}) failed: {status}");
            }
            return false;
        }
        true
    }

    /// Check whether the NVENC-reported picture type indicates a keyframe.
    ///
    /// For H.264 only `NV_ENC_PIC_TYPE_IDR` (3) is a true key frame.
    /// For AV1 the driver may report either `NV_ENC_PIC_TYPE_IDR` or
    /// `NV_ENC_PIC_TYPE_I` (2) — AV1 has no separate IDR concept, so
    /// both intra types correspond to key frames in practice (the
    /// ultra-low-latency preset never emits intra-only non-key frames).
    fn is_keyframe_pic_type(&self, pic_type: u32) -> bool {
        if pic_type == NV_ENC_PIC_TYPE_IDR {
            return true;
        }
        if self.codec_flag == blit_remote::SURFACE_FRAME_CODEC_AV1 && pic_type == NV_ENC_PIC_TYPE_I
        {
            return true;
        }
        false
    }

    /// Ensure an H.264 IDR frame includes SPS/PPS NAL units.
    ///
    /// NVENC only includes SPS/PPS in the very first IDR unless the
    /// `repeatSPSPPS` config flag is set (which requires fragile
    /// struct-offset writes).  Instead we cache the SPS+PPS from the
    /// first IDR and prepend them to subsequent IDRs that lack them.
    fn ensure_h264_sps_pps(&mut self, data: &mut Vec<u8>, is_idr: bool) {
        if self.codec_flag != blit_remote::SURFACE_FRAME_CODEC_H264 || !is_idr {
            return;
        }
        // Scan for SPS (NAL type 7) and PPS (NAL type 8).
        let has_sps_pps = h264_has_sps_pps(data);
        if has_sps_pps {
            // Cache the SPS+PPS prefix (everything before the first IDR
            // slice NAL, type 5).
            if self.h264_sps_pps.is_empty()
                && let Some(prefix) = h264_extract_sps_pps_prefix(data)
            {
                self.h264_sps_pps = prefix;
            }
        } else if !self.h264_sps_pps.is_empty() {
            // Prepend cached SPS+PPS.
            let mut full = self.h264_sps_pps.clone();
            full.append(data);
            *data = full;
        }
    }

    /// Encode from a DMA-BUF fd, importing it into CUDA device memory via
    /// `cuImportExternalMemory` and registering that with NVENC.
    ///
    /// **This does not work today, and has never run.** Nothing hands NVENC a
    /// `PixelData::DmaBuf` — the server only takes the external-buffer branch
    /// for VA-API — so it has no live caller; and if it had one, the import
    /// would fail. It asks for `CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD`,
    /// which CUDA documents as a handle obtained from Vulkan via
    /// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT` — an NVIDIA-internal
    /// object, not a `dma_buf`. Every fd this codebase can produce (GBM BOs,
    /// `vkGetMemoryFdKHR` with `DMA_BUF_EXT`) is a `dma_buf`.
    ///
    /// Measured on nvidia-x11 595.84 / RTX 4090, using the exact descriptor
    /// layout below against a real GBM BO from `/dev/dri/renderD128`: every
    /// handle type from 1 to 32 returns `CUDA_ERROR_INVALID_VALUE`. The blob
    /// carries one dma_buf string —
    /// `CU_EXTERNAL_MEMORY_HANDLE_TYPE_DMABUF_FD not supported on platform`.
    ///
    /// Making the path real means exporting the compositor's NV12 buffer as
    /// `OPAQUE_FD` instead of `DMA_BUF_EXT`, plus an exported `VkSemaphore`
    /// waited on with `cuWaitExternalSemaphoresAsync`: an `OPAQUE_FD`
    /// allocation carries no implicit `dma_buf` fencing, so CUDA would
    /// otherwise race the Vulkan blit. Kept rather than deleted because the
    /// registration and encode half is still the shape that work needs.
    ///
    /// Returns `None` if the CUDA driver lacks external-memory import
    /// (pre-10.0) or if the import fails for this fd — today, always the
    /// latter. Callers fall back to a CPU copy, which is why the failure is
    /// invisible apart from the one-shot `[nvenc-dmabuf]
    /// cuImportExternalMemory failed` line.
    #[cfg(target_os = "linux")]
    #[allow(clippy::too_many_arguments)]
    pub fn encode_dmabuf_fd(
        &mut self,
        fd: std::os::fd::RawFd,
        fourcc: u32,
        _modifier: u64,
        stride: u32,
        _offset: u32,
        src_width: u32,
        src_height: u32,
    ) -> Option<(Vec<u8>, bool)> {
        let cuda = gpu_libs::cuda().ok()?;
        let cu_import = cuda.cuImportExternalMemory?;
        let cu_get_buf = cuda.cuExternalMemoryGetMappedBuffer?;
        let cu_destroy = cuda.cuDestroyExternalMemory?;

        // Map DRM fourcc to the NVENC buffer format.  NVENC accepts ARGB
        // (BGRA in memory) and ABGR (RGBA in memory) natively — no CPU
        // colorspace conversion needed for either.
        let nvenc_fmt = match fourcc {
            f if f == blit_compositor::drm_fourcc::ARGB8888
                || f == blit_compositor::drm_fourcc::XRGB8888 =>
            {
                NV_ENC_BUFFER_FORMAT_ARGB
            }
            f if f == blit_compositor::drm_fourcc::ABGR8888
                || f == blit_compositor::drm_fourcc::XBGR8888 =>
            {
                NV_ENC_BUFFER_FORMAT_ABGR
            }
            _ => return None, // NV12 DMA-BUFs are multi-plane; skip for now
        };

        // DMA-BUF size from lseek.
        let buf_size = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
        if buf_size <= 0 {
            return None;
        }
        let buf_size = buf_size as u64;

        // dup() the fd because CUDA takes ownership and closes it.
        let dup_fd = unsafe { libc::dup(fd) };
        if dup_fd < 0 {
            return None;
        }

        unsafe { (cuda.cuCtxPushCurrent_v2)(self.cuda_ctx) };

        // CUDA_EXTERNAL_MEMORY_HANDLE_DESC (CUDA 10.0+)
        // Layout (from cuda.h):
        //   enum CUexternalMemoryHandleType type;  // offset 0, 4 bytes
        //   union { int fd; ... } handle;           // offset 8 (aligned), 8 bytes
        //     (for CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD, handle.fd at offset 8)
        //   unsigned long long size;                // offset 16
        //   unsigned int flags;                     // offset 24
        //   unsigned int reserved[16];              // offset 28
        // Total size: ~96 bytes, we use 128 to be safe.
        const CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD: u32 = 1;
        let mut handle_desc = [0u8; 128];
        // type @ 0
        handle_desc[0..4].copy_from_slice(&CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD.to_ne_bytes());
        // handle.fd @ 8 (store as i32 in the union)
        handle_desc[8..12].copy_from_slice(&dup_fd.to_ne_bytes());
        // size @ 16
        handle_desc[16..24].copy_from_slice(&buf_size.to_ne_bytes());
        // flags @ 24 = 0

        let mut ext_mem: gpu_libs::CUexternalMemory = ptr::null_mut();
        let status = unsafe { cu_import(&mut ext_mem, handle_desc.as_ptr() as *const _) };
        if status != 0 {
            // Import failed — close the dup'd fd (CUDA didn't take it).
            unsafe { libc::close(dup_fd) };
            let mut dummy: gpu_libs::CUcontext = ptr::null_mut();
            unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };
            static LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!("[nvenc-dmabuf] cuImportExternalMemory failed: {status}");
            }
            return None;
        }
        // fd ownership transferred to CUDA on success — do NOT close dup_fd.

        // CUDA_EXTERNAL_MEMORY_BUFFER_DESC
        // Layout:
        //   unsigned long long offset;    // 0
        //   unsigned long long size;      // 8
        //   unsigned int flags;           // 16
        //   unsigned int reserved[16];    // 20
        // Total: ~84 bytes, use 128.
        let mut buf_desc = [0u8; 128];
        // offset @ 0 = 0
        buf_desc[8..16].copy_from_slice(&buf_size.to_ne_bytes()); // size @ 8

        let mut devptr: gpu_libs::CUdeviceptr = 0;
        let status = unsafe { cu_get_buf(&mut devptr, ext_mem, buf_desc.as_ptr() as *const _) };
        if status != 0 {
            unsafe { cu_destroy(ext_mem) };
            let mut dummy: gpu_libs::CUcontext = ptr::null_mut();
            unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };
            eprintln!("[nvenc-dmabuf] cuExternalMemoryGetMappedBuffer failed: {status}");
            return None;
        }

        // Register the imported device pointer with NVENC as a temporary
        // input resource.  The existing self.cuda_registered is for the
        // persistent staging buffer — we need a separate registration here
        // because the devptr, pitch, and dimensions may differ.
        let enc_w = src_width;
        let enc_h = src_height;
        let pitch = stride;

        let mut reg_buf = vec![0u8; NVENC_REGISTER_RESOURCE_SIZE];
        w32(&mut reg_buf, 0, NV_ENC_REGISTER_RESOURCE_VER);
        w32(&mut reg_buf, 4, NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR);
        w32(&mut reg_buf, 8, enc_w);
        w32(&mut reg_buf, 12, enc_h);
        w32(&mut reg_buf, 16, pitch);
        wptr(&mut reg_buf, 24, devptr as *mut c_void);
        w32(&mut reg_buf, 40, nvenc_fmt);

        let nv_status = unsafe {
            (self.fns.nvEncRegisterResource)(self.encoder, reg_buf.as_mut_ptr() as *mut c_void)
        };
        if nv_status != NV_ENC_SUCCESS {
            unsafe { cu_destroy(ext_mem) };
            let mut dummy: gpu_libs::CUcontext = ptr::null_mut();
            unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };
            eprintln!("[nvenc-dmabuf] nvEncRegisterResource failed: {nv_status}");
            return None;
        }
        let registered = rptr(&reg_buf, 32);

        // Map the resource for encoding.
        let mut map_buf = vec![0u8; NVENC_MAP_INPUT_RESOURCE_SIZE];
        w32(&mut map_buf, 0, NV_ENC_MAP_INPUT_RESOURCE_VER);
        wptr(&mut map_buf, 16, registered);

        let nv_status = unsafe {
            (self.fns.nvEncMapInputResource)(self.encoder, map_buf.as_mut_ptr() as *mut c_void)
        };
        if nv_status != NV_ENC_SUCCESS {
            unsafe {
                (self.fns.nvEncUnregisterResource)(self.encoder, registered);
                cu_destroy(ext_mem);
            }
            let mut dummy: gpu_libs::CUcontext = ptr::null_mut();
            unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };
            eprintln!("[nvenc-dmabuf] nvEncMapInputResource failed: {nv_status}");
            return None;
        }
        let mapped_resource = rptr(&map_buf, 24);

        // Encode.
        let mut pic_buf = vec![0u8; NVENC_PIC_PARAMS_SIZE];
        w32(&mut pic_buf, 0, NV_ENC_PIC_PARAMS_VER);
        w32(&mut pic_buf, 4, enc_w);
        w32(&mut pic_buf, 8, enc_h);
        w32(&mut pic_buf, 12, pitch);
        w32(&mut pic_buf, 20, self.frame_idx);
        w64(&mut pic_buf, 24, self.frame_idx as u64);
        wptr(&mut pic_buf, 40, mapped_resource);
        wptr(&mut pic_buf, 48, self.output_buffer);
        w32(&mut pic_buf, 64, nvenc_fmt);
        w32(&mut pic_buf, 68, 1); // NV_ENC_PIC_STRUCT_FRAME

        if self.force_idr {
            // Include OUTPUT_SPSPPS (0x4) so that AV1 keyframes contain
            // the sequence header OBU and H.264 IDRs include SPS/PPS.
            // Without this, decoders joining mid-stream cannot decode
            // forced keyframes produced via the DMA-BUF path.
            w32(&mut pic_buf, 16, NV_ENC_PIC_FLAGS_FORCEIDR | 0x4);
            w32(&mut pic_buf, 72, NV_ENC_PIC_TYPE_IDR);
        }

        self.frame_idx += 1;

        let nv_status = unsafe {
            (self.fns.nvEncEncodePicture)(self.encoder, pic_buf.as_mut_ptr() as *mut c_void)
        };

        let result = if nv_status == NV_ENC_SUCCESS {
            // Encode succeeded — safe to clear the IDR request.
            self.force_idr = false;

            // Lock and read bitstream.
            let mut lock_buf = vec![0u8; NVENC_LOCK_BITSTREAM_SIZE];
            w32(&mut lock_buf, 0, NV_ENC_LOCK_BITSTREAM_VER);
            wptr(&mut lock_buf, 8, self.output_buffer);

            let lock_status = unsafe {
                (self.fns.nvEncLockBitstream)(self.encoder, lock_buf.as_mut_ptr() as *mut c_void)
            };
            if lock_status == NV_ENC_SUCCESS {
                let size = r32(&lock_buf, 36) as usize;
                let buf_ptr = rptr(&lock_buf, 56) as *const u8;
                let nal_data = if !buf_ptr.is_null() && size > 0 {
                    unsafe { std::slice::from_raw_parts(buf_ptr, size) }.to_vec()
                } else {
                    Vec::new()
                };
                let is_idr = self.is_keyframe_pic_type(r32(&lock_buf, 64));
                unsafe { (self.fns.nvEncUnlockBitstream)(self.encoder, self.output_buffer) };
                if nal_data.is_empty() {
                    None
                } else {
                    let mut nal_data = nal_data;
                    self.ensure_h264_sps_pps(&mut nal_data, is_idr);
                    Some((nal_data, is_idr))
                }
            } else {
                None
            }
        } else {
            if nv_status != NV_ENC_ERR_NEED_MORE_INPUT {
                eprintln!("[nvenc-dmabuf] nvEncEncodePicture failed: {nv_status}");
            }
            // force_idr stays true — next call retries.
            None
        };

        // Cleanup: unmap, unregister, destroy external memory.
        unsafe {
            (self.fns.nvEncUnmapInputResource)(self.encoder, mapped_resource);
            (self.fns.nvEncUnregisterResource)(self.encoder, registered);
            cu_destroy(ext_mem);
        }

        let mut dummy: gpu_libs::CUcontext = ptr::null_mut();
        unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };

        if result.is_some() {
            static LOGGED_OK: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !LOGGED_OK.swap(true, std::sync::atomic::Ordering::Relaxed) && self.verbose {
                eprintln!(
                    "[nvenc-dmabuf] zero-copy encode ok {src_width}x{src_height} stride={stride}"
                );
            }
        }

        result
    }

    pub fn codec_flag(&self) -> u8 {
        self.codec_flag
    }

    /// Encode from BGRA with edge-pixel padding for odd dimensions.
    pub fn encode_bgra_padded(
        &mut self,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let t0 = std::time::Instant::now();
        let enc_w = self.width as usize;
        let enc_h = self.height as usize;
        let pitch = self.cuda_pitch as usize; // aligned pitch from cuMemAllocPitch
        let frame_bytes = pitch * enc_h;

        // Write directly into the pinned staging buffer — avoids an extra
        // memcpy through a temporary Vec.  Pinned memory is regular RAM
        // that the CUDA driver has page-locked for fast DMA.
        //
        // The pinned buffer uses the pitch-aligned stride so the layout
        // matches the device allocation exactly.
        assert!(frame_bytes <= self.pinned_size);
        let dst = self.pinned_host;

        // Always write row-by-row because the destination pitch (aligned)
        // will generally differ from the source stride (width * 4).
        let src_row_bytes = src_w * 4;
        let copy_bytes = (enc_w.min(src_w)) * 4;
        for row in 0..enc_h {
            let sr = row.min(src_h.saturating_sub(1));
            let src_start = sr * src_row_bytes;
            let dst_off = row * pitch;
            unsafe {
                ptr::copy_nonoverlapping(
                    bgra.as_ptr().add(src_start),
                    dst.add(dst_off),
                    copy_bytes,
                );
            }
            // If the encoder width exceeds the source, replicate the last
            // source pixel across padding columns.
            if enc_w > src_w {
                let last = unsafe {
                    std::slice::from_raw_parts(bgra.as_ptr().add(src_start + (src_w - 1) * 4), 4)
                };
                for col in src_w..enc_w {
                    let off = dst_off + col * 4;
                    unsafe { ptr::copy_nonoverlapping(last.as_ptr(), dst.add(off), 4) };
                }
            }
            // Zero any trailing padding bytes between enc_w*4 and pitch.
            let used = enc_w * 4;
            if used < pitch {
                unsafe { ptr::write_bytes(dst.add(dst_off + used), 0, pitch - used) };
            }
        }

        let t_write = t0.elapsed();

        // --- Single CUDA context scope for upload + encode ---
        // Keeping the context pushed through both the DMA transfer and the
        // NVENC encode ensures the video engine sees the completed writes.
        let result = self.upload_and_encode(
            self.cuda_devptr,
            self.cuda_registered,
            NV_ENC_BUFFER_FORMAT_ARGB,
            frame_bytes,
        );

        let t_total = t0.elapsed();
        if t_total.as_millis() > 50 && self.verbose {
            eprintln!(
                "[nvenc-timing] {}x{} (src {}x{}) write={:.1}ms encode={:.1}ms total={:.1}ms",
                self.width,
                self.height,
                src_w,
                src_h,
                t_write.as_secs_f64() * 1000.0,
                (t_total - t_write).as_secs_f64() * 1000.0,
                t_total.as_secs_f64() * 1000.0,
            );
        }
        result
    }

    /// Unified upload → sync → encode pipeline.
    ///
    /// Keeps the CUDA context pushed through the entire sequence so that
    /// the NVENC video engine is guaranteed to see the completed DMA
    /// transfer.  All error paths clean up properly (unmap, pop context).
    fn upload_and_encode(
        &mut self,
        devptr: gpu_libs::CUdeviceptr,
        registered: *mut c_void,
        buf_fmt: u32,
        upload_bytes: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let pitch = self.cuda_pitch;
        let cuda = crate::gpu_libs::cuda().expect("CUDA loaded during init");

        // Push CUDA context — stays active until the very end.
        unsafe { (cuda.cuCtxPushCurrent_v2)(self.cuda_ctx) };

        // Upload pinned host → device.
        let status = unsafe {
            (cuda.cuMemcpyHtoD_v2)(devptr, self.pinned_host as *const c_void, upload_bytes)
        };
        if status != 0 {
            eprintln!("[nvenc-direct] cuMemcpyHtoD failed: {status}");
            let mut dummy: gpu_libs::CUcontext = ptr::null_mut();
            unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };
            return None;
        }

        // Belt-and-suspenders: drain the default stream so the DMA is
        // fully complete before NVENC's video engine reads the buffer.
        unsafe { (cuda.cuStreamSynchronize)(ptr::null_mut()) };

        // Map the registered CUDA resource for NVENC input.
        let mut map_buf = vec![0u8; NVENC_MAP_INPUT_RESOURCE_SIZE];
        w32(&mut map_buf, 0, NV_ENC_MAP_INPUT_RESOURCE_VER);
        wptr(&mut map_buf, 16, registered);

        let status = unsafe {
            (self.fns.nvEncMapInputResource)(self.encoder, map_buf.as_mut_ptr() as *mut c_void)
        };
        if status != NV_ENC_SUCCESS {
            eprintln!("[nvenc-direct] nvEncMapInputResource failed: {status}");
            let mut dummy: gpu_libs::CUcontext = ptr::null_mut();
            unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };
            return None;
        }
        let mapped_resource = rptr(&map_buf, 24);

        // NV_ENC_PIC_PARAMS offsets (from nv-codec-headers 12.1):
        //   version=0, inputWidth=4, inputHeight=8, inputPitch=12,
        //   encodePicFlags=16, frameIdx=20, inputTimestamp=24(u64),
        //   inputBuffer=40(ptr), outputBitstream=48(ptr),
        //   bufferFmt=64, pictureStruct=68, pictureType=72
        let mut pic_buf = vec![0u8; NVENC_PIC_PARAMS_SIZE];
        w32(&mut pic_buf, 0, NV_ENC_PIC_PARAMS_VER);
        w32(&mut pic_buf, 4, self.width);
        w32(&mut pic_buf, 8, self.height);
        w32(&mut pic_buf, 12, pitch);
        w32(&mut pic_buf, 20, self.frame_idx);
        w64(&mut pic_buf, 24, self.frame_idx as u64);
        wptr(&mut pic_buf, 40, mapped_resource);
        wptr(&mut pic_buf, 48, self.output_buffer);
        w32(&mut pic_buf, 64, buf_fmt);
        w32(&mut pic_buf, 68, 1); // NV_ENC_PIC_STRUCT_FRAME

        if self.force_idr {
            w32(&mut pic_buf, 16, NV_ENC_PIC_FLAGS_FORCEIDR | 0x4);
            w32(&mut pic_buf, 72, NV_ENC_PIC_TYPE_IDR);
        }

        self.frame_idx += 1;

        let status = unsafe {
            (self.fns.nvEncEncodePicture)(self.encoder, pic_buf.as_mut_ptr() as *mut c_void)
        };

        // On any encode failure, clean up mapped resource and context.
        if status != NV_ENC_SUCCESS {
            unsafe { (self.fns.nvEncUnmapInputResource)(self.encoder, mapped_resource) };
            let mut dummy: gpu_libs::CUcontext = ptr::null_mut();
            unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };
            if status != NV_ENC_ERR_NEED_MORE_INPUT {
                eprintln!("[nvenc-direct] nvEncEncodePicture failed: {status}");
            }
            // force_idr stays true — next call retries.
            return None;
        }
        // Encode succeeded — safe to clear the IDR request.
        self.force_idr = false;

        // Lock and read bitstream.
        let mut lock_buf = vec![0u8; NVENC_LOCK_BITSTREAM_SIZE];
        w32(&mut lock_buf, 0, NV_ENC_LOCK_BITSTREAM_VER);
        wptr(&mut lock_buf, 8, self.output_buffer);

        let status = unsafe {
            (self.fns.nvEncLockBitstream)(self.encoder, lock_buf.as_mut_ptr() as *mut c_void)
        };
        if status != NV_ENC_SUCCESS {
            eprintln!("[nvenc-direct] nvEncLockBitstream failed: {status}");
            unsafe { (self.fns.nvEncUnmapInputResource)(self.encoder, mapped_resource) };
            let mut dummy: gpu_libs::CUcontext = ptr::null_mut();
            unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };
            return None;
        }

        let size = r32(&lock_buf, 36) as usize;
        let buf_ptr = rptr(&lock_buf, 56) as *const u8;
        let nal_data = if !buf_ptr.is_null() && size > 0 {
            unsafe { std::slice::from_raw_parts(buf_ptr, size) }.to_vec()
        } else {
            Vec::new()
        };

        let is_idr = self.is_keyframe_pic_type(r32(&lock_buf, 64));

        unsafe { (self.fns.nvEncUnlockBitstream)(self.encoder, self.output_buffer) };

        // Unmap the CUDA input resource.
        unsafe { (self.fns.nvEncUnmapInputResource)(self.encoder, mapped_resource) };

        // Pop CUDA context.
        let mut dummy_ctx: gpu_libs::CUcontext = ptr::null_mut();
        unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy_ctx) };

        if nal_data.is_empty() {
            None
        } else {
            let mut nal_data = nal_data;
            self.ensure_h264_sps_pps(&mut nal_data, is_idr);
            Some((nal_data, is_idr))
        }
    }

    // -----------------------------------------------------------------------
    // RGBA (ABGR) path — avoids CPU R/B swap
    // -----------------------------------------------------------------------

    /// Encode from RGBA with edge-pixel padding for odd dimensions.
    /// Uploads to the ABGR-registered CUDA buffer so NVENC does the
    /// RGBA→YUV conversion on the GPU — no CPU colorspace conversion.
    pub fn encode_rgba_padded(
        &mut self,
        rgba: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let enc_w = self.width as usize;
        let enc_h = self.height as usize;
        let pitch = self.cuda_pitch as usize;
        let frame_bytes = pitch * enc_h;

        assert!(frame_bytes <= self.pinned_size);
        let dst = self.pinned_host;

        let src_row_bytes = src_w * 4;
        let copy_bytes = (enc_w.min(src_w)) * 4;
        for row in 0..enc_h {
            let sr = row.min(src_h.saturating_sub(1));
            let src_start = sr * src_row_bytes;
            let dst_off = row * pitch;
            unsafe {
                ptr::copy_nonoverlapping(
                    rgba.as_ptr().add(src_start),
                    dst.add(dst_off),
                    copy_bytes,
                );
            }
            if enc_w > src_w {
                let last = unsafe {
                    std::slice::from_raw_parts(rgba.as_ptr().add(src_start + (src_w - 1) * 4), 4)
                };
                for col in src_w..enc_w {
                    let off = dst_off + col * 4;
                    unsafe { ptr::copy_nonoverlapping(last.as_ptr(), dst.add(off), 4) };
                }
            }
            let used = enc_w * 4;
            if used < pitch {
                unsafe { ptr::write_bytes(dst.add(dst_off + used), 0, pitch - used) };
            }
        }

        self.upload_and_encode(
            self.cuda_devptr_abgr,
            self.cuda_registered_abgr,
            NV_ENC_BUFFER_FORMAT_ABGR,
            frame_bytes,
        )
    }

    // -----------------------------------------------------------------------
    // NV12 path — avoids NV12→RGBA→BGRA CPU conversion
    // -----------------------------------------------------------------------

    /// Encode from NV12 data directly.  Uploads Y+UV to the NV12-registered
    /// CUDA buffer so NVENC reads it natively — no colorspace conversion.
    ///
    /// `data` is contiguous: Y plane at [0..y_stride*src_h], UV at
    /// [y_stride*src_h..].  `y_stride` / `uv_stride` are source pitches.
    /// `src_h` is the original surface height before any encoder padding.
    pub fn encode_nv12(
        &mut self,
        data: &[u8],
        y_stride: usize,
        uv_stride: usize,
        src_h: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let enc_w = self.width as usize;
        let enc_h = self.height as usize;
        let nv12_pitch = self.nv12_pitch as usize;
        let y_plane_size = nv12_pitch * enc_h;
        let uv_h = enc_h / 2;
        let nv12_total = y_plane_size + nv12_pitch * uv_h;

        // Pack into pinned host memory with encoder pitch (strip source padding).
        assert!(nv12_total <= self.pinned_size);
        let dst = self.pinned_host;

        // Y plane — copy row by row to strip source stride padding.
        for row in 0..enc_h {
            let sr = row.min(src_h.saturating_sub(1));
            let src_off = sr * y_stride;
            let dst_off = row * nv12_pitch;
            let copy_len = enc_w.min(y_stride);
            if src_off + copy_len <= data.len() {
                unsafe {
                    ptr::copy_nonoverlapping(
                        data.as_ptr().add(src_off),
                        dst.add(dst_off),
                        copy_len,
                    );
                }
            }
            // Zero padding bytes between Y data and pitch.
            if enc_w < nv12_pitch {
                unsafe { ptr::write_bytes(dst.add(dst_off + enc_w), 0, nv12_pitch - enc_w) };
            }
        }

        // UV plane — interleaved U/V, same width as Y, half height.
        let src_uv_h = src_h / 2;
        let uv_src_base = y_stride * src_h;
        for row in 0..uv_h {
            let sr = row.min(src_uv_h.saturating_sub(1));
            let src_off = uv_src_base + sr * uv_stride;
            let dst_off = y_plane_size + row * nv12_pitch;
            let copy_len = enc_w.min(uv_stride);
            if src_off + copy_len <= data.len() {
                unsafe {
                    ptr::copy_nonoverlapping(
                        data.as_ptr().add(src_off),
                        dst.add(dst_off),
                        copy_len,
                    );
                }
            }
            if enc_w < nv12_pitch {
                unsafe { ptr::write_bytes(dst.add(dst_off + enc_w), 0, nv12_pitch - enc_w) };
            }
        }

        self.upload_and_encode_nv12(nv12_total)
    }

    /// NV12-specific upload+encode.  Uses nv12_pitch for the encode params
    /// since NV12 has a different pitch from the RGBA buffers.
    fn upload_and_encode_nv12(&mut self, upload_bytes: usize) -> Option<(Vec<u8>, bool)> {
        let pitch = self.nv12_pitch;
        let cuda = crate::gpu_libs::cuda().expect("CUDA loaded during init");

        unsafe { (cuda.cuCtxPushCurrent_v2)(self.cuda_ctx) };

        let status = unsafe {
            (cuda.cuMemcpyHtoD_v2)(
                self.cuda_devptr_nv12,
                self.pinned_host as *const c_void,
                upload_bytes,
            )
        };
        if status != 0 {
            eprintln!("[nvenc-direct] cuMemcpyHtoD (NV12) failed: {status}");
            let mut dummy: gpu_libs::CUcontext = ptr::null_mut();
            unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };
            return None;
        }

        unsafe { (cuda.cuStreamSynchronize)(ptr::null_mut()) };

        // Map the registered NV12 resource.
        let mut map_buf = vec![0u8; NVENC_MAP_INPUT_RESOURCE_SIZE];
        w32(&mut map_buf, 0, NV_ENC_MAP_INPUT_RESOURCE_VER);
        wptr(&mut map_buf, 16, self.cuda_registered_nv12);

        let status = unsafe {
            (self.fns.nvEncMapInputResource)(self.encoder, map_buf.as_mut_ptr() as *mut c_void)
        };
        if status != NV_ENC_SUCCESS {
            eprintln!("[nvenc-direct] nvEncMapInputResource (NV12) failed: {status}");
            let mut dummy: gpu_libs::CUcontext = ptr::null_mut();
            unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };
            return None;
        }
        let mapped_resource = rptr(&map_buf, 24);

        let mut pic_buf = vec![0u8; NVENC_PIC_PARAMS_SIZE];
        w32(&mut pic_buf, 0, NV_ENC_PIC_PARAMS_VER);
        w32(&mut pic_buf, 4, self.width);
        w32(&mut pic_buf, 8, self.height);
        w32(&mut pic_buf, 12, pitch);
        w32(&mut pic_buf, 20, self.frame_idx);
        w64(&mut pic_buf, 24, self.frame_idx as u64);
        wptr(&mut pic_buf, 40, mapped_resource);
        wptr(&mut pic_buf, 48, self.output_buffer);
        w32(&mut pic_buf, 64, NV_ENC_BUFFER_FORMAT_NV12);
        w32(&mut pic_buf, 68, 1);

        if self.force_idr {
            w32(&mut pic_buf, 16, NV_ENC_PIC_FLAGS_FORCEIDR | 0x4);
            w32(&mut pic_buf, 72, NV_ENC_PIC_TYPE_IDR);
        }

        self.frame_idx += 1;

        let status = unsafe {
            (self.fns.nvEncEncodePicture)(self.encoder, pic_buf.as_mut_ptr() as *mut c_void)
        };
        if status != NV_ENC_SUCCESS {
            unsafe { (self.fns.nvEncUnmapInputResource)(self.encoder, mapped_resource) };
            let mut dummy: gpu_libs::CUcontext = ptr::null_mut();
            unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };
            if status != NV_ENC_ERR_NEED_MORE_INPUT {
                eprintln!("[nvenc-direct] nvEncEncodePicture (NV12) failed: {status}");
            }
            return None;
        }
        self.force_idr = false;

        let mut lock_buf = vec![0u8; NVENC_LOCK_BITSTREAM_SIZE];
        w32(&mut lock_buf, 0, NV_ENC_LOCK_BITSTREAM_VER);
        wptr(&mut lock_buf, 8, self.output_buffer);

        let status = unsafe {
            (self.fns.nvEncLockBitstream)(self.encoder, lock_buf.as_mut_ptr() as *mut c_void)
        };
        if status != NV_ENC_SUCCESS {
            eprintln!("[nvenc-direct] nvEncLockBitstream (NV12) failed: {status}");
            unsafe { (self.fns.nvEncUnmapInputResource)(self.encoder, mapped_resource) };
            let mut dummy: gpu_libs::CUcontext = ptr::null_mut();
            unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy) };
            return None;
        }

        let size = r32(&lock_buf, 36) as usize;
        let buf_ptr = rptr(&lock_buf, 56) as *const u8;
        let nal_data = if !buf_ptr.is_null() && size > 0 {
            unsafe { std::slice::from_raw_parts(buf_ptr, size) }.to_vec()
        } else {
            Vec::new()
        };
        let is_idr = self.is_keyframe_pic_type(r32(&lock_buf, 64));

        unsafe { (self.fns.nvEncUnlockBitstream)(self.encoder, self.output_buffer) };
        unsafe { (self.fns.nvEncUnmapInputResource)(self.encoder, mapped_resource) };

        let mut dummy_ctx: gpu_libs::CUcontext = ptr::null_mut();
        unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy_ctx) };

        if nal_data.is_empty() {
            None
        } else {
            let mut nal_data = nal_data;
            self.ensure_h264_sps_pps(&mut nal_data, is_idr);
            Some((nal_data, is_idr))
        }
    }
}

/// Check if an Annex B H.264 bitstream contains SPS (NAL type 7) and PPS (NAL type 8).
fn h264_has_sps_pps(data: &[u8]) -> bool {
    let mut has_sps = false;
    let mut has_pps = false;
    for_each_annex_b_nal(data, |nal_type, _offset| {
        if nal_type == 7 {
            has_sps = true;
        }
        if nal_type == 8 {
            has_pps = true;
        }
    });
    has_sps && has_pps
}

/// Extract the Annex B prefix containing SPS+PPS NAL units (everything
/// before the first VCL NAL, i.e. IDR slice type 5).
fn h264_extract_sps_pps_prefix(data: &[u8]) -> Option<Vec<u8>> {
    let mut first_vcl_offset = None;
    for_each_annex_b_nal(data, |nal_type, offset| {
        if first_vcl_offset.is_none() && (nal_type == 5 || nal_type == 1) {
            first_vcl_offset = Some(offset);
        }
    });
    first_vcl_offset
        .filter(|&off| off > 0)
        .map(|off| data[..off].to_vec())
}

/// Iterate over NAL units in an Annex B byte stream, calling `f` with the
/// NAL unit type and byte offset of each start code.
fn for_each_annex_b_nal(data: &[u8], mut f: impl FnMut(u8, usize)) {
    let len = data.len();
    let mut i = 0;
    while i < len.saturating_sub(3) {
        if data[i] == 0 && data[i + 1] == 0 {
            let (sc_len, nal_start) = if data[i + 2] == 1 {
                (3, i + 3)
            } else if data[i + 2] == 0 && i + 3 < len && data[i + 3] == 1 {
                (4, i + 4)
            } else {
                i += 1;
                continue;
            };
            let _ = sc_len;
            if nal_start < len {
                let nal_type = data[nal_start] & 0x1f;
                f(nal_type, i);
            }
            i = nal_start + 1;
        } else {
            i += 1;
        }
    }
}

impl Drop for NvencDirectEncoder {
    fn drop(&mut self) {
        unsafe {
            // Push the CUDA context — Drop may run on any thread.
            if let Ok(cuda) = gpu_libs::cuda() {
                (cuda.cuCtxPushCurrent_v2)(self.cuda_ctx);
            }
            if !self.cuda_registered.is_null() {
                (self.fns.nvEncUnregisterResource)(self.encoder, self.cuda_registered);
            }
            if !self.cuda_registered_abgr.is_null() {
                (self.fns.nvEncUnregisterResource)(self.encoder, self.cuda_registered_abgr);
            }
            if !self.cuda_registered_nv12.is_null() {
                (self.fns.nvEncUnregisterResource)(self.encoder, self.cuda_registered_nv12);
            }
            (self.fns.nvEncDestroyInputBuffer)(self.encoder, self.input_buffer);
            (self.fns.nvEncDestroyBitstreamBuffer)(self.encoder, self.output_buffer);
            (self.fns.nvEncDestroyEncoder)(self.encoder);
            if let Ok(cuda) = gpu_libs::cuda() {
                if !self.pinned_host.is_null() {
                    (cuda.cuMemFreeHost)(self.pinned_host as *mut c_void);
                }
                if self.cuda_devptr != 0 {
                    (cuda.cuMemFree_v2)(self.cuda_devptr);
                }
                if self.cuda_devptr_abgr != 0 {
                    (cuda.cuMemFree_v2)(self.cuda_devptr_abgr);
                }
                if self.cuda_devptr_nv12 != 0 {
                    (cuda.cuMemFree_v2)(self.cuda_devptr_nv12);
                }
                (cuda.cuCtxDestroy_v2)(self.cuda_ctx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> NvencCaps {
        // What an Ada-generation AV1 engine reports.
        NvencCaps {
            min_width: 160,
            min_height: 128,
            max_width: 8192,
            max_height: 8192,
            yuv444: false,
        }
    }

    /// The dock renders panes at 256x54.  That is a statement about the
    /// frame, and the caller has to be able to read it as one — writing it
    /// off as "this host has no NVENC" is what took hardware AV1 away from
    /// every viewer on the machine.
    #[test]
    fn a_frame_under_the_engine_minimum_is_refused_not_the_engine() {
        assert!(caps().refuse(256, 54).is_some(), "under min_height");
        assert!(caps().refuse(64, 480).is_some(), "under min_width");
        assert!(caps().refuse(9000, 480).is_some(), "over max_width");
        assert!(caps().refuse(1920, 9000).is_some(), "over max_height");
        assert!(caps().refuse(1920, 1080).is_none());
        // Exactly on the bounds is inside them.
        assert!(caps().refuse(160, 128).is_none());
        assert!(caps().refuse(8192, 8192).is_none());
    }

    /// The probe frame every backend is measured against has to clear the
    /// minimums, or the thing meant to tell a host's fault from a frame's
    /// would report every host as broken.
    #[test]
    fn the_probe_frame_clears_the_engine_minimum() {
        let (w, h) = crate::surface_encoder::PROBE_SIZE;
        assert!(caps().refuse(w, h).is_none());
    }

    /// The refusal names the range, because it is read in logs beside the
    /// size that was asked for.
    #[test]
    fn a_refusal_says_what_the_range_is() {
        let msg = caps().refuse(256, 54).unwrap();
        assert!(msg.contains("256x54"), "{msg}");
        assert!(msg.contains("160x128"), "{msg}");
    }
}
