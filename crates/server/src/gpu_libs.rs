//! Runtime GPU library loading via dlopen.
//!
//! All GPU driver libraries are loaded on first use.  If a library is
//! missing the corresponding encoder backend is simply unavailable —
//! the binary remains fully functional with software-only encoding.
//!
//! Release binaries are dynamically linked against musl libc so that
//! dlopen works, while all other dependencies are statically linked.
//! This avoids a build-time dependency on libva, libcuda,
//! libnvidia-encode, etc. — hardware acceleration is available when
//! the drivers are installed at runtime.

#![allow(non_camel_case_types, non_snake_case, dead_code)]

use std::ffi::{c_int, c_uint, c_void};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// dlopen helpers
// ---------------------------------------------------------------------------

pub(crate) struct DynLib {
    handle: *mut c_void,
}

unsafe impl Send for DynLib {}
unsafe impl Sync for DynLib {}

impl DynLib {
    #[cfg(unix)]
    pub(crate) fn open(names: &[&str]) -> Result<Self, String> {
        Self::open_flags(names, libc::RTLD_NOW | libc::RTLD_LOCAL)
    }

    #[cfg(unix)]
    fn open_flags(names: &[&str], flags: libc::c_int) -> Result<Self, String> {
        let mut last_err = String::new();
        for name in names {
            let Some(cname) = std::ffi::CString::new(*name).ok() else {
                continue;
            };
            let handle = unsafe { libc::dlopen(cname.as_ptr(), flags) };
            if !handle.is_null() {
                return Ok(Self { handle });
            }
            let err = unsafe { libc::dlerror() };
            if !err.is_null() {
                last_err = unsafe { std::ffi::CStr::from_ptr(err) }
                    .to_string_lossy()
                    .into_owned();
            }
        }
        Err(last_err)
    }

    #[cfg(not(unix))]
    pub(crate) fn open(_names: &[&str]) -> Result<Self, String> {
        Err("dlopen not available on this platform".into())
    }

    #[cfg(unix)]
    pub(crate) unsafe fn sym<T>(&self, name: &str) -> Result<T, String> {
        let cname =
            std::ffi::CString::new(name).map_err(|_| format!("invalid symbol name: {name}"))?;
        let ptr = unsafe { libc::dlsym(self.handle, cname.as_ptr()) };
        if ptr.is_null() {
            let err = unsafe { libc::dlerror() };
            let detail = if !err.is_null() {
                unsafe { std::ffi::CStr::from_ptr(err) }
                    .to_string_lossy()
                    .into_owned()
            } else {
                "symbol not found".into()
            };
            return Err(format!("{name}: {detail}"));
        }
        Ok(unsafe { std::mem::transmute_copy(&ptr) })
    }

    #[cfg(not(unix))]
    unsafe fn sym<T>(&self, _name: &str) -> Result<T, String> {
        Err("dlsym not available on this platform".into())
    }
}

impl Drop for DynLib {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::dlclose(self.handle);
        }
    }
}

// ---------------------------------------------------------------------------
// CUDA driver API
// ---------------------------------------------------------------------------

pub type CUresult = c_int;
pub type CUdevice = c_int;
pub type CUcontext = *mut c_void;
pub type CUdeviceptr = u64;

/// Opaque handle for imported external memory (CUDA 10.0+).
pub type CUexternalMemory = *mut c_void;

/// Opaque handle for an imported external semaphore (CUDA 10.0+).
pub type CUexternalSemaphore = *mut c_void;

/// Byte offsets into `CUDA_EXTERNAL_MEMORY_HANDLE_DESC` (cuda.h).
///
/// The `handle` union is sized by its largest member —
/// `struct { void *handle; const void *name; } win32`, 16 bytes — so it
/// spans 8..24 and `size` lands at 24, not at 16.  Getting this wrong
/// passes size=0, which `cuImportExternalMemory` rejects with
/// `CUDA_ERROR_INVALID_VALUE` for *every* handle type, making a plain
/// descriptor bug look like a driver limitation.  See the note on
/// `NvencDirectEncoder::encode_nv12_opaque_fd`.
pub mod cu_ext_mem_desc {
    /// `CUexternalMemoryHandleType type`
    pub const TYPE: usize = 0;
    /// `union { int fd; ... } handle` — `fd` is the first member.
    pub const FD: usize = 8;
    /// `unsigned long long size`
    pub const SIZE: usize = 24;
    /// `unsigned int flags`
    pub const FLAGS: usize = 32;
    /// Comfortably covers the ~104-byte struct including `reserved[16]`.
    pub const BYTES: usize = 128;
}

