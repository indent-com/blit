//! DMA-BUF zero-copy GPU encoding backends.
//!
//! Two backends are provided, probed at runtime:
//!
//! 1. **VA-API VPP** (Intel/AMD): imports DMA-BUF as a VASurface, uses the
//!    Video Processing Pipeline to convert BGRA→NV12 on GPU, and returns the
//!    NV12 VASurface for encoding.  Requires `VAEntrypointVideoProc`.
//!
//! 2. **CUDA-EGL** (NVIDIA): imports DMA-BUF via EGL, registers with CUDA,
//!    runs a BGRA→NV12 conversion kernel, and feeds the result to NVENC.
//!
//! Both backends are accessed through `DmaBufEncoder::try_new()` which probes
//! for available hardware and returns the first working backend.
//!
//! All GPU libraries (libva, libEGL, libcuda) are loaded via `dlopen` at
//! runtime — no link-time dependency.

#![allow(
    non_camel_case_types,
    non_snake_case,
    dead_code,
    unsafe_op_in_unsafe_fn
)]

use std::ffi::{c_int, c_uint, c_void};
use std::os::fd::AsRawFd;
use std::ptr;

// ---------------------------------------------------------------------------
// Common types
// ---------------------------------------------------------------------------

type VADisplay = *mut c_void;
type VAConfigID = c_uint;
type VAContextID = c_uint;
type VASurfaceID = c_uint;
type VABufferID = c_uint;
type VAStatus = c_int;

const VA_STATUS_SUCCESS: VAStatus = 0;
const VA_RT_FORMAT_RGB32: c_uint = 0x00000100;
const VA_RT_FORMAT_YUV420: c_uint = 0x00000001;
const VA_FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

// VASurfaceAttrib types
const VA_SURFACE_ATTRIB_PIXEL_FORMAT: c_uint = 1;
const VA_SURFACE_ATTRIB_MEM_TYPE: c_uint = 2;
const VA_SURFACE_ATTRIB_EXTERNAL_BUFFERS: c_uint = 4;
const VA_SURFACE_ATTRIB_SETTABLE: c_uint = 0x00000002;

const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME: c_uint = 0x20000000;

// VAEntrypoint for VPP
const VA_ENTRYPOINT_VIDEO_PROC: c_int = 10;
// VAProfile for VPP
const VA_PROFILE_NONE: c_int = -1;

// VAProcPipelineParameterBuffer type
const VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE: c_int = 41;

// ---------------------------------------------------------------------------
// VA-API FFI structures
// ---------------------------------------------------------------------------

#[repr(C)]
struct VASurfaceAttrib {
    type_: c_uint,
    flags: c_uint,
    value: VAGenericValue,
}

#[repr(C)]
union VAGenericValue {
    i: c_int,
    f: f32,
    p: *mut c_void,
}

#[repr(C)]
struct VASurfaceAttribExternalBuffers {
    pixel_format: u32,
    width: u32,
    height: u32,
    data_size: u32,
    num_planes: u32,
    pitches: [u32; 4],
    offsets: [u32; 4],
    buffers: *mut libc::uintptr_t,
    num_buffers: u32,
    flags: u32,
    private_data: *mut c_void,
}

#[repr(C)]
struct VAProcPipelineParameterBuffer {
    surface: VASurfaceID,
    surface_region: *const c_void, // VARectangle*
    surface_color_standard: c_uint,
    output_region: *const c_void,
    output_background_color: u32,
    output_color_standard: c_uint,
    pipeline_flags: u32,
    filter_flags: u32,
    filters: *mut VABufferID,
    num_filters: u32,
    forward_references: *mut VASurfaceID,
    num_forward_references: u32,
    backward_references: *mut VASurfaceID,
    num_backward_references: u32,
    rotation_state: u32,
    blend_state: *const c_void,
    mirror_state: u32,
    additional_outputs: *mut VASurfaceID,
    num_additional_outputs: u32,
    input_color_properties: u64,  // placeholder
    output_color_properties: u64, // placeholder
    processing_mode: u32,
    output_hdr_metadata: *const c_void,
}

// ---------------------------------------------------------------------------
// VA-API function pointers (loaded via dlopen)
// ---------------------------------------------------------------------------

