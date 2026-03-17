//! Direct NVENC encoder — no ffmpeg dependency.
//!
//! Uses the NVIDIA Video Codec SDK via `dlopen("libnvidia-encode.so")`.
//! The CUDA context is created via `dlopen("libcuda.so")`.
//!
//! The encoder accepts BGRA input directly (`NV_ENC_BUFFER_FORMAT_ARGB`),
//! so no CPU-side colorspace conversion is needed.  NVENC handles the
//! BGRA→YUV conversion internally on the GPU.

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code
)]

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
const NV_ENC_BUFFER_FORMAT_ARGB: u32 = 0x01000000;
const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x00000001;
const NV_ENC_BUFFER_FORMAT_ABGR: u32 = 0x10000000;

// Resource types for nvEncRegisterResource
const NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR: u32 = 0x01;
const NV_ENC_REGISTER_RESOURCE_VER: u32 = nvencapi_struct_version(4);
const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = nvencapi_struct_version(4);

// NV_ENC_REGISTER_RESOURCE struct size (must cover all fields + reserved[245] + reserved2[61])
const NVENC_REGISTER_RESOURCE_SIZE: usize = 2048;
// NV_ENC_MAP_INPUT_RESOURCE struct size (includes reserved fields)
const NVENC_MAP_INPUT_RESOURCE_SIZE: usize = 2048;

