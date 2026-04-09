//! Runtime GPU library loading via dlopen.
//!
//! All GPU driver libraries are loaded on first use.  If a library is
//! missing the corresponding encoder backend is simply unavailable —
//! the binary remains fully functional with software-only encoding.
//!
//! This allows the server binary to be statically linked (no build-time
//! dependency on libva, libcuda, libnvidia-encode, etc.) while still
//! using hardware acceleration when the drivers are installed.

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

pub struct CudaFns {
    pub cuInit: unsafe extern "C" fn(flags: c_uint) -> CUresult,
    pub cuDeviceGet: unsafe extern "C" fn(device: *mut CUdevice, ordinal: c_int) -> CUresult,
    pub cuCtxCreate_v2:
        unsafe extern "C" fn(pctx: *mut CUcontext, flags: c_uint, dev: CUdevice) -> CUresult,
    pub cuCtxDestroy_v2: unsafe extern "C" fn(ctx: CUcontext) -> CUresult,
    pub cuCtxPushCurrent_v2: unsafe extern "C" fn(ctx: CUcontext) -> CUresult,
    pub cuCtxPopCurrent_v2: unsafe extern "C" fn(pctx: *mut CUcontext) -> CUresult,
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
// Singleton accessors
// ---------------------------------------------------------------------------

static CUDA: OnceLock<Result<CudaFns, String>> = OnceLock::new();
static NVENC: OnceLock<Result<NvEncFns, String>> = OnceLock::new();
static VA: OnceLock<Result<VaFns, String>> = OnceLock::new();
static VA_DRM: OnceLock<Result<VaDrmFns, String>> = OnceLock::new();

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