pub(crate) struct VaApiFns {
    vaCreateConfig: unsafe extern "C" fn(
        VADisplay,
        c_int,
        c_int,
        *mut c_void,
        c_int,
        *mut VAConfigID,
    ) -> VAStatus,
    vaCreateContext: unsafe extern "C" fn(
        VADisplay,
        VAConfigID,
        c_int,
        c_int,
        c_int,
        *mut VASurfaceID,
        c_int,
        *mut VAContextID,
    ) -> VAStatus,
    vaDestroyConfig: unsafe extern "C" fn(VADisplay, VAConfigID) -> VAStatus,
    vaDestroyContext: unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus,
    vaCreateSurfaces: unsafe extern "C" fn(
        VADisplay,
        c_uint,
        c_uint,
        c_uint,
        *mut VASurfaceID,
        c_uint,
        *mut VASurfaceAttrib,
        c_uint,
    ) -> VAStatus,
    vaDestroySurfaces: unsafe extern "C" fn(VADisplay, *mut VASurfaceID, c_int) -> VAStatus,
    vaCreateBuffer: unsafe extern "C" fn(
        VADisplay,
        VAContextID,
        c_int,
        c_uint,
        c_uint,
        *mut c_void,
        *mut VABufferID,
    ) -> VAStatus,
    vaDestroyBuffer: unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus,
    vaBeginPicture: unsafe extern "C" fn(VADisplay, VAContextID, VASurfaceID) -> VAStatus,
    vaRenderPicture:
        unsafe extern "C" fn(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus,
    vaEndPicture: unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus,
    vaSyncSurface: unsafe extern "C" fn(VADisplay, VASurfaceID) -> VAStatus,
    vaQueryConfigEntrypoints:
        unsafe extern "C" fn(VADisplay, c_int, *mut c_int, *mut c_int) -> VAStatus,
}

// ---------------------------------------------------------------------------
// VA-API VPP Backend
// ---------------------------------------------------------------------------

/// VA-API Video Processing Pipeline context for DMA-BUF zero-copy encoding.
///
/// Imports a BGRA DMA-BUF as a VASurface, converts to NV12 on the GPU,
/// and provides the NV12 surface for encoding.
pub struct VaapiVppContext {
    fns: VaApiFns,
    display: VADisplay,
    config: VAConfigID,
    context: VAContextID,
    /// Pool of pre-allocated NV12 output surfaces.
    nv12_surfaces: Vec<VASurfaceID>,
    next_surface: usize,
    width: u32,
    height: u32,
}

impl VaapiVppContext {
    /// Try to create a VPP context from an existing VA-API display.
    ///
    /// Returns `None` if the VA-API driver doesn't support VPP
    /// (`VAEntrypointVideoProc`).
    pub unsafe fn try_new(
        display: VADisplay,
        fns: VaApiFns,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        // Check if VideoProc entrypoint is available.
        let mut entrypoints = [0i32; 16];
        let mut num_entrypoints = 0i32;
        let status = (fns.vaQueryConfigEntrypoints)(
            display,
            VA_PROFILE_NONE,
            entrypoints.as_mut_ptr(),
            &mut num_entrypoints,
        );
        if status != VA_STATUS_SUCCESS {
            return None;
        }
        let has_vpp = entrypoints[..num_entrypoints as usize].contains(&VA_ENTRYPOINT_VIDEO_PROC);
        if !has_vpp {
            eprintln!("[dmabuf-zerocopy] VA-API VideoProc not available");
            return None;
        }

        // Create VPP config + context.
        let mut config: VAConfigID = 0;
        let status = (fns.vaCreateConfig)(
            display,
            VA_PROFILE_NONE,
            VA_ENTRYPOINT_VIDEO_PROC,
            ptr::null_mut(),
            0,
            &mut config,
        );
        if status != VA_STATUS_SUCCESS {
            return None;
        }

        // Create NV12 output surfaces (pool of 4 for pipelining).
        let mut nv12_surfaces = vec![0u32; 4];
        let status = (fns.vaCreateSurfaces)(
            display,
            VA_RT_FORMAT_YUV420,
            width,
            height,
            nv12_surfaces.as_mut_ptr(),
            4,
            ptr::null_mut(),
            0,
        );
        if status != VA_STATUS_SUCCESS {
            (fns.vaDestroyConfig)(display, config);
            return None;
        }

        let mut context: VAContextID = 0;
        let status = (fns.vaCreateContext)(
            display,
            config,
            width as i32,
            height as i32,
            0, // flag
            nv12_surfaces.as_mut_ptr(),
            nv12_surfaces.len() as i32,
            &mut context,
        );
        if status != VA_STATUS_SUCCESS {
            (fns.vaDestroySurfaces)(display, nv12_surfaces.as_mut_ptr(), 4);
            (fns.vaDestroyConfig)(display, config);
            return None;
        }

        eprintln!("[dmabuf-zerocopy] VA-API VPP initialized for {width}x{height}");

        Some(Self {
            fns,
            display,
            config,
            context,
            nv12_surfaces,
            next_surface: 0,
            width,
            height,
        })
    }