// Codec GUIDs (H.264 and H.265)
const NV_ENC_CODEC_H264_GUID: NvGuid = NvGuid(
    0x6BC82762,
    0x4E63,
    0x4CA4,
    [0xAA, 0x85, 0x1E, 0x50, 0xF3, 0x21, 0xF6, 0xBF],
);
const NV_ENC_CODEC_HEVC_GUID: NvGuid = NvGuid(
    0x790CDC88,
    0x4522,
    0x4D7B,
    [0x94, 0x25, 0xBD, 0xA9, 0x97, 0x5F, 0x76, 0x03],
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

// Tuning info
const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 1;

// Picture types
const NV_ENC_PIC_TYPE_IDR: u32 = 4;
const NV_ENC_PIC_FLAGS_FORCEIDR: u32 = 4;

// Rate control modes
const NV_ENC_PARAMS_RC_CONSTQP: u32 = 0;

// ---------------------------------------------------------------------------
// NVENC API types
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct NvGuid(u32, u16, u16, [u8; 8]);

impl NvGuid {
    const fn zeroed() -> Self {
        Self(0, 0, 0, [0; 8])
    }
}

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
const NVENC_LOCK_INPUT_BUFFER_SIZE: usize = 1544;

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

// -- Lock bitstream --
#[repr(C)]
struct NvEncLockBitstream {
    version: u32,
    do_not_wait: u32,
    output_bitstream: *mut c_void,
    _reserved: [*mut c_void; 2],
    bitstream_buffer_ptr: *mut c_void,
    bitstream_size_in_bytes: u32,
    pic_type: u32,
    pic_idx: u32,
    frame_avg_qp: u32,
    frame_idx: u32,
    hwenc_pic_type: u32,
    // ... lots more fields we don't read
    _rest: [u8; 1024],
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
    cuda_devptr: gpu_libs::CUdeviceptr,
    cuda_registered: *mut c_void, // NV_ENC registered resource handle
    cuda_pitch: u32,              // pitch in bytes (width * 4 for ARGB)
    pinned_host: *mut u8,         // page-locked staging buffer
    pinned_size: usize,
}

// NVENC encoder handle and CUDA context are thread-safe with proper push/pop.
unsafe impl Send for NvencDirectEncoder {}

impl NvencDirectEncoder {
    /// Try to create an NVENC encoder for the given codec and dimensions.
    ///
    /// `codec` should be `"h264"`, `"h265"`, or `"av1"`.
    pub fn try_new(codec: &str, width: u32, height: u32) -> Result<Self, String> {
        let cuda = gpu_libs::cuda().ok_or("libcuda.so not found")?;
        let nvenc_fns = gpu_libs::nvenc().ok_or("libnvidia-encode.so not found")?;

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
            "h265" => (
                NV_ENC_CODEC_HEVC_GUID,
                blit_remote::SURFACE_FRAME_CODEC_H265,
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
        // rcParams.rateControlMode @ 44, rcParams.constQP @ 48 (3 × u32)
        w32(&mut config_buf, 44, NV_ENC_PARAMS_RC_CONSTQP);
        w32(&mut config_buf, 48, 23); // qp_inter_p
        w32(&mut config_buf, 52, 23); // qp_inter_b
        w32(&mut config_buf, 56, 23); // qp_intra

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
        let cuda_pitch = width * 4; // ARGB: 4 bytes per pixel
        let frame_size = (cuda_pitch * height) as usize;
        let mut cuda_devptr: gpu_libs::CUdeviceptr = 0;
        status = unsafe { (cuda.cuMemAlloc_v2)(&mut cuda_devptr, frame_size) };
        if status != 0 {
            return Err(format!("cuMemAlloc failed: {status}"));
        }

        // Allocate page-locked (pinned) host memory for staging.
        // cuMemcpyHtoD from pinned memory uses DMA at full PCIe bandwidth;
        // from pageable memory the driver must pin pages on every call (~60ms
        // overhead at 1920×1080).
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

        eprintln!("[nvenc-direct] initialized {codec} encoder for {width}x{height} (CUDA upload)");

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
        })
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    pub fn codec_flag(&self) -> u8 {
        self.codec_flag
    }

    /// Encode a BGRA frame.  The input buffer is `width * height * 4` bytes,
    /// BGRA pixel order (DRM ARGB8888 layout on little-endian).
    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Option<(Vec<u8>, bool)> {
        self.write_input_bgra(bgra)?;
        self.submit_and_read()
    }

    /// Encode from BGRA with edge-pixel padding for odd dimensions.
    pub fn encode_bgra_padded(
        &mut self,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let t0 = std::time::Instant::now();
        self.write_input_bgra_padded(bgra, src_w, src_h)?;
        let t_write = t0.elapsed();
        let result = self.submit_and_read();
        let t_total = t0.elapsed();
        if t_total.as_millis() > 50 {
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

    fn write_input_bgra(&mut self, bgra: &[u8]) -> Option<()> {
        let mut lock_buf = vec![0u8; NVENC_LOCK_INPUT_BUFFER_SIZE];
        w32(&mut lock_buf, 0, nvencapi_struct_version(1)); // version @ 0
        wptr(&mut lock_buf, 8, self.input_buffer); // inputBuffer @ 8

        let status = unsafe {
            (self.fns.nvEncLockInputBuffer)(self.encoder, lock_buf.as_mut_ptr() as *mut c_void)
        };
        if status != NV_ENC_SUCCESS {
            return None;
        }

        let dst = rptr(&lock_buf, 16) as *mut u8; // bufferDataPtr @ 16
        let pitch = r32(&lock_buf, 24) as usize; // pitch @ 24
        let w = self.width as usize;
        let h = self.height as usize;
        let row_bytes = w * 4;

        unsafe {
            if pitch == row_bytes && bgra.len() >= row_bytes * h {
                // Fast path: contiguous copy
                ptr::copy_nonoverlapping(bgra.as_ptr(), dst, row_bytes * h);
            } else {
                // Row-by-row with pitch
                for row in 0..h {
                    let src_start = row * row_bytes;
                    let dst_start = row * pitch;
                    if src_start + row_bytes <= bgra.len() {
                        ptr::copy_nonoverlapping(
                            bgra.as_ptr().add(src_start),
                            dst.add(dst_start),
                            row_bytes,
                        );
                    }
                }
            }
        }

        unsafe { (self.fns.nvEncUnlockInputBuffer)(self.encoder, self.input_buffer) };
        Some(())
    }

    fn write_input_bgra_padded(&mut self, bgra: &[u8], src_w: usize, src_h: usize) -> Option<()> {
        let enc_w = self.width as usize;
        let enc_h = self.height as usize;
        let row_bytes = enc_w * 4;
        let frame_bytes = row_bytes * enc_h;

        // Write directly into the pinned staging buffer — avoids an extra
        // memcpy through a temporary Vec.  Pinned memory is regular RAM
        // that the CUDA driver has page-locked for fast DMA.
        assert!(frame_bytes <= self.pinned_size);
        let dst = self.pinned_host;

        if src_w == enc_w && src_h == enc_h && bgra.len() >= frame_bytes {
            // Fast path: no padding, single bulk copy into pinned buffer.
            unsafe { ptr::copy_nonoverlapping(bgra.as_ptr(), dst, frame_bytes) };
        } else {
            // Pad directly into pinned buffer.
            let copy_w = enc_w.min(src_w);
            for row in 0..enc_h {
                let sr = row.min(src_h - 1);
                let src_start = sr * src_w * 4;
                let dst_off = row * row_bytes;
                unsafe {
                    ptr::copy_nonoverlapping(
                        bgra.as_ptr().add(src_start),
                        dst.add(dst_off),
                        copy_w * 4,
                    );
                }
                if enc_w > src_w {
                    // Replicate last source pixel across padding columns.
                    let last = unsafe {
                        std::slice::from_raw_parts(
                            bgra.as_ptr().add(src_start + (src_w - 1) * 4),
                            4,
                        )
                    };
                    for col in src_w..enc_w {
                        let off = dst_off + col * 4;
                        unsafe { ptr::copy_nonoverlapping(last.as_ptr(), dst.add(off), 4) };
                    }
                }
            }
        }

        let cuda = crate::gpu_libs::cuda().unwrap();
        unsafe { (cuda.cuCtxPushCurrent_v2)(self.cuda_ctx) };
        let status = unsafe {
            (cuda.cuMemcpyHtoD_v2)(
                self.cuda_devptr,
                self.pinned_host as *const c_void,
                frame_bytes,
            )
        };
        let mut dummy_ctx: gpu_libs::CUcontext = ptr::null_mut();
        unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy_ctx) };
        if status != 0 {
            eprintln!("[nvenc-direct] cuMemcpyHtoD failed: {status}");
            return None;
        }
        Some(())
    }

    fn submit_and_read(&mut self) -> Option<(Vec<u8>, bool)> {
        // Ensure the CUDA context is current on this thread (encode runs
        // on a tokio blocking thread, not the thread that created the context).
        let cuda = crate::gpu_libs::cuda().unwrap();
        unsafe { (cuda.cuCtxPushCurrent_v2)(self.cuda_ctx) };

        // Map the registered CUDA resource for NVENC input.
        // NV_ENC_MAP_INPUT_RESOURCE offsets (nv-codec-headers 12.1):
        //   version=0, subResourceIndex=4, inputResource=8(ptr, DEPRECATED),
        //   registeredResource=16(ptr), mappedResource=24(ptr, out),
        //   mappedBufferFmt=32(u32, out)
        let mut map_buf = vec![0u8; NVENC_MAP_INPUT_RESOURCE_SIZE];
        w32(&mut map_buf, 0, NV_ENC_MAP_INPUT_RESOURCE_VER);
        wptr(&mut map_buf, 16, self.cuda_registered); // registeredResource

        let status = unsafe {
            (self.fns.nvEncMapInputResource)(self.encoder, map_buf.as_mut_ptr() as *mut c_void)
        };
        if status != NV_ENC_SUCCESS {
            eprintln!("[nvenc-direct] nvEncMapInputResource failed: {status}");
            return None;
        }
        let mapped_resource = rptr(&map_buf, 24); // mappedResource @ 24

        // NV_ENC_PIC_PARAMS offsets (from nv-codec-headers 12.1):
        //   version=0, inputWidth=4, inputHeight=8, inputPitch=12,
        //   encodePicFlags=16, frameIdx=20, inputTimestamp=24(u64),
        //   inputBuffer=40(ptr), outputBitstream=48(ptr),
        //   bufferFmt=64, pictureStruct=68, pictureType=72
        let mut pic_buf = vec![0u8; NVENC_PIC_PARAMS_SIZE];
        w32(&mut pic_buf, 0, NV_ENC_PIC_PARAMS_VER);
        w32(&mut pic_buf, 4, self.width);
        w32(&mut pic_buf, 8, self.height);
        w32(&mut pic_buf, 12, self.cuda_pitch);
        w32(&mut pic_buf, 20, self.frame_idx);
        w64(&mut pic_buf, 24, self.frame_idx as u64);
        wptr(&mut pic_buf, 40, mapped_resource);
        wptr(&mut pic_buf, 48, self.output_buffer);
        w32(&mut pic_buf, 64, NV_ENC_BUFFER_FORMAT_ARGB);
        w32(&mut pic_buf, 68, 1); // NV_ENC_PIC_STRUCT_FRAME

        if self.force_idr {
            w32(&mut pic_buf, 16, NV_ENC_PIC_FLAGS_FORCEIDR);
            w32(&mut pic_buf, 72, NV_ENC_PIC_TYPE_IDR);
            self.force_idr = false;
        }

        self.frame_idx += 1;

        let status = unsafe {
            (self.fns.nvEncEncodePicture)(self.encoder, pic_buf.as_mut_ptr() as *mut c_void)
        };
        if status != NV_ENC_SUCCESS && status != NV_ENC_ERR_NEED_MORE_INPUT {
            eprintln!("[nvenc-direct] nvEncEncodePicture failed: {status}");
            return None;
        }
        if status == NV_ENC_ERR_NEED_MORE_INPUT {
            return None;
        }

        // Lock and read bitstream.
        // NV_ENC_LOCK_BITSTREAM offsets:
        //   version=0, outputBitstream=8(ptr),
        //   bitstreamSizeInBytes=36, bitstreamBufferPtr=56(ptr), pictureType=64
        let mut lock_buf = vec![0u8; NVENC_LOCK_BITSTREAM_SIZE];
        w32(&mut lock_buf, 0, NV_ENC_LOCK_BITSTREAM_VER);
        wptr(&mut lock_buf, 8, self.output_buffer);

        let status = unsafe {
            (self.fns.nvEncLockBitstream)(self.encoder, lock_buf.as_mut_ptr() as *mut c_void)
        };
        if status != NV_ENC_SUCCESS {
            eprintln!("[nvenc-direct] nvEncLockBitstream failed: {status}");
            return None;
        }

        let size = r32(&lock_buf, 36) as usize;
        let buf_ptr = rptr(&lock_buf, 56) as *const u8;
        let nal_data = if !buf_ptr.is_null() && size > 0 {
            unsafe { std::slice::from_raw_parts(buf_ptr, size) }.to_vec()
        } else {
            Vec::new()
        };

        let is_idr = r32(&lock_buf, 64) == NV_ENC_PIC_TYPE_IDR;

        unsafe { (self.fns.nvEncUnlockBitstream)(self.encoder, self.output_buffer) };

        // Unmap the CUDA input resource.
        unsafe {
            (self.fns.nvEncUnmapInputResource)(self.encoder, mapped_resource);
        }

        // Pop CUDA context.
        let mut dummy_ctx: gpu_libs::CUcontext = ptr::null_mut();
        unsafe { (cuda.cuCtxPopCurrent_v2)(&mut dummy_ctx) };

        if nal_data.is_empty() {
            None
        } else {
            Some((nal_data, is_idr))
        }
    }
}

impl Drop for NvencDirectEncoder {
    fn drop(&mut self) {
        unsafe {
            // Push the CUDA context — Drop may run on any thread.
            if let Some(cuda) = gpu_libs::cuda() {
                (cuda.cuCtxPushCurrent_v2)(self.cuda_ctx);
            }
            if !self.cuda_registered.is_null() {
                (self.fns.nvEncUnregisterResource)(self.encoder, self.cuda_registered);
            }
            (self.fns.nvEncDestroyInputBuffer)(self.encoder, self.input_buffer);
            (self.fns.nvEncDestroyBitstreamBuffer)(self.encoder, self.output_buffer);
            (self.fns.nvEncDestroyEncoder)(self.encoder);
            if let Some(cuda) = gpu_libs::cuda() {
                if !self.pinned_host.is_null() {
                    (cuda.cuMemFreeHost)(self.pinned_host as *mut c_void);
                }
                if self.cuda_devptr != 0 {
                    (cuda.cuMemFree_v2)(self.cuda_devptr);
                }
                (cuda.cuCtxDestroy_v2)(self.cuda_ctx);
            }
        }
    }
}