/// Byte offsets into `CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC` (cuda.h).
///
/// Same union as above, but the struct has no `size`, so `flags` sits at 24.
pub mod cu_ext_sem_desc {
    /// `CUexternalSemaphoreHandleType type`
    pub const TYPE: usize = 0;
    /// `union { int fd; ... } handle`
    pub const FD: usize = 8;
    /// `unsigned int flags`
    pub const FLAGS: usize = 24;
    pub const BYTES: usize = 128;
}

/// `CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD`, and the identically-valued
/// `CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD`: both enums start at 1
/// with the opaque-fd variant.
pub const CU_EXTERNAL_HANDLE_TYPE_OPAQUE_FD: u32 = 1;

pub struct CudaFns {
    pub cuInit: unsafe extern "C" fn(flags: c_uint) -> CUresult,
    pub cuDeviceGet: unsafe extern "C" fn(device: *mut CUdevice, ordinal: c_int) -> CUresult,
    pub cuCtxCreate_v2:
        unsafe extern "C" fn(pctx: *mut CUcontext, flags: c_uint, dev: CUdevice) -> CUresult,
    pub cuCtxDestroy_v2: unsafe extern "C" fn(ctx: CUcontext) -> CUresult,
    pub cuCtxPushCurrent_v2: unsafe extern "C" fn(ctx: CUcontext) -> CUresult,
    pub cuCtxPopCurrent_v2: unsafe extern "C" fn(pctx: *mut CUcontext) -> CUresult,
    // Device primary context, retained once per device and shared by every
    // encoder in the process — see nvenc_encode.rs::primary_ctx.
    pub cuDevicePrimaryCtxRetain:
        unsafe extern "C" fn(pctx: *mut CUcontext, dev: CUdevice) -> CUresult,
    pub cuCtxSetCurrent: unsafe extern "C" fn(ctx: CUcontext) -> CUresult,
    pub cuMemAlloc_v2: unsafe extern "C" fn(dptr: *mut CUdeviceptr, bytesize: usize) -> CUresult,
    pub cuMemFree_v2: unsafe extern "C" fn(dptr: CUdeviceptr) -> CUresult,
    pub cuMemcpyHtoD_v2:
        unsafe extern "C" fn(dst: CUdeviceptr, src: *const c_void, bytesize: usize) -> CUresult,
    pub cuMemAllocHost_v2: unsafe extern "C" fn(pp: *mut *mut c_void, bytesize: usize) -> CUresult,
    pub cuMemFreeHost: unsafe extern "C" fn(p: *mut c_void) -> CUresult,
    pub cuMemAllocPitch_v2: unsafe extern "C" fn(
        dptr: *mut CUdeviceptr,
        pPitch: *mut usize,
        WidthInBytes: usize,
        Height: usize,
        ElementSizeBytes: c_uint,
    ) -> CUresult,
    pub cuStreamSynchronize: unsafe extern "C" fn(hStream: *mut c_void) -> CUresult,
    // External memory import (CUDA 10.0+) — used for zero-copy DMA-BUF import.
    pub cuImportExternalMemory: Option<
        unsafe extern "C" fn(
            extMem_out: *mut CUexternalMemory,
            memHandleDesc: *const c_void,
        ) -> CUresult,
    >,
    pub cuExternalMemoryGetMappedBuffer: Option<
        unsafe extern "C" fn(
            devPtr: *mut CUdeviceptr,
            extMem: CUexternalMemory,
            bufferDesc: *const c_void,
        ) -> CUresult,
    >,
    pub cuDestroyExternalMemory: Option<unsafe extern "C" fn(extMem: CUexternalMemory) -> CUresult>,
    // External semaphore import (CUDA 10.0+).  An OPAQUE_FD allocation is
    // not a dma_buf and so carries no implicit fencing: without waiting on
    // the Vulkan semaphore that guards the BGRA→NV12 compute dispatch, CUDA
    // reads race the compositor's writes.  The symptom is intermittent
    // tearing and stale frames, worst at high frame rates — i.e. exactly the
    // kind of failure that looks like "works" in a short test.
    pub cuImportExternalSemaphore: Option<
        unsafe extern "C" fn(
            extSem_out: *mut CUexternalSemaphore,
            semHandleDesc: *const c_void,
        ) -> CUresult,
    >,
    pub cuWaitExternalSemaphoresAsync: Option<
        unsafe extern "C" fn(
            extSemArray: *const CUexternalSemaphore,
            paramsArray: *const c_void,
            numExtSems: c_uint,
            stream: *mut c_void,
        ) -> CUresult,
    >,
    pub cuDestroyExternalSemaphore:
        Option<unsafe extern "C" fn(extSem: CUexternalSemaphore) -> CUresult>,
    /// Used to pick the CUDA device matching the Vulkan `deviceUUID`.  On a
    /// multi-GPU host (this one has an AMD iGPU alongside the 4090) importing
    /// into a context on the wrong device fails, and blit has a history of
    /// landing on the iGPU.
    pub cuDeviceGetUuid_v2:
        Option<unsafe extern "C" fn(uuid: *mut [u8; 16], dev: CUdevice) -> CUresult>,
    _lib: DynLib,
}