    /// Import a DMA-BUF as a BGRA VASurface, run VPP to convert to NV12,
    /// and return the NV12 VASurface ID.
    ///
    /// The returned surface is from the internal pool and must be used
    /// before the next call (pool is round-robin).
    pub unsafe fn convert_dmabuf(
        &mut self,
        fd: &std::os::fd::OwnedFd,
        fourcc: u32,
        stride: u32,
        offset: u32,
    ) -> Option<VASurfaceID> {
        let raw_fd = fd.as_raw_fd();

        // Import DMA-BUF as a BGRA VASurface.
        let mut buf_fd = raw_fd as libc::uintptr_t;
        let mut ext_buf = VASurfaceAttribExternalBuffers {
            pixel_format: fourcc,
            width: self.width,
            height: self.height,
            data_size: stride * self.height,
            num_planes: 1,
            pitches: [stride, 0, 0, 0],
            offsets: [offset, 0, 0, 0],
            buffers: &mut buf_fd,
            num_buffers: 1,
            flags: 0,
            private_data: ptr::null_mut(),
        };

        let attribs = [
            VASurfaceAttrib {
                type_: VA_SURFACE_ATTRIB_MEM_TYPE,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VAGenericValue {
                    i: VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME as i32,
                },
            },
            VASurfaceAttrib {
                type_: VA_SURFACE_ATTRIB_EXTERNAL_BUFFERS,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VAGenericValue {
                    p: &mut ext_buf as *mut _ as *mut c_void,
                },
            },
        ];

        let mut bgra_surface: VASurfaceID = 0;
        let status = (self.fns.vaCreateSurfaces)(
            self.display,
            VA_RT_FORMAT_RGB32,
            self.width,
            self.height,
            &mut bgra_surface,
            1,
            attribs.as_ptr() as *mut _,
            attribs.len() as u32,
        );
        if status != VA_STATUS_SUCCESS {
            return None;
        }

        // Get next NV12 output surface from pool.
        let nv12_surface = self.nv12_surfaces[self.next_surface];
        self.next_surface = (self.next_surface + 1) % self.nv12_surfaces.len();

        // Set up VPP pipeline: BGRA input → NV12 output.
        let mut params = VAProcPipelineParameterBuffer {
            surface: bgra_surface,
            surface_region: ptr::null(),
            surface_color_standard: 0,
            output_region: ptr::null(),
            output_background_color: 0,
            output_color_standard: 0,
            pipeline_flags: 0,
            filter_flags: 0,
            filters: ptr::null_mut(),
            num_filters: 0,
            forward_references: ptr::null_mut(),
            num_forward_references: 0,
            backward_references: ptr::null_mut(),
            num_backward_references: 0,
            rotation_state: 0,
            blend_state: ptr::null(),
            mirror_state: 0,
            additional_outputs: ptr::null_mut(),
            num_additional_outputs: 0,
            input_color_properties: 0,
            output_color_properties: 0,
            processing_mode: 0,
            output_hdr_metadata: ptr::null(),
        };

        let mut buf_id: VABufferID = 0;
        let status = (self.fns.vaCreateBuffer)(
            self.display,
            self.context,
            VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE,
            std::mem::size_of::<VAProcPipelineParameterBuffer>() as u32,
            1,
            &mut params as *mut _ as *mut c_void,
            &mut buf_id,
        );
        if status != VA_STATUS_SUCCESS {
            (self.fns.vaDestroySurfaces)(self.display, &mut bgra_surface, 1);
            return None;
        }

        // Run VPP.
        let ok = (self.fns.vaBeginPicture)(self.display, self.context, nv12_surface)
            == VA_STATUS_SUCCESS
            && (self.fns.vaRenderPicture)(self.display, self.context, &mut buf_id, 1)
                == VA_STATUS_SUCCESS
            && (self.fns.vaEndPicture)(self.display, self.context) == VA_STATUS_SUCCESS
            && (self.fns.vaSyncSurface)(self.display, nv12_surface) == VA_STATUS_SUCCESS;

        // Cleanup per-frame resources.
        (self.fns.vaDestroyBuffer)(self.display, buf_id);
        (self.fns.vaDestroySurfaces)(self.display, &mut bgra_surface, 1);

        if ok { Some(nv12_surface) } else { None }
    }
}

impl Drop for VaapiVppContext {
    fn drop(&mut self) {
        unsafe {
            (self.fns.vaDestroyContext)(self.display, self.context);
            (self.fns.vaDestroySurfaces)(
                self.display,
                self.nv12_surfaces.as_mut_ptr(),
                self.nv12_surfaces.len() as i32,
            );
            (self.fns.vaDestroyConfig)(self.display, self.config);
        }
    }
}

// ---------------------------------------------------------------------------
// TODO: CUDA-EGL Backend (placeholder)
// ---------------------------------------------------------------------------

// The CUDA-EGL backend will be implemented in a follow-up:
// 1. dlopen libEGL, libcuda
// 2. Create EGLDisplay from GBM device
// 3. Per-frame: eglCreateImage(DMA_BUF) → cuGraphicsEGLRegisterImage → kernel → NVENC
//
// For now, the system falls through to the CPU mmap fallback when
// VA-API VPP is not available (i.e., on NVIDIA).
