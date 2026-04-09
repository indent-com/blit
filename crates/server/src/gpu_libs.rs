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
    pub(crate) fn open(names: &[&str]) -> Option<Self> {
        Self::open_flags(names, libc::RTLD_NOW | libc::RTLD_LOCAL)
    }

    /// Open with RTLD_GLOBAL — needed for GLVND so EGL and GL dispatch
    /// tables are shared across libEGL.so and libGLESv2.so.
    #[cfg(unix)]
    pub(crate) fn open_global(names: &[&str]) -> Option<Self> {
        Self::open_flags(names, libc::RTLD_NOW | libc::RTLD_GLOBAL)
    }

    #[cfg(unix)]
    fn open_flags(names: &[&str], flags: libc::c_int) -> Option<Self> {
        for name in names {
            let cname = std::ffi::CString::new(*name).ok()?;
            let handle = unsafe { libc::dlopen(cname.as_ptr(), flags) };
            if !handle.is_null() {
                eprintln!("[gpu-libs] loaded {name}");
                return Some(Self { handle });
            }
        }
        None
    }

    #[cfg(not(unix))]
    pub(crate) fn open(_names: &[&str]) -> Option<Self> {
        None
    }

    #[cfg(unix)]
    pub(crate) unsafe fn sym<T>(&self, name: &str) -> Option<T> {
        let cname = std::ffi::CString::new(name).ok()?;
        let ptr = unsafe { libc::dlsym(self.handle, cname.as_ptr()) };
        if ptr.is_null() {
            return None;
        }
        Some(unsafe { std::mem::transmute_copy(&ptr) })
    }

    #[cfg(not(unix))]
    unsafe fn sym<T>(&self, _name: &str) -> Option<T> {
        None
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
    _lib: DynLib,
}

impl CudaFns {
    pub fn load() -> Option<Self> {
        let lib = DynLib::open(&["libcuda.so.1", "libcuda.so"])?;
        unsafe {
            Some(Self {
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
                _lib: lib,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// NVENC API
// ---------------------------------------------------------------------------

/// Opaque NVENC encoder handle.
pub type NvEncHandle = *mut c_void;

/// NVENC uses a function-pointer table returned by NvEncodeAPICreateInstance.
/// We store the two entry points loaded from libnvidia-encode.so and the
/// function table is obtained at encoder creation time.
pub struct NvEncFns {
    pub NvEncodeAPICreateInstance: unsafe extern "C" fn(functionList: *mut c_void) -> c_uint,
    pub NvEncodeAPIGetMaxSupportedVersion: unsafe extern "C" fn(version: *mut u32) -> c_uint,
    _lib: DynLib,
}

impl NvEncFns {
    pub fn load() -> Option<Self> {
        let lib = DynLib::open(&["libnvidia-encode.so.1", "libnvidia-encode.so"])?;
        unsafe {
            Some(Self {
                NvEncodeAPICreateInstance: lib.sym("NvEncodeAPICreateInstance")?,
                NvEncodeAPIGetMaxSupportedVersion: lib.sym("NvEncodeAPIGetMaxSupportedVersion")?,
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
    pub vaQueryConfigProfiles:
        unsafe extern "C" fn(dpy: VADisplay, profiles: *mut VAProfile, num: *mut c_int) -> VAStatus,
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
    _lib: DynLib,
}

impl VaFns {
    pub fn load() -> Option<Self> {
        let lib = DynLib::open(&["libva.so.2", "libva.so"])?;
        unsafe {
            Some(Self {
                vaInitialize: lib.sym("vaInitialize")?,
                vaTerminate: lib.sym("vaTerminate")?,
                vaQueryConfigProfiles: lib.sym("vaQueryConfigProfiles")?,
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
    pub fn load() -> Option<Self> {
        let lib = DynLib::open(&["libva-drm.so.2", "libva-drm.so"])?;
        unsafe {
            Some(Self {
                vaGetDisplayDRM: lib.sym("vaGetDisplayDRM")?,
                _lib: lib,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Singleton accessors
// ---------------------------------------------------------------------------

static CUDA: OnceLock<Option<CudaFns>> = OnceLock::new();
static NVENC: OnceLock<Option<NvEncFns>> = OnceLock::new();
static VA: OnceLock<Option<VaFns>> = OnceLock::new();
static VA_DRM: OnceLock<Option<VaDrmFns>> = OnceLock::new();

pub fn cuda() -> Option<&'static CudaFns> {
    CUDA.get_or_init(CudaFns::load).as_ref()
}

pub fn nvenc() -> Option<&'static NvEncFns> {
    NVENC.get_or_init(NvEncFns::load).as_ref()
}

pub fn va() -> Option<&'static VaFns> {
    VA.get_or_init(VaFns::load).as_ref()
}

pub fn va_drm() -> Option<&'static VaDrmFns> {
    VA_DRM.get_or_init(VaDrmFns::load).as_ref()
}