impl CudaFns {
    pub fn load() -> Result<Self, String> {
        let lib = DynLib::open(&["libcuda.so.1", "libcuda.so"])?;
        unsafe {
            Ok(Self {
                cuInit: lib.sym("cuInit")?,
                cuDeviceGet: lib.sym("cuDeviceGet")?,
                cuCtxCreate_v2: lib.sym("cuCtxCreate_v2")?,
                cuCtxDestroy_v2: lib.sym("cuCtxDestroy_v2")?,
                cuCtxPushCurrent_v2: lib.sym("cuCtxPushCurrent_v2")?,
                cuCtxPopCurrent_v2: lib.sym("cuCtxPopCurrent_v2")?,
                cuDevicePrimaryCtxRetain: lib.sym("cuDevicePrimaryCtxRetain")?,
                cuCtxSetCurrent: lib.sym("cuCtxSetCurrent")?,
                cuMemAlloc_v2: lib.sym("cuMemAlloc_v2")?,
                cuMemFree_v2: lib.sym("cuMemFree_v2")?,
                cuMemcpyHtoD_v2: lib.sym("cuMemcpyHtoD_v2")?,
                cuMemAllocHost_v2: lib.sym("cuMemAllocHost_v2")?,
                cuMemFreeHost: lib.sym("cuMemFreeHost")?,
                cuMemAllocPitch_v2: lib.sym("cuMemAllocPitch_v2")?,
                cuStreamSynchronize: lib.sym("cuStreamSynchronize")?,
                // Optional: only available with CUDA 10.0+ drivers.
                cuImportExternalMemory: lib.sym("cuImportExternalMemory").ok(),
                cuExternalMemoryGetMappedBuffer: lib.sym("cuExternalMemoryGetMappedBuffer").ok(),
                cuDestroyExternalMemory: lib.sym("cuDestroyExternalMemory").ok(),
                cuImportExternalSemaphore: lib.sym("cuImportExternalSemaphore").ok(),
                cuWaitExternalSemaphoresAsync: lib.sym("cuWaitExternalSemaphoresAsync").ok(),
                cuDestroyExternalSemaphore: lib.sym("cuDestroyExternalSemaphore").ok(),
                cuDeviceGetUuid_v2: lib.sym("cuDeviceGetUuid_v2").ok(),
                _lib: lib,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// NVENC API
// ---------------------------------------------------------------------------

/// NVENC uses a function-pointer table returned by NvEncodeAPICreateInstance.
/// We store the entry point loaded from libnvidia-encode.so and the
/// function table is obtained at encoder creation time.
pub struct NvEncFns {
    pub NvEncodeAPICreateInstance: unsafe extern "C" fn(functionList: *mut c_void) -> c_uint,
    _lib: DynLib,
}

impl NvEncFns {
    pub fn load() -> Result<Self, String> {
        let lib = DynLib::open(&["libnvidia-encode.so.1", "libnvidia-encode.so"])?;
        unsafe {
            Ok(Self {
                NvEncodeAPICreateInstance: lib.sym("NvEncodeAPICreateInstance")?,
                _lib: lib,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// VA-API
// ---------------------------------------------------------------------------

pub type VADisplay = *mut c_void;
pub type VAConfigID = c_uint;
pub type VAContextID = c_uint;
pub type VASurfaceID = c_uint;
pub type VABufferID = c_uint;
pub type VAStatus = c_int;
pub type VAImageID = c_uint;
pub type VAEntrypoint = c_int;
pub type VAProfile = c_int;

pub const VA_STATUS_SUCCESS: VAStatus = 0;

pub struct VaFns {
    pub vaInitialize:
        unsafe extern "C" fn(dpy: VADisplay, major: *mut c_int, minor: *mut c_int) -> VAStatus,
    pub vaTerminate: unsafe extern "C" fn(dpy: VADisplay) -> VAStatus,
    pub vaQueryConfigEntrypoints: unsafe extern "C" fn(
        dpy: VADisplay,
        profile: VAProfile,
        entrypoints: *mut VAEntrypoint,
        num: *mut c_int,
    ) -> VAStatus,
    pub vaCreateConfig: unsafe extern "C" fn(
        dpy: VADisplay,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        attrib_list: *mut c_void,
        num_attribs: c_int,
        config_id: *mut VAConfigID,
    ) -> VAStatus,
    pub vaDestroyConfig: unsafe extern "C" fn(dpy: VADisplay, config: VAConfigID) -> VAStatus,
    pub vaCreateContext: unsafe extern "C" fn(
        dpy: VADisplay,
        config: VAConfigID,
        width: c_int,
        height: c_int,
        flag: c_int,
        render_targets: *mut VASurfaceID,
        num_render_targets: c_int,
        context: *mut VAContextID,
    ) -> VAStatus,
    pub vaDestroyContext: unsafe extern "C" fn(dpy: VADisplay, context: VAContextID) -> VAStatus,
    pub vaCreateSurfaces: unsafe extern "C" fn(
        dpy: VADisplay,
        format: c_uint,
        width: c_uint,
        height: c_uint,
        surfaces: *mut VASurfaceID,
        num_surfaces: c_uint,
        attrib_list: *mut c_void,
        num_attribs: c_uint,
    ) -> VAStatus,
    pub vaDestroySurfaces: unsafe extern "C" fn(
        dpy: VADisplay,
        surfaces: *mut VASurfaceID,
        num_surfaces: c_int,
    ) -> VAStatus,
    pub vaCreateBuffer: unsafe extern "C" fn(
        dpy: VADisplay,
        context: VAContextID,
        type_: c_int,
        size: c_uint,
        num_elements: c_uint,
        data: *mut c_void,
        buf_id: *mut VABufferID,
    ) -> VAStatus,
    pub vaDestroyBuffer: unsafe extern "C" fn(dpy: VADisplay, buf: VABufferID) -> VAStatus,
    pub vaMapBuffer:
        unsafe extern "C" fn(dpy: VADisplay, buf: VABufferID, pbuf: *mut *mut c_void) -> VAStatus,
    pub vaUnmapBuffer: unsafe extern "C" fn(dpy: VADisplay, buf: VABufferID) -> VAStatus,
    pub vaDeriveImage: unsafe extern "C" fn(
        dpy: VADisplay,
        surface: VASurfaceID,
        image: *mut c_void, // VAImage*
    ) -> VAStatus,
    pub vaDestroyImage: unsafe extern "C" fn(dpy: VADisplay, image: VAImageID) -> VAStatus,
    pub vaBeginPicture: unsafe extern "C" fn(
        dpy: VADisplay,
        context: VAContextID,
        render_target: VASurfaceID,
    ) -> VAStatus,
    pub vaRenderPicture: unsafe extern "C" fn(
        dpy: VADisplay,
        context: VAContextID,
        buffers: *mut VABufferID,
        num_buffers: c_int,
    ) -> VAStatus,
    pub vaEndPicture: unsafe extern "C" fn(dpy: VADisplay, context: VAContextID) -> VAStatus,
    pub vaSyncSurface: unsafe extern "C" fn(dpy: VADisplay, surface: VASurfaceID) -> VAStatus,
    pub vaExportSurfaceHandle: unsafe extern "C" fn(
        dpy: VADisplay,
        surface: VASurfaceID,
        mem_type: c_uint,
        flags: c_uint,
        descriptor: *mut c_void,
    ) -> VAStatus,
    _lib: DynLib,
}

impl VaFns {
    pub fn load() -> Result<Self, String> {
        let lib = DynLib::open(&["libva.so.2", "libva.so"])?;
        unsafe {
            Ok(Self {
                vaInitialize: lib.sym("vaInitialize")?,
                vaTerminate: lib.sym("vaTerminate")?,
                vaQueryConfigEntrypoints: lib.sym("vaQueryConfigEntrypoints")?,
                vaCreateConfig: lib.sym("vaCreateConfig")?,
                vaDestroyConfig: lib.sym("vaDestroyConfig")?,
                vaCreateContext: lib.sym("vaCreateContext")?,
                vaDestroyContext: lib.sym("vaDestroyContext")?,
                vaCreateSurfaces: lib.sym("vaCreateSurfaces")?,
                vaDestroySurfaces: lib.sym("vaDestroySurfaces")?,
                vaCreateBuffer: lib.sym("vaCreateBuffer")?,
                vaDestroyBuffer: lib.sym("vaDestroyBuffer")?,
                vaMapBuffer: lib.sym("vaMapBuffer")?,
                vaUnmapBuffer: lib.sym("vaUnmapBuffer")?,
                vaDeriveImage: lib.sym("vaDeriveImage")?,
                vaDestroyImage: lib.sym("vaDestroyImage")?,
                vaBeginPicture: lib.sym("vaBeginPicture")?,
                vaRenderPicture: lib.sym("vaRenderPicture")?,
                vaEndPicture: lib.sym("vaEndPicture")?,
                vaSyncSurface: lib.sym("vaSyncSurface")?,
                vaExportSurfaceHandle: lib.sym("vaExportSurfaceHandle")?,
                _lib: lib,
            })
        }
    }
}

/// VA-API DRM display creation (from libva-drm.so).
pub struct VaDrmFns {
    pub vaGetDisplayDRM: unsafe extern "C" fn(fd: c_int) -> VADisplay,
    _lib: DynLib,
}

impl VaDrmFns {
    pub fn load() -> Result<Self, String> {
        let lib = DynLib::open(&["libva-drm.so.2", "libva-drm.so"])?;
        unsafe {
            Ok(Self {
                vaGetDisplayDRM: lib.sym("vaGetDisplayDRM")?,
                _lib: lib,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// GBM (Generic Buffer Manager) — allocates DRM-native DMA-BUFs that both
// VA-API and Vulkan can import.
// ---------------------------------------------------------------------------

pub type GbmDevice = *mut c_void;
pub type GbmBo = *mut c_void;

pub struct GbmFns {
    pub gbm_create_device: unsafe extern "C" fn(fd: c_int) -> GbmDevice,
    pub gbm_device_destroy: unsafe extern "C" fn(gbm: GbmDevice),
    pub gbm_bo_create: unsafe extern "C" fn(
        gbm: GbmDevice,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> GbmBo,
    pub gbm_bo_destroy: unsafe extern "C" fn(bo: GbmBo),
    pub gbm_bo_get_fd: unsafe extern "C" fn(bo: GbmBo) -> c_int,
    pub gbm_bo_get_stride: unsafe extern "C" fn(bo: GbmBo) -> u32,
    pub gbm_bo_get_modifier: unsafe extern "C" fn(bo: GbmBo) -> u64,
    pub gbm_bo_get_handle: unsafe extern "C" fn(bo: GbmBo) -> GbmBoHandle,
    _lib: DynLib,
}

/// `union gbm_bo_handle` — only the u32 GEM handle field is used.
#[repr(C)]
#[derive(Copy, Clone)]
pub union GbmBoHandle {
    pub u32_: u32,
    pub u64_: u64,
}

unsafe impl Send for GbmFns {}
unsafe impl Sync for GbmFns {}

// GBM_FORMAT_ARGB8888 = __gbm_fourcc_code('A','R','2','4')
pub const GBM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
pub const GBM_BO_USE_RENDERING: u32 = 1 << 2;
pub const GBM_BO_USE_LINEAR: u32 = 1 << 4;

impl GbmFns {
    pub fn load() -> Result<Self, String> {
        let lib = DynLib::open(&["libgbm.so.1", "libgbm.so"])?;
        unsafe {
            Ok(Self {
                gbm_create_device: lib.sym("gbm_create_device")?,
                gbm_device_destroy: lib.sym("gbm_device_destroy")?,
                gbm_bo_create: lib.sym("gbm_bo_create")?,
                gbm_bo_destroy: lib.sym("gbm_bo_destroy")?,
                gbm_bo_get_fd: lib.sym("gbm_bo_get_fd")?,
                gbm_bo_get_stride: lib.sym("gbm_bo_get_stride")?,
                gbm_bo_get_modifier: lib.sym("gbm_bo_get_modifier")?,
                gbm_bo_get_handle: lib.sym("gbm_bo_get_handle")?,
                _lib: lib,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Singleton accessors
// ---------------------------------------------------------------------------

static CUDA: OnceLock<Result<CudaFns, String>> = OnceLock::new();
static NVENC: OnceLock<Result<NvEncFns, String>> = OnceLock::new();
static VA: OnceLock<Result<VaFns, String>> = OnceLock::new();
static VA_DRM: OnceLock<Result<VaDrmFns, String>> = OnceLock::new();
static GBM: OnceLock<Result<GbmFns, String>> = OnceLock::new();

pub fn cuda() -> Result<&'static CudaFns, &'static str> {
    CUDA.get_or_init(CudaFns::load)
        .as_ref()
        .map_err(|e| e.as_str())
}

pub fn nvenc() -> Result<&'static NvEncFns, &'static str> {
    NVENC
        .get_or_init(NvEncFns::load)
        .as_ref()
        .map_err(|e| e.as_str())
}

pub fn va() -> Result<&'static VaFns, &'static str> {
    VA.get_or_init(VaFns::load).as_ref().map_err(|e| e.as_str())
}

pub fn va_drm() -> Result<&'static VaDrmFns, &'static str> {
    VA_DRM
        .get_or_init(VaDrmFns::load)
        .as_ref()
        .map_err(|e| e.as_str())
}

pub fn gbm() -> Result<&'static GbmFns, &'static str> {
    GBM.get_or_init(GbmFns::load)
        .as_ref()
        .map_err(|e| e.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CUDA_EXTERNAL_MEMORY_HANDLE_DESC`, transcribed from cuda.h.  We build
    /// this descriptor as raw bytes at the call site (we have no CUDA headers
    /// to bind against), so nothing but this mirror checks our arithmetic.
    ///
    /// The offsets were wrong for as long as the external-memory path existed:
    /// `size` was written at 16, inside the `handle` union's tail, and the
    /// driver read size=0 and refused every import with
    /// `CUDA_ERROR_INVALID_VALUE` — for every handle type, which is what made
    /// it read as "this driver will not import a dma_buf" rather than "this
    /// descriptor is malformed".  Let the compiler compute the offsets.
    #[repr(C)]
    struct CudaExternalMemoryHandleDesc {
        typ: u32,
        handle: CudaExternalMemoryHandleUnion,
        size: u64,
        flags: u32,
        reserved: [u32; 16],
    }

    #[repr(C)]
    union CudaExternalMemoryHandleUnion {
        fd: c_int,
        win32: CudaWin32Handle,
        nv_sci_buf_object: *const c_void,
    }

    /// The member that sizes the union: two pointers, so 16 bytes — not the
    /// 8 the old comment assumed.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CudaWin32Handle {
        handle: *mut c_void,
        name: *const c_void,
    }

    #[repr(C)]
    struct CudaExternalSemaphoreHandleDesc {
        typ: u32,
        handle: CudaExternalMemoryHandleUnion,
        flags: u32,
        reserved: [u32; 16],
    }

    #[test]
    fn ext_mem_desc_offsets_match_cuda_layout() {
        assert_eq!(
            std::mem::offset_of!(CudaExternalMemoryHandleDesc, typ),
            cu_ext_mem_desc::TYPE
        );
        assert_eq!(
            std::mem::offset_of!(CudaExternalMemoryHandleDesc, handle),
            cu_ext_mem_desc::FD,
            "handle.fd is the union's first member, so it shares its offset"
        );
        assert_eq!(
            std::mem::offset_of!(CudaExternalMemoryHandleDesc, size),
            cu_ext_mem_desc::SIZE,
            "size follows the 16-byte handle union; writing it at 16 passes size=0"
        );
        assert_eq!(
            std::mem::offset_of!(CudaExternalMemoryHandleDesc, flags),
            cu_ext_mem_desc::FLAGS
        );
        assert!(
            std::mem::size_of::<CudaExternalMemoryHandleDesc>() <= cu_ext_mem_desc::BYTES,
            "the byte buffer we pass must cover the whole struct"
        );
    }

    #[test]
    fn ext_sem_desc_offsets_match_cuda_layout() {
        assert_eq!(
            std::mem::offset_of!(CudaExternalSemaphoreHandleDesc, typ),
            cu_ext_sem_desc::TYPE
        );
        assert_eq!(
            std::mem::offset_of!(CudaExternalSemaphoreHandleDesc, handle),
            cu_ext_sem_desc::FD
        );
        assert_eq!(
            std::mem::offset_of!(CudaExternalSemaphoreHandleDesc, flags),
            cu_ext_sem_desc::FLAGS,
            "this struct has no `size`, so flags sits directly after the union"
        );
        assert!(std::mem::size_of::<CudaExternalSemaphoreHandleDesc>() <= cu_ext_sem_desc::BYTES);
    }
}
