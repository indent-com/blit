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
use std::ffi::c_void;
use std::ptr;

// ---------------------------------------------------------------------------
// NVENC API constants
// ---------------------------------------------------------------------------

const NV_ENC_SUCCESS: u32 = 0;
const NV_ENC_ERR_NEED_MORE_INPUT: u32 = 10;

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

// Buffer formats (from nv-codec-headers 12.1)
const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x00000001;
const NV_ENC_BUFFER_FORMAT_ARGB: u32 = 0x01000000; // B8G8R8A8 in memory (DRM ARGB8888)
const NV_ENC_BUFFER_FORMAT_ABGR: u32 = 0x10000000; // R8G8B8A8 in memory (DRM ABGR8888)

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

// Preset GUIDs
const NV_ENC_PRESET_P1_GUID: NvGuid = NvGuid(
    0xFC0A8D3E,
    0x45F8,
    0x4CF8,
    [0x80, 0xC7, 0x29, 0x88, 0x71, 0x59, 0x0E, 0xBF],
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
    nvEncGetEncodeGUIDs: *const c_void,
    nvEncGetEncodeProfileGUIDCount: *const c_void,
    nvEncGetEncodeProfileGUIDs: *const c_void,
    nvEncGetInputFormatCount: *const c_void,
    nvEncGetInputFormats: *const c_void,
    nvEncGetEncodeCaps: *const c_void,
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
    nvEncReconfigureEncoder: *const c_void,
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
    nvEncLookaheadPicture: *const c_void,
    // Padding for future SDK additions (13.x+).  NvEncodeAPICreateInstance
    // fills function pointers into this struct; if the driver knows more
    // entries than we declared it writes past our last field.  64 spare
    // slots should cover several major version bumps.
    _future: [*const c_void; 64],
}

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
const NVENC_CREATE_INPUT_BUFFER_SIZE: usize = 776;
const NVENC_CREATE_BITSTREAM_BUFFER_SIZE: usize = 776;
const NVENC_PIC_PARAMS_SIZE: usize = 3360;
const NVENC_LOCK_BITSTREAM_SIZE: usize = 1552;

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
}

// NVENC encoder handle and CUDA context are thread-safe with proper push/pop.
unsafe impl Send for NvencDirectEncoder {}

impl NvencDirectEncoder {
    /// Try to create an NVENC encoder for the given codec and dimensions.
    ///
    /// `codec` should be `"h264"` or `"av1"`.
    /// `qp` is the constant QP value (0–51 for H.264, 0–255 for AV1).
    pub fn try_new(
        codec: &str,
        width: u32,
        height: u32,
        qp: u32,
        verbose: bool,
    ) -> Result<Self, String> {
        let cuda = gpu_libs::cuda().map_err(|e| format!("CUDA: {e}"))?;
        let nvenc_fns = gpu_libs::nvenc().map_err(|e| format!("NVENC: {e}"))?;

        // Initialize CUDA
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

        // Get NVENC function table (initialized once, reused across all instances)
        static NVENC_FN_LIST: std::sync::OnceLock<Result<NvEncFunctionList, String>> =
            std::sync::OnceLock::new();
        let result = NVENC_FN_LIST.get_or_init(|| {
            let fn_list_ver = nvencapi_struct_version(2);
            let mut fl = std::mem::MaybeUninit::<NvEncFunctionList>::zeroed();
            // SAFETY: version is the first field (offset 0) in the repr(C) struct.
            unsafe { (*fl.as_mut_ptr()).version = fn_list_ver };
            let nv_status =
                unsafe { (nvenc_fns.NvEncodeAPICreateInstance)(fl.as_mut_ptr().cast()) };
            // SAFETY: NvEncodeAPICreateInstance fills all function pointers.
            let fl = unsafe { fl.assume_init() };
            if nv_status != NV_ENC_SUCCESS {
                return Err(format!("NvEncodeAPICreateInstance failed: {nv_status}"));
            }
            Ok(fl)
        });
        let fns = match result {
            Ok(fl) => fl,
            Err(e) => return Err(e.clone()),
        };
        let fns: &'static NvEncFunctionList =
            // SAFETY: OnceLock guarantees the value lives for 'static.
            unsafe { &*(fns as *const NvEncFunctionList) };

        // Open encode session
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
            return Err(format!("nvEncOpenEncodeSessionEx failed: {nv_status}"));
        }

        let (codec_guid, codec_flag) = match codec {
            "h264" => (
                NV_ENC_CODEC_H264_GUID,
                blit_remote::SURFACE_FRAME_CODEC_H264,
            ),
            "av1" => (NV_ENC_CODEC_AV1_GUID, blit_remote::SURFACE_FRAME_CODEC_AV1),
            _ => return Err(format!("unsupported NVENC codec: {codec}")),
        };

        // Get preset config — uses exact SDK struct sizes to avoid version
        // mismatch (the driver validates struct size via the version tag).
        let mut preset_buf = vec![0u8; NVENC_PRESET_CONFIG_SIZE];
        w32(&mut preset_buf, 0, NV_ENC_PRESET_CONFIG_VER); // version @ 0
        w32(&mut preset_buf, 8, NV_ENC_CONFIG_VER); // presetCfg.version @ 8

        let nv_status = unsafe {
            (fns.nvEncGetEncodePresetConfigEx)(
                encoder,
                codec_guid,
                NV_ENC_PRESET_P1_GUID,
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
        // frameFieldMode=32, mvPrecision=36).
        // rcParams.rateControlMode @ 40, rcParams.constQP @ 44 (3 × u32)
        w32(&mut config_buf, 40, NV_ENC_PARAMS_RC_CONSTQP);
        w32(&mut config_buf, 44, qp); // qp_inter_p
        w32(&mut config_buf, 48, qp); // qp_inter_b
        w32(&mut config_buf, 52, qp); // qp_intra

        // Initialize encoder
        let mut init_buf = vec![0u8; NVENC_INITIALIZE_PARAMS_SIZE];
        w32(&mut init_buf, 0, NV_ENC_INITIALIZE_PARAMS_VER);
        wguid(&mut init_buf, 4, codec_guid); // encodeGUID @ 4
        wguid(&mut init_buf, 20, NV_ENC_PRESET_P1_GUID); // presetGUID @ 20
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
        })
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
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

    /// Zero-copy encode from a DMA-BUF fd.  Imports the fd directly into
    /// CUDA device memory via `cuImportExternalMemory`, registers it with
    /// NVENC, and encodes — no CPU-side copies at all.
    ///
    /// Returns `None` if the CUDA driver doesn't support external memory
    /// import (pre-10.0) or if the import fails for this particular fd.
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
