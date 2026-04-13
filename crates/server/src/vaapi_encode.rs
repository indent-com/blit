//! Direct VA-API H.264 encoder — no ffmpeg dependency.
//!
//! Uses `dlopen("libva.so")` and `dlopen("libva-drm.so")` at runtime.
//! Implements Constrained Baseline profile H.264 encoding via the VA-API
//! EncSliceLP (Low Power) or EncSlice entrypoint.
//!
//! The parameter buffer structs are accessed via raw byte arrays at
//! verified offsets rather than `#[repr(C)]` struct translation, since
//! the VA-API header structs contain complex bitfields and large padding
//! arrays that are fragile to replicate in Rust.

#![allow(non_upper_case_globals, clippy::identity_op)]

use crate::gpu_libs::{
    self, VA_STATUS_SUCCESS, VABufferID, VAConfigID, VAContextID, VADisplay, VASurfaceID,
};
use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;

// ---------------------------------------------------------------------------
// VA-API constants
// ---------------------------------------------------------------------------

// Profiles
const VAProfileH264High: i32 = 8;
const VA_PROFILE_NONE: i32 = -1;

// Entrypoints
const VAEntrypointEncSliceLP: i32 = 8;
const VAEntrypointEncSlice: i32 = 6;
const VA_ENTRYPOINT_VIDEO_PROC: i32 = 10;

// RT formats
const VA_RT_FORMAT_YUV420: u32 = 0x00000001;
const VA_RT_FORMAT_RGB32: u32 = 0x00000100;

// VASurfaceAttrib type enum (VASurfaceAttribType in va.h):
//   None=0, PixelFormat=1, MinWidth=2, MaxWidth=3, MinHeight=4, MaxHeight=5,
//   MemoryType=6, ExternalBufferDescriptor=7
const VA_SURFACE_ATTRIB_MEM_TYPE: u32 = 6;
const VA_SURFACE_ATTRIB_EXTERNAL_BUFFERS: u32 = 7;
const VA_SURFACE_ATTRIB_SETTABLE: u32 = 0x00000002;
const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME: u32 = 0x20000000;
const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: u32 = 0x40000000;

// VAProcPipelineParameterBuffer type
const VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE: i32 = 41;

// Buffer types
const VAEncCodedBufferType: i32 = 21;
const VAEncSequenceParameterBufferType: i32 = 22;
const VAEncPictureParameterBufferType: i32 = 23;
const VAEncSliceParameterBufferType: i32 = 24;
const VAEncPackedHeaderParameterBufferType: i32 = 25;
const VAEncPackedHeaderDataBufferType: i32 = 26;
const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 1;
const VA_ENC_PACKED_HEADER_PICTURE: u32 = 2;

/// Packed header parameter buffer (va.h VAEncPackedHeaderParameterBuffer).
#[repr(C)]
struct VAEncPackedHeaderParameterBuffer {
    r#type: u32,
    bit_length: u32,
    has_emulation_bytes: u8,
}

/// Submit packed header data to the VA-API encoder.  The driver includes
/// these bytes in the coded buffer output (verified on AMD radeonsi for
/// AV1; applying the same pattern to H.264).
fn create_packed_header_buffers(
    va: &gpu_libs::VaFns,
    display: VADisplay,
    context: VAContextID,
    header_type: u32,
    data: &[u8],
) -> Option<(VABufferID, VABufferID)> {
    let param = VAEncPackedHeaderParameterBuffer {
        r#type: header_type,
        bit_length: (data.len() * 8) as u32,
        has_emulation_bytes: 0,
    };
    let mut param_buf: VABufferID = 0;
    let st = unsafe {
        (va.vaCreateBuffer)(
            display,
            context,
            VAEncPackedHeaderParameterBufferType,
            std::mem::size_of::<VAEncPackedHeaderParameterBuffer>() as u32,
            1,
            &param as *const _ as *mut c_void,
            &mut param_buf,
        )
    };
    if st != VA_STATUS_SUCCESS {
        return None;
    }
    let mut data_buf: VABufferID = 0;
    let st = unsafe {
        (va.vaCreateBuffer)(
            display,
            context,
            VAEncPackedHeaderDataBufferType,
            data.len() as u32,
            1,
            data.as_ptr() as *mut c_void,
            &mut data_buf,
        )
    };
    if st != VA_STATUS_SUCCESS {
        unsafe {
            (va.vaDestroyBuffer)(display, param_buf);
        }
        return None;
    }
    Some((param_buf, data_buf))
}

// Surface sentinel
const VA_INVALID_SURFACE: u32 = 0xFFFF_FFFF;
const VA_PICTURE_H264_INVALID: u32 = 0x01;

// Struct sizes (from va_enc_h264.h on VA-API 1.23, verified via offsetof)
const SPS_SIZE: usize = 1132;
const PPS_SIZE: usize = 648;
const SLICE_SIZE: usize = 3140;
const VA_IMAGE_SIZE: usize = 120;

// Coded buffer segment
const CBS_BUF_OFF: usize = 16;
const CBS_NEXT_OFF: usize = 24;

// VAImage offsets
const VAIMG_BUF_OFF: usize = 52;
const VAIMG_PITCHES_OFF: usize = 68;
const VAIMG_OFFSETS_OFF: usize = 80;
const VAIMG_ID_OFF: usize = 0;

// VA-API fourcc values differ from DRM fourcc values.
// DRM uses little-endian channel order in the name; VA-API uses memory order.
// DRM AR24 (ARGB8888) = [B,G,R,A] in memory = VA_FOURCC_BGRA.
// DRM AB24 (ABGR8888) = [R,G,B,A] in memory = VA_FOURCC_RGBA.
const VA_FOURCC_BGRA: u32 = u32::from_le_bytes(*b"BGRA"); // DRM ARGB8888
const VA_FOURCC_BGRX: u32 = u32::from_le_bytes(*b"BGRX"); // DRM XRGB8888
const VA_FOURCC_RGBA: u32 = u32::from_le_bytes(*b"RGBA"); // DRM ABGR8888
const VA_FOURCC_RGBX: u32 = u32::from_le_bytes(*b"RGBX"); // DRM XBGR8888

/// Translate a DRM fourcc to VA-API fourcc for packed surface import.
fn drm_fourcc_to_va(drm: u32) -> Option<u32> {
    use blit_compositor::drm_fourcc::*;
    match drm {
        ARGB8888 => Some(VA_FOURCC_BGRA),
        XRGB8888 => Some(VA_FOURCC_BGRX),
        ABGR8888 => Some(VA_FOURCC_RGBA),
        XBGR8888 => Some(VA_FOURCC_RGBX),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// VA-API VPP structs (for DMA-BUF import and BGRA→NV12 conversion on GPU)
// ---------------------------------------------------------------------------

#[repr(C)]
struct VASurfaceAttrib {
    type_: u32,
    flags: u32,
    value: VAGenericValue,
}

#[repr(C)]
struct VAGenericValue {
    type_: u32, // VAGenericValueType (0=int, 1=float, 2=ptr, 3=func)
    value: VAGenericValueInner,
}

#[repr(C)]
union VAGenericValueInner {
    i: i32,
    f: f32,
    p: *mut c_void,
}

/// Legacy PRIME import descriptor (DRM_PRIME).
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

/// Modern PRIME_2 import descriptor (VADRMPRIMESurfaceDescriptor).
/// Includes modifier, used with VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2.
#[repr(C)]
struct VADRMPRIMESurfaceDescriptor {
    fourcc: u32,
    width: u32,
    height: u32,
    num_objects: u32,
    objects: [DRMObject; 4],
    num_layers: u32,
    layers: [DRMLayer; 4],
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct DRMObject {
    fd: i32,
    size: u32,
    drm_format_modifier: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct DRMLayer {
    drm_format: u32,
    num_planes: u32,
    object_index: [u32; 4],
    offset: [u32; 4],
    pitch: [u32; 4],
}

/// VARectangle — used for surface_region / output_region in VPP.
#[repr(C)]
#[derive(Copy, Clone)]
struct VARectangle {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

/// VAProcPipelineParameterBuffer — enough fields for a simple BGRA→NV12 blit.
/// We zero-init the full struct so padding and unused fields are safe.
#[repr(C)]
struct VAProcPipelineParameterBuffer {
    surface: u32, // input VASurfaceID
    surface_region: *const c_void,
    surface_color_standard: u32,
    output_region: *const c_void,
    output_background_color: u32,
    output_color_standard: u32,
    pipeline_flags: u32,
    filter_flags: u32,
    filters: *mut u32,
    num_filters: u32,
    forward_references: *mut u32,
    num_forward_references: u32,
    backward_references: *mut u32,
    num_backward_references: u32,
    rotation_state: u32,
    blend_state: *const c_void,
    mirror_state: u32,
    additional_outputs: *mut u32,
    num_additional_outputs: u32,
    input_color_properties: u64,
    output_color_properties: u64,
    processing_mode: u32,
    output_hdr_metadata: *const c_void,
}

fn fd_inode(fd: std::os::fd::RawFd) -> u64 {
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) == 0 {
            st.st_ino
        } else {
            0
        }
    }
}

// ---------------------------------------------------------------------------
// VPP context — BGRA DMA-BUF → NV12 VASurface on the GPU
// ---------------------------------------------------------------------------

/// Key for caching PRIME-imported VASurfaces so we don't re-import
/// the same GBM BO every frame.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PrimeImportKey {
    ino: u64,
    width: u32,
    height: u32,
    fourcc: u32,
    stride: u32,
}

struct CachedPrimeSurface {
    surface: VASurfaceID,
    key: PrimeImportKey,
}

/// VA-API Video Processing Pipeline context.
/// Shares the VADisplay with the encoder; convert_dmabuf() takes a BGRA
/// DMA-BUF fd and returns an NV12 VASurface ready to be encoded.
/// Info about a VA-API-allocated BGRA surface exported as DMA-BUF.
/// The compositor can import the fd into EGL for FBO rendering;
/// the encoder uses the surface_id directly for VPP.
pub struct ExportedVaSurface {
    pub surface_id: u32,
    pub fd: std::os::fd::OwnedFd,
    pub fourcc: u32,
    pub modifier: u64,
    pub stride: u32,
    pub offset: u32,
    pub width: u32,
    pub height: u32,
}

pub(crate) struct VppContext {
    va: &'static crate::gpu_libs::VaFns,
    display: VADisplay,
    config: u32,
    context: u32,
    /// Pool of NV12 output surfaces (round-robin).
    nv12_surfaces: [u32; 4],
    next_surf: usize,
    width: u32,
    height: u32,
    /// Small cache of PRIME-imported BGRA surfaces (keyed by fd inode).
    /// Avoids expensive vaCreateSurfaces+vaDestroySurfaces per frame.
    import_cache: Vec<CachedPrimeSurface>,
    /// Pre-allocated BGRA input surfaces for zero-copy compositor→encoder.
    bgra_surfaces: Vec<VASurfaceID>,
    verbose: bool,
}

impl VppContext {
    /// Try to create a VPP context on an existing VADisplay.
    /// Returns None if VAEntrypointVideoProc is unavailable.
    pub(crate) unsafe fn try_new(
        va: &'static crate::gpu_libs::VaFns,
        display: VADisplay,
        width: u32,
        height: u32,
        verbose: bool,
    ) -> Option<Self> {
        // Check VideoProc entrypoint is available on VAProfileNone.
        let mut eps = [0i32; 16];
        let mut n = 0i32;
        let st = unsafe {
            (va.vaQueryConfigEntrypoints)(display, VA_PROFILE_NONE, eps.as_mut_ptr(), &mut n)
        };
        if st != crate::gpu_libs::VA_STATUS_SUCCESS {
            return None;
        }
        if !eps[..n as usize].contains(&VA_ENTRYPOINT_VIDEO_PROC) {
            eprintln!("[vaapi-vpp] VAEntrypointVideoProc not available — dmabuf zerocopy disabled");
            return None;
        }

        // Config for VPP (no profile, VideoProc entrypoint).
        let mut config = 0u32;
        let st = unsafe {
            (va.vaCreateConfig)(
                display,
                VA_PROFILE_NONE,
                VA_ENTRYPOINT_VIDEO_PROC,
                ptr::null_mut(),
                0,
                &mut config,
            )
        };
        if st != crate::gpu_libs::VA_STATUS_SUCCESS {
            return None;
        }

        // Allocate pool of NV12 output surfaces.
        let mut nv12_surfaces = [0u32; 4];
        let st = unsafe {
            (va.vaCreateSurfaces)(
                display,
                VA_RT_FORMAT_YUV420,
                width,
                height,
                nv12_surfaces.as_mut_ptr(),
                4,
                ptr::null_mut(),
                0,
            )
        };
        if st != crate::gpu_libs::VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroyConfig)(display, config);
            }
            return None;
        }

        // VPP context.
        let mut context = 0u32;
        let st = unsafe {
            (va.vaCreateContext)(
                display,
                config,
                width as i32,
                height as i32,
                0,
                nv12_surfaces.as_mut_ptr(),
                4,
                &mut context,
            )
        };
        if st != crate::gpu_libs::VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroySurfaces)(display, nv12_surfaces.as_mut_ptr(), 4);
                (va.vaDestroyConfig)(display, config);
            }
            return None;
        }

        // Allocate BGRA input surfaces for the zero-copy path.
        // The compositor renders into these via EGL (after exporting as DMA-BUF);
        // the encoder uses the surface_id directly for VPP BGRA→NV12.
        const NUM_BGRA: usize = 3; // triple-buffered
        let mut bgra_surfaces = vec![0u32; NUM_BGRA];
        let st = unsafe {
            (va.vaCreateSurfaces)(
                display,
                VA_RT_FORMAT_RGB32,
                width,
                height,
                bgra_surfaces.as_mut_ptr(),
                NUM_BGRA as u32,
                ptr::null_mut(),
                0,
            )
        };
        if st != crate::gpu_libs::VA_STATUS_SUCCESS {
            eprintln!("[vaapi-vpp] failed to allocate BGRA surfaces (st={st})");
            bgra_surfaces.clear();
        }

        if verbose {
            eprintln!(
                "[vaapi-vpp] initialized {width}x{height} BGRA→NV12 VPP ({} BGRA surfaces)",
                bgra_surfaces.len()
            );
        }
        Some(Self {
            va,
            display,
            config,
            context,
            nv12_surfaces,
            next_surf: 0,
            width,
            height,
            import_cache: Vec::new(),
            bgra_surfaces,
            verbose,
        })
    }

    /// Export pre-allocated BGRA surfaces as DMA-BUF fds.
    /// The compositor imports these into EGL for FBO rendering.
    pub(crate) fn export_surfaces(&self) -> Vec<ExportedVaSurface> {
        let va = self.va;
        let mut result = Vec::new();
        for &surf_id in &self.bgra_surfaces {
            // Export as PRIME_2 DMA-BUF.
            #[repr(C)]
            struct ExportDesc {
                fourcc: u32,
                width: u32,
                height: u32,
                num_objects: u32,
                objects: [ExportObj; 4],
                num_layers: u32,
                layers: [ExportLayer; 4],
            }
            #[repr(C)]
            #[derive(Default)]
            struct ExportObj {
                fd: i32,
                size: u32,
                drm_format_modifier: u64,
            }
            #[repr(C)]
            #[derive(Default)]
            struct ExportLayer {
                drm_format: u32,
                num_planes: u32,
                object_index: [u32; 4],
                offset: [u32; 4],
                pitch: [u32; 4],
            }
            let mut desc = ExportDesc {
                fourcc: 0,
                width: 0,
                height: 0,
                num_objects: 0,
                objects: Default::default(),
                num_layers: 0,
                layers: Default::default(),
            };
            // VA_EXPORT_SURFACE_READ_WRITE = 0x07
            // VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 = 0x40000000
            let st = unsafe {
                (va.vaExportSurfaceHandle)(
                    self.display,
                    surf_id,
                    0x40000000,
                    0x07,
                    &mut desc as *mut _ as *mut c_void,
                )
            };
            if st != crate::gpu_libs::VA_STATUS_SUCCESS {
                eprintln!("[vaapi-vpp] export surface {surf_id} failed (st={st})");
                continue;
            }
            if desc.num_objects == 0 || desc.num_layers == 0 {
                continue;
            }
            let fd = desc.objects[0].fd;
            if fd < 0 {
                continue;
            }
            let owned_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
            // Force ARGB8888 fourcc for the export.  The surfaces were
            // created with VA_RT_FORMAT_RGB32 (= BGRA in memory), but on
            // AMD (radeonsi) the export descriptor reports the internal
            // tiled format (R16 per-layer, P010 top-level) which is
            // useless for EGL import.  We know the logical format.
            let fourcc: u32 = 0x34325241; // DRM_FORMAT_ARGB8888 = "AR24"
            result.push(ExportedVaSurface {
                surface_id: surf_id,
                fd: owned_fd,
                fourcc,
                modifier: desc.objects[0].drm_format_modifier,
                stride: desc.layers[0].pitch[0],
                offset: desc.layers[0].offset[0],
                width: desc.width,
                height: desc.height,
            });
        }
        if self.verbose {
            for (i, r) in result.iter().enumerate() {
                let fc = r.fourcc.to_le_bytes();
                eprintln!(
                    "[vaapi-vpp] exported surface {i}: {}x{} fourcc={}{}{}{} (0x{:08x}) modifier=0x{:016x} stride={}",
                    r.width,
                    r.height,
                    fc[0] as char,
                    fc[1] as char,
                    fc[2] as char,
                    fc[3] as char,
                    r.fourcc,
                    r.modifier,
                    r.stride,
                );
            }
            eprintln!(
                "[vaapi-vpp] exported {} of {} BGRA surfaces as DMA-BUF",
                result.len(),
                self.bgra_surfaces.len()
            );
        }
        result
    }

    /// Get the raw VADisplay pointer (as usize for Send safety).
    pub(crate) fn va_display_usize(&self) -> usize {
        self.display as usize
    }

    /// Convert a pre-allocated BGRA surface (by VASurfaceID) to NV12.
    /// Zero-copy path — no import needed, the surface is already on this VADisplay.
    pub(crate) unsafe fn convert_surface(&mut self, bgra_surf: VASurfaceID) -> Option<u32> {
        let va = self.va;
        let nv12_surf = self.nv12_surfaces[self.next_surf];
        self.next_surf = (self.next_surf + 1) % self.nv12_surfaces.len();

        let params = VAProcPipelineParameterBuffer {
            surface: bgra_surf,
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
        let mut buf_id = 0u32;
        let st = unsafe {
            (va.vaCreateBuffer)(
                self.display,
                self.context,
                VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE,
                std::mem::size_of::<VAProcPipelineParameterBuffer>() as u32,
                1,
                &params as *const _ as *mut c_void,
                &mut buf_id,
            )
        };
        if st != crate::gpu_libs::VA_STATUS_SUCCESS {
            return None;
        }

        let ok = unsafe {
            (va.vaBeginPicture)(self.display, self.context, nv12_surf)
                == crate::gpu_libs::VA_STATUS_SUCCESS
                && (va.vaRenderPicture)(self.display, self.context, &mut buf_id, 1)
                    == crate::gpu_libs::VA_STATUS_SUCCESS
                && (va.vaEndPicture)(self.display, self.context)
                    == crate::gpu_libs::VA_STATUS_SUCCESS
                && (va.vaSyncSurface)(self.display, nv12_surf) == crate::gpu_libs::VA_STATUS_SUCCESS
        };

        unsafe {
            (va.vaDestroyBuffer)(self.display, buf_id);
        }

        if ok { Some(nv12_surf) } else { None }
    }

    /// Like [`convert_surface`] but flips the image vertically during the
    /// VPP pass.  Used for surfaces rendered by EGL into FBOs, where
    /// OpenGL's bottom-up row order must be corrected to top-down for the
    /// video encoder.
    #[expect(dead_code)]
    pub(crate) unsafe fn convert_surface_flipped(&mut self, bgra_surf: VASurfaceID) -> Option<u32> {
        let va = self.va;
        let nv12_surf = self.nv12_surfaces[self.next_surf];
        self.next_surf = (self.next_surf + 1) % self.nv12_surfaces.len();

        let params = VAProcPipelineParameterBuffer {
            surface: bgra_surf,
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
            mirror_state: 2, // VA_MIRROR_VERTICAL
            additional_outputs: ptr::null_mut(),
            num_additional_outputs: 0,
            input_color_properties: 0,
            output_color_properties: 0,
            processing_mode: 0,
            output_hdr_metadata: ptr::null(),
        };
        let mut buf_id = 0u32;
        let st = unsafe {
            (va.vaCreateBuffer)(
                self.display,
                self.context,
                VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE,
                std::mem::size_of::<VAProcPipelineParameterBuffer>() as u32,
                1,
                &params as *const _ as *mut c_void,
                &mut buf_id,
            )
        };
        if st != crate::gpu_libs::VA_STATUS_SUCCESS {
            return None;
        }

        let ok = unsafe {
            (va.vaBeginPicture)(self.display, self.context, nv12_surf)
                == crate::gpu_libs::VA_STATUS_SUCCESS
                && (va.vaRenderPicture)(self.display, self.context, &mut buf_id, 1)
                    == crate::gpu_libs::VA_STATUS_SUCCESS
                && (va.vaEndPicture)(self.display, self.context)
                    == crate::gpu_libs::VA_STATUS_SUCCESS
                && (va.vaSyncSurface)(self.display, nv12_surf) == crate::gpu_libs::VA_STATUS_SUCCESS
        };

        unsafe {
            (va.vaDestroyBuffer)(self.display, buf_id);
        }

        if ok { Some(nv12_surf) } else { None }
    }

    /// Import a BGRA/XRGB DMA-BUF, run VPP BGRA→NV12, return the NV12 surface.
    /// The returned VASurfaceID is from the internal pool; it must be consumed
    /// (encoded) before the next call.
    ///
    /// PRIME-imported BGRA surfaces are cached by fd inode so that the same
    /// GBM BO (triple-buffered in the compositor) is imported only once.
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn convert_dmabuf(
        &mut self,
        fd: std::os::fd::RawFd,
        fourcc: u32,
        modifier: u64,
        stride: u32,
        offset: u32,
        src_width: u32,
        src_height: u32,
    ) -> Option<u32> {
        let va = self.va;
        let import_w = if src_width > 0 { src_width } else { self.width };
        let import_h = if src_height > 0 {
            src_height
        } else {
            self.height
        };

        // Normalize DRM_FORMAT_MOD_INVALID → LINEAR.
        let modifier = if modifier == 0x00ff_ffff_ffff_ffff {
            0
        } else {
            modifier
        };

        let key = PrimeImportKey {
            ino: fd_inode(fd),
            width: import_w,
            height: import_h,
            fourcc,
            stride,
        };

        // Look up cached PRIME import.
        let bgra_surf = if let Some(cached) = self.import_cache.iter().find(|c| c.key == key) {
            cached.surface
        } else {
            // Cache miss — do the expensive PRIME import.
            let surf = unsafe {
                self.prime_import(fd, fourcc, modifier, stride, offset, import_w, import_h)?
            };
            // Evict oldest if cache is full (keep up to 8 entries to cover
            // triple-buffered compositor + Vulkan WSI quad-buffering).
            const MAX_CACHE: usize = 8;
            if self.import_cache.len() >= MAX_CACHE {
                let old = self.import_cache.remove(0);
                unsafe {
                    let mut s = old.surface;
                    (va.vaDestroySurfaces)(self.display, &mut s, 1);
                }
            }
            self.import_cache
                .push(CachedPrimeSurface { surface: surf, key });
            surf
        };

        let nv12_surf = self.nv12_surfaces[self.next_surf];
        self.next_surf = (self.next_surf + 1) % self.nv12_surfaces.len();

        // When the NV12 output surface is larger than the source (e.g. AV1
        // 64-pixel alignment padding), use output_region to place the source
        // content at the top-left and leave the padding area as black.
        // Without this the VPP would stretch the source to fill the padded
        // surface, producing a slight vertical distortion.
        let out_rect = VARectangle {
            x: 0,
            y: 0,
            width: import_w as u16,
            height: import_h as u16,
        };
        let needs_region = import_w < self.width || import_h < self.height;

        // Run VPP: bgra_surf → nv12_surf.
        let params = VAProcPipelineParameterBuffer {
            surface: bgra_surf,
            surface_region: ptr::null(),
            surface_color_standard: 0,
            output_region: if needs_region {
                &out_rect as *const _ as *const c_void
            } else {
                ptr::null()
            },
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
        let mut buf_id = 0u32;
        let st = unsafe {
            (va.vaCreateBuffer)(
                self.display,
                self.context,
                VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE,
                std::mem::size_of::<VAProcPipelineParameterBuffer>() as u32,
                1,
                &params as *const _ as *mut c_void,
                &mut buf_id,
            )
        };
        if st != crate::gpu_libs::VA_STATUS_SUCCESS {
            return None;
        }

        let ok = unsafe {
            (va.vaBeginPicture)(self.display, self.context, nv12_surf)
                == crate::gpu_libs::VA_STATUS_SUCCESS
                && (va.vaRenderPicture)(self.display, self.context, &mut buf_id, 1)
                    == crate::gpu_libs::VA_STATUS_SUCCESS
                && (va.vaEndPicture)(self.display, self.context)
                    == crate::gpu_libs::VA_STATUS_SUCCESS
                && (va.vaSyncSurface)(self.display, nv12_surf) == crate::gpu_libs::VA_STATUS_SUCCESS
        };

        unsafe {
            (va.vaDestroyBuffer)(self.display, buf_id);
        }

        if ok { Some(nv12_surf) } else { None }
    }

    /// Do the actual PRIME import (expensive — called only on cache miss).
    #[allow(clippy::too_many_arguments)]
    unsafe fn prime_import(
        &self,
        fd: std::os::fd::RawFd,
        fourcc: u32,
        modifier: u64,
        stride: u32,
        offset: u32,
        import_w: u32,
        import_h: u32,
    ) -> Option<VASurfaceID> {
        let va = self.va;

        let va_fourcc = drm_fourcc_to_va(fourcc)?;
        let (surface_fourcc, layer_drm_fourcc) = match va_fourcc {
            VA_FOURCC_RGBA => (VA_FOURCC_BGRA, blit_compositor::drm_fourcc::ARGB8888),
            VA_FOURCC_RGBX => (VA_FOURCC_BGRX, blit_compositor::drm_fourcc::XRGB8888),
            _ => (va_fourcc, fourcc),
        };

        let actual_size = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
        let buf_size = if actual_size > 0 {
            actual_size as u32
        } else {
            stride * import_h
        };

        let mut desc = VADRMPRIMESurfaceDescriptor {
            fourcc: surface_fourcc,
            width: import_w,
            height: import_h,
            num_objects: 1,
            objects: [
                DRMObject {
                    fd,
                    size: buf_size,
                    drm_format_modifier: modifier,
                },
                DRMObject::default(),
                DRMObject::default(),
                DRMObject::default(),
            ],
            num_layers: 1,
            layers: [
                DRMLayer {
                    drm_format: layer_drm_fourcc,
                    num_planes: 1,
                    object_index: [0, 0, 0, 0],
                    offset: [offset, 0, 0, 0],
                    pitch: [stride, 0, 0, 0],
                },
                DRMLayer::default(),
                DRMLayer::default(),
                DRMLayer::default(),
            ],
        };
        let attribs = [
            VASurfaceAttrib {
                type_: VA_SURFACE_ATTRIB_MEM_TYPE,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VAGenericValue {
                    type_: 0,
                    value: VAGenericValueInner {
                        i: VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 as i32,
                    },
                },
            },
            VASurfaceAttrib {
                type_: VA_SURFACE_ATTRIB_EXTERNAL_BUFFERS,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VAGenericValue {
                    type_: 2,
                    value: VAGenericValueInner {
                        p: &mut desc as *mut _ as *mut c_void,
                    },
                },
            },
        ];
        let mut bgra_surf: VASurfaceID = 0;
        let st = unsafe {
            (va.vaCreateSurfaces)(
                self.display,
                VA_RT_FORMAT_RGB32,
                import_w,
                import_h,
                &mut bgra_surf,
                1,
                attribs.as_ptr() as *mut c_void,
                2,
            )
        };
        if st != crate::gpu_libs::VA_STATUS_SUCCESS {
            eprintln!(
                "[vpp] PRIME_2 failed (st={st}) fd={fd} {import_w}x{import_h} drm=0x{fourcc:08x} va=0x{surface_fourcc:08x} layer=0x{layer_drm_fourcc:08x} modifier=0x{modifier:016x} stride={stride} buf_size={buf_size}",
            );
            // Fallback: PRIME_1 (legacy).
            let mut ext_buf = VASurfaceAttribExternalBuffers {
                pixel_format: surface_fourcc,
                width: import_w,
                height: import_h,
                data_size: buf_size,
                num_planes: 1,
                pitches: [stride, 0, 0, 0],
                offsets: [offset, 0, 0, 0],
                buffers: &mut (fd as libc::uintptr_t) as *mut _,
                num_buffers: 1,
                flags: 0,
                private_data: ptr::null_mut(),
            };
            let attribs_p1 = [
                VASurfaceAttrib {
                    type_: VA_SURFACE_ATTRIB_MEM_TYPE,
                    flags: VA_SURFACE_ATTRIB_SETTABLE,
                    value: VAGenericValue {
                        type_: 0,
                        value: VAGenericValueInner {
                            i: VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME as i32,
                        },
                    },
                },
                VASurfaceAttrib {
                    type_: VA_SURFACE_ATTRIB_EXTERNAL_BUFFERS,
                    flags: VA_SURFACE_ATTRIB_SETTABLE,
                    value: VAGenericValue {
                        type_: 2,
                        value: VAGenericValueInner {
                            p: &mut ext_buf as *mut _ as *mut c_void,
                        },
                    },
                },
            ];
            let st2 = unsafe {
                (va.vaCreateSurfaces)(
                    self.display,
                    VA_RT_FORMAT_RGB32,
                    import_w,
                    import_h,
                    &mut bgra_surf,
                    1,
                    attribs_p1.as_ptr() as *mut c_void,
                    2,
                )
            };
            if st2 != crate::gpu_libs::VA_STATUS_SUCCESS {
                eprintln!("[vpp] PRIME_1 also failed (st={st2}) fd={fd} {import_w}x{import_h}");
                return None;
            }
        }
        Some(bgra_surf)
    }
}

impl Drop for VppContext {
    fn drop(&mut self) {
        unsafe {
            let va = self.va;
            // Destroy cached PRIME-imported surfaces.
            for cached in self.import_cache.drain(..) {
                let mut s = cached.surface;
                (va.vaDestroySurfaces)(self.display, &mut s, 1);
            }
            // Destroy pre-allocated BGRA surfaces.
            if !self.bgra_surfaces.is_empty() {
                (va.vaDestroySurfaces)(
                    self.display,
                    self.bgra_surfaces.as_mut_ptr(),
                    self.bgra_surfaces.len() as i32,
                );
            }
            (va.vaDestroyContext)(self.display, self.context);
            (va.vaDestroySurfaces)(self.display, self.nv12_surfaces.as_mut_ptr(), 4);
            (va.vaDestroyConfig)(self.display, self.config);
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: write a value at an offset in a byte buffer
// ---------------------------------------------------------------------------

fn w8(buf: &mut [u8], off: usize, val: u8) {
    buf[off] = val;
}
fn w16(buf: &mut [u8], off: usize, val: u16) {
    buf[off..off + 2].copy_from_slice(&val.to_ne_bytes());
}
fn w32(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_ne_bytes());
}
fn r32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// VaapiDirectEncoder
// ---------------------------------------------------------------------------

/// Number of reconstructed reference surfaces (double-buffered).
const NUM_REF_SURFACES: usize = 2;
/// Number of input surfaces.
const NUM_INPUT_SURFACES: usize = 1;
const TOTAL_SURFACES: usize = NUM_REF_SURFACES + NUM_INPUT_SURFACES;

// ---------------------------------------------------------------------------
// Minimal H.264 bitstream writer for SPS/PPS NAL generation
// ---------------------------------------------------------------------------

struct BitstreamWriter {
    buf: Vec<u8>,
    byte: u8,
    bits_left: u8,
}

impl BitstreamWriter {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(32),
            byte: 0,
            bits_left: 8,
        }
    }

    fn write_bit(&mut self, b: u8) {
        self.byte |= (b & 1) << (self.bits_left - 1);
        self.bits_left -= 1;
        if self.bits_left == 0 {
            self.buf.push(self.byte);
            self.byte = 0;
            self.bits_left = 8;
        }
    }

    fn write_bits(&mut self, val: u32, n: u8) {
        for i in (0..n).rev() {
            self.write_bit(((val >> i) & 1) as u8);
        }
    }

    fn write_ue(&mut self, val: u32) {
        let x = val + 1;
        let leading = 31 - x.leading_zeros(); // number of leading zeros
        for _ in 0..leading {
            self.write_bit(0);
        }
        self.write_bits(x, leading as u8 + 1);
    }

    fn write_se(&mut self, val: i32) {
        if val > 0 {
            self.write_ue((val as u32) * 2 - 1);
        } else {
            self.write_ue((-val as u32) * 2);
        }
    }

    /// Append RBSP trailing bits (1 + alignment zeros) and return the bytes.
    fn finish(mut self) -> Vec<u8> {
        self.write_bit(1); // rbsp_stop_one_bit
        if self.bits_left < 8 {
            self.buf.push(self.byte);
        }
        self.buf
    }
}

/// Build an Annex B SPS NAL for Constrained Baseline H.264.
fn build_h264_sps_nal(width_in_mbs: u16, height_in_mbs: u16, width: u32, height: u32) -> Vec<u8> {
    let max_fs = width_in_mbs as u32 * height_in_mbs as u32;
    let level_idc: u8 = if max_fs <= 1620 {
        31
    } else if max_fs <= 8192 {
        40
    } else if max_fs <= 22080 {
        50
    } else if max_fs <= 36864 {
        51
    } else {
        52
    };

    let mut w = BitstreamWriter::new();
    // profile_idc = 100 (High)
    w.write_bits(100, 8);
    // constraint_set flags = 0 (High profile)
    w.write_bits(0b00000000, 8);
    // level_idc
    w.write_bits(level_idc as u32, 8);
    // seq_parameter_set_id
    w.write_ue(0);
    // --- High profile additional fields ---
    // chroma_format_idc = 1 (4:2:0)
    w.write_ue(1);
    // bit_depth_luma_minus8 = 0
    w.write_ue(0);
    // bit_depth_chroma_minus8 = 0
    w.write_ue(0);
    // qpprime_y_zero_transform_bypass_flag
    w.write_bit(0);
    // seq_scaling_matrix_present_flag
    w.write_bit(0);
    // --- end High profile fields ---
    // log2_max_frame_num_minus4
    w.write_ue(0);
    // pic_order_cnt_type
    w.write_ue(2);
    // max_num_ref_frames
    w.write_ue(1);
    // gaps_in_frame_num_value_allowed_flag
    w.write_bit(0);
    // pic_width_in_mbs_minus1
    w.write_ue(width_in_mbs as u32 - 1);
    // pic_height_in_map_units_minus1
    w.write_ue(height_in_mbs as u32 - 1);
    // frame_mbs_only_flag
    w.write_bit(1);
    // direct_8x8_inference_flag
    w.write_bit(1);

    // Frame cropping
    let crop_w = width_in_mbs as u32 * 16;
    let crop_h = height_in_mbs as u32 * 16;
    if crop_w != width || crop_h != height {
        w.write_bit(1); // frame_cropping_flag
        w.write_ue(0); // left
        w.write_ue((crop_w - width) / 2); // right (chroma samples for 4:2:0)
        w.write_ue(0); // top
        w.write_ue((crop_h - height) / 2); // bottom
    } else {
        w.write_bit(0);
    }

    // vui_parameters_present_flag
    w.write_bit(0);

    let rbsp = w.finish();

    // Assemble: start code + NAL header + RBSP
    let mut nal = Vec::with_capacity(4 + 1 + rbsp.len());
    nal.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    nal.push(0x67); // forbidden=0, nal_ref_idc=3, nal_unit_type=7 (SPS)
    nal.extend_from_slice(&rbsp);
    nal
}

/// Build an Annex B PPS NAL for High profile H.264 (CABAC + 8×8 transform).
fn build_h264_pps_nal() -> Vec<u8> {
    let mut w = BitstreamWriter::new();
    // pic_parameter_set_id
    w.write_ue(0);
    // seq_parameter_set_id
    w.write_ue(0);
    // entropy_coding_mode_flag (1 = CABAC)
    w.write_bit(1);
    // bottom_field_pic_order_in_frame_present_flag
    w.write_bit(0);
    // num_slice_groups_minus1
    w.write_ue(0);
    // num_ref_idx_l0_default_active_minus1
    w.write_ue(0);
    // num_ref_idx_l1_default_active_minus1
    w.write_ue(0);
    // weighted_pred_flag
    w.write_bit(0);
    // weighted_bipred_idc
    w.write_bits(0, 2);
    // pic_init_qp_minus26
    w.write_se(0);
    // pic_init_qs_minus26
    w.write_se(0);
    // chroma_qp_index_offset
    w.write_se(0);
    // deblocking_filter_control_present_flag
    w.write_bit(1);
    // constrained_intra_pred_flag
    w.write_bit(0);
    // redundant_pic_cnt_present_flag
    w.write_bit(0);
    // --- High profile additional PPS fields ---
    // transform_8x8_mode_flag
    w.write_bit(1);
    // pic_scaling_matrix_present_flag
    w.write_bit(0);
    // second_chroma_qp_index_offset
    w.write_se(0);

    let rbsp = w.finish();

    let mut nal = Vec::with_capacity(4 + 1 + rbsp.len());
    nal.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    nal.push(0x68); // forbidden=0, nal_ref_idc=3, nal_unit_type=8 (PPS)
    nal.extend_from_slice(&rbsp);
    nal
}

/// Find the position of the first Annex B start code (00 00 01 or 00 00 00 01).
fn find_annex_b_start(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(2) {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                return Some(i);
            }
            if i + 3 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some(i);
            }
        }
    }
    None
}

pub struct VaapiDirectEncoder {
    va: &'static gpu_libs::VaFns,
    display: VADisplay,
    config: VAConfigID,
    context: VAContextID,
    surfaces: [VASurfaceID; TOTAL_SURFACES],
    coded_buf: VABufferID,
    width: u32,
    height: u32,
    width_in_mbs: u16,
    height_in_mbs: u16,
    frame_num: u16,
    idr_num: u32,
    force_idr: bool,
    cur_ref_idx: usize,
    qp: u8,
    _verbose: bool,
    _drm_fd: OwnedFd,
    /// Optional VA-API VPP context for zero-copy DMA-BUF import.
    /// Present when VAEntrypointVideoProc is supported by the driver.
    vpp: Option<VppContext>,
}

unsafe impl Send for VaapiDirectEncoder {}

impl VaapiDirectEncoder {
    pub fn try_new(
        width: u32,
        height: u32,
        vaapi_device: &str,
        qp: u8,
        verbose: bool,
    ) -> Result<Self, String> {
        let va = gpu_libs::va().map_err(|e| format!("VA-API: {e}"))?;
        let va_drm = gpu_libs::va_drm().map_err(|e| format!("VA-DRM: {e}"))?;

        // Open render node
        let drm_fd = {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(vaapi_device)
                .map_err(|e| format!("failed to open {vaapi_device}: {e}"))?;
            OwnedFd::from(file)
        };

        let display = unsafe { (va_drm.vaGetDisplayDRM)(drm_fd.as_raw_fd()) };
        if display.is_null() {
            return Err("vaGetDisplayDRM returned null".into());
        }

        let mut major = 0i32;
        let mut minor = 0i32;
        let st = unsafe { (va.vaInitialize)(display, &mut major, &mut minor) };
        if st != VA_STATUS_SUCCESS {
            return Err(format!("vaInitialize failed: {st}"));
        }

        // Probe for EncSliceLP or EncSlice on H264ConstrainedBaseline
        let mut entrypoints = [0i32; 16];
        let mut num_ep = 0i32;
        unsafe {
            (va.vaQueryConfigEntrypoints)(
                display,
                VAProfileH264High,
                entrypoints.as_mut_ptr(),
                &mut num_ep,
            );
        }
        let ep_slice = &entrypoints[..num_ep as usize];
        let entrypoint = if ep_slice.contains(&VAEntrypointEncSliceLP) {
            VAEntrypointEncSliceLP
        } else if ep_slice.contains(&VAEntrypointEncSlice) {
            VAEntrypointEncSlice
        } else {
            unsafe {
                (va.vaTerminate)(display);
            }
            return Err("H.264 encode not supported on this VA-API device".into());
        };

        // Create config
        let mut config: VAConfigID = 0;
        let st = unsafe {
            (va.vaCreateConfig)(
                display,
                VAProfileH264High,
                entrypoint,
                ptr::null_mut(),
                0,
                &mut config,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateConfig failed: {st}"));
        }

        // Create surfaces: 2 reference + 1 input
        let mut surfaces = [0u32; TOTAL_SURFACES];
        let st = unsafe {
            (va.vaCreateSurfaces)(
                display,
                VA_RT_FORMAT_YUV420,
                width,
                height,
                surfaces.as_mut_ptr(),
                TOTAL_SURFACES as u32,
                ptr::null_mut(),
                0,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroyConfig)(display, config);
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateSurfaces failed: {st}"));
        }

        // Create context
        let mut context: VAContextID = 0;
        let st = unsafe {
            (va.vaCreateContext)(
                display,
                config,
                width as i32,
                height as i32,
                0x00000002, // VA_PROGRESSIVE
                surfaces.as_mut_ptr(),
                TOTAL_SURFACES as i32,
                &mut context,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroySurfaces)(display, surfaces.as_mut_ptr(), TOTAL_SURFACES as i32);
                (va.vaDestroyConfig)(display, config);
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateContext failed: {st}"));
        }

        // Coded buffer (output bitstream) — allocate generously
        let coded_buf_size = width * height; // ~1 byte per pixel is generous
        let mut coded_buf: VABufferID = 0;
        let st = unsafe {
            (va.vaCreateBuffer)(
                display,
                context,
                VAEncCodedBufferType,
                coded_buf_size,
                1,
                ptr::null_mut(),
                &mut coded_buf,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroyContext)(display, context);
                (va.vaDestroySurfaces)(display, surfaces.as_mut_ptr(), TOTAL_SURFACES as i32);
                (va.vaDestroyConfig)(display, config);
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateBuffer(coded) failed: {st}"));
        }

        let width_in_mbs = width.div_ceil(16) as u16;
        let height_in_mbs = height.div_ceil(16) as u16;

        if verbose {
            eprintln!(
                "[vaapi-direct] initialized H.264 CB encoder for {width}x{height} (ep={entrypoint})"
            );
        }
        Ok(Self {
            va,
            display,
            config,
            context,
            surfaces,
            coded_buf,
            width,
            height,
            width_in_mbs,
            height_in_mbs,
            frame_num: 0,
            idr_num: 0,
            force_idr: false,
            cur_ref_idx: 0,
            qp,
            _verbose: verbose,
            _drm_fd: drm_fd,
            vpp: unsafe { VppContext::try_new(va, display, width, height, verbose) },
        })
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    /// Encode directly from a DMA-BUF fd (zero-copy GPU path).
    ///
    /// Imports the DMA-BUF as a BGRA VASurface, uses the VPP to convert
    /// to NV12 on the GPU, then encodes the NV12 surface.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_dmabuf_fd(
        &mut self,
        fd: std::os::fd::RawFd,
        fourcc: u32,
        modifier: u64,
        stride: u32,
        offset: u32,
        src_width: u32,
        src_height: u32,
    ) -> Option<(Vec<u8>, bool)> {
        let vpp = self.vpp.as_mut()?;
        let nv12_surf = unsafe {
            vpp.convert_dmabuf(fd, fourcc, modifier, stride, offset, src_width, src_height)?
        };
        self.encode_surface(nv12_surf)
    }

    /// Encode from a pre-allocated VASurface (true zero-copy path).
    /// The surface was allocated by VPP and rendered into by the compositor
    /// via a shared DMA-BUF/EGL FBO.
    pub fn encode_va_surface(&mut self, surface_id: VASurfaceID) -> Option<(Vec<u8>, bool)> {
        let vpp = self.vpp.as_mut()?;
        let nv12_surf = unsafe { vpp.convert_surface(surface_id)? };
        self.encode_surface(nv12_surf)
    }

    /// Export VPP's pre-allocated BGRA surfaces as DMA-BUFs.
    pub fn export_vpp_surfaces(&self) -> Vec<ExportedVaSurface> {
        match &self.vpp {
            Some(vpp) => vpp.export_surfaces(),
            None => Vec::new(),
        }
    }

    /// Get the VADisplay as usize.
    pub fn va_display_usize(&self) -> usize {
        match &self.vpp {
            Some(vpp) => vpp.va_display_usize(),
            None => 0,
        }
    }

    /// Encode an NV12 frame (Y + UV interleaved planes).
    pub fn encode_nv12(
        &mut self,
        y_data: &[u8],
        uv_data: &[u8],
        y_stride: usize,
        uv_stride: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let input_surface = self.surfaces[NUM_REF_SURFACES]; // last surface is input

        // Upload NV12 data to the input surface
        self.upload_nv12(input_surface, y_data, uv_data, y_stride, uv_stride)?;
        self.encode_surface(input_surface)
    }

    /// Encode from BGRA pixels — converts to NV12 and uploads.
    pub fn encode_bgra_padded(
        &mut self,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let input_surface = self.surfaces[NUM_REF_SURFACES];

        // Convert BGRA→NV12 and upload to surface
        self.upload_bgra(input_surface, bgra, src_w, src_h)?;
        self.encode_surface(input_surface)
    }

    fn upload_nv12(
        &self,
        surface: VASurfaceID,
        y_data: &[u8],
        uv_data: &[u8],
        src_y_stride: usize,
        src_uv_stride: usize,
    ) -> Option<()> {
        let mut image = [0u8; VA_IMAGE_SIZE];
        let st = unsafe {
            (self.va.vaDeriveImage)(self.display, surface, image.as_mut_ptr() as *mut c_void)
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }

        let image_id = r32(&image, VAIMG_ID_OFF);
        let buf_id = r32(&image, VAIMG_BUF_OFF);
        let y_pitch = r32(&image, VAIMG_PITCHES_OFF) as usize;
        let uv_pitch = r32(&image, VAIMG_PITCHES_OFF + 4) as usize;
        let y_offset = r32(&image, VAIMG_OFFSETS_OFF) as usize;
        let uv_offset = r32(&image, VAIMG_OFFSETS_OFF + 4) as usize;

        let mut map_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, buf_id, &mut map_ptr) };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (self.va.vaDestroyImage)(self.display, image_id);
            }
            return None;
        }

        let w = self.width as usize;
        let h = self.height as usize;
        let dst = map_ptr as *mut u8;
        unsafe {
            // Copy Y plane
            for row in 0..h {
                let sr = row.min(h - 1);
                let src_start = sr * src_y_stride;
                let dst_start = y_offset + row * y_pitch;
                let copy_len = w.min(y_data.len() - src_start);
                ptr::copy_nonoverlapping(
                    y_data.as_ptr().add(src_start),
                    dst.add(dst_start),
                    copy_len,
                );
            }
            // Copy UV plane
            let uv_h = h / 2;
            for row in 0..uv_h {
                let src_start = row * src_uv_stride;
                let dst_start = uv_offset + row * uv_pitch;
                let copy_len = w.min(uv_data.len() - src_start);
                ptr::copy_nonoverlapping(
                    uv_data.as_ptr().add(src_start),
                    dst.add(dst_start),
                    copy_len,
                );
            }
        }

        unsafe {
            (self.va.vaUnmapBuffer)(self.display, buf_id);
            (self.va.vaDestroyImage)(self.display, image_id);
        }
        Some(())
    }

    fn upload_bgra(
        &self,
        surface: VASurfaceID,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<()> {
        let mut image = [0u8; VA_IMAGE_SIZE];
        let st = unsafe {
            (self.va.vaDeriveImage)(self.display, surface, image.as_mut_ptr() as *mut c_void)
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }

        let image_id = r32(&image, VAIMG_ID_OFF);
        let buf_id = r32(&image, VAIMG_BUF_OFF);
        let y_pitch = r32(&image, VAIMG_PITCHES_OFF) as usize;
        let uv_pitch = r32(&image, VAIMG_PITCHES_OFF + 4) as usize;
        let y_offset = r32(&image, VAIMG_OFFSETS_OFF) as usize;
        let uv_offset = r32(&image, VAIMG_OFFSETS_OFF + 4) as usize;

        let mut map_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, buf_id, &mut map_ptr) };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (self.va.vaDestroyImage)(self.display, image_id);
            }
            return None;
        }

        let enc_w = self.width as usize;
        let enc_h = self.height as usize;
        let dst = map_ptr as *mut u8;

        // BGRA→NV12 directly into mapped surface memory
        unsafe {
            // Y plane
            for row in 0..enc_h {
                let sr = row.min(src_h - 1);
                let dst_row = dst.add(y_offset + row * y_pitch);
                for col in 0..enc_w {
                    let sc = col.min(src_w - 1);
                    let i = (sr * src_w + sc) * 4;
                    let r = bgra[i + 2] as i32;
                    let g = bgra[i + 1] as i32;
                    let b = bgra[i] as i32;
                    let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                    *dst_row.add(col) = y.clamp(0, 255) as u8;
                }
            }
            // UV plane (interleaved)
            let chroma_h = enc_h / 2;
            let chroma_w = enc_w / 2;
            for cy in 0..chroma_h {
                let dst_row = dst.add(uv_offset + cy * uv_pitch);
                for cx in 0..chroma_w {
                    let row = cy * 2;
                    let col = cx * 2;
                    let mut u_sum = 0i32;
                    let mut v_sum = 0i32;
                    for dy in 0..2usize {
                        for dx in 0..2usize {
                            let sr = (row + dy).min(src_h - 1);
                            let sc = (col + dx).min(src_w - 1);
                            let i = (sr * src_w + sc) * 4;
                            let r = bgra[i + 2] as i32;
                            let g = bgra[i + 1] as i32;
                            let b = bgra[i] as i32;
                            u_sum += ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                            v_sum += ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                        }
                    }
                    *dst_row.add(cx * 2) = (u_sum / 4).clamp(0, 255) as u8;
                    *dst_row.add(cx * 2 + 1) = (v_sum / 4).clamp(0, 255) as u8;
                }
            }
        }

        unsafe {
            (self.va.vaUnmapBuffer)(self.display, buf_id);
            (self.va.vaDestroyImage)(self.display, image_id);
        }
        Some(())
    }

    fn encode_surface(&mut self, input_surface: VASurfaceID) -> Option<(Vec<u8>, bool)> {
        let is_idr = self.force_idr || self.frame_num == 0;
        if is_idr {
            self.frame_num = 0;
            self.idr_num += 1;
            self.force_idr = false;
        }

        let ref_surface = self.surfaces[self.cur_ref_idx];
        let recon_idx = (self.cur_ref_idx + 1) % NUM_REF_SURFACES;
        let recon_surface = self.surfaces[recon_idx];

        // Submit parameter buffers
        let sps_buf = self.create_sps_buffer()?;
        let pps_buf = self.create_pps_buffer(is_idr, ref_surface, recon_surface)?;
        let slice_buf = self.create_slice_buffer(is_idr, ref_surface)?;

        let mut buffers: Vec<VABufferID> = vec![sps_buf, pps_buf, slice_buf];

        // Submit packed SPS + PPS NALs on IDR frames so the driver includes
        // them in the coded buffer.  This replaces the manual prepend that
        // used synthetic NAL builders.
        if is_idr {
            let sps_nal = build_h264_sps_nal(
                self.width_in_mbs,
                self.height_in_mbs,
                self.width,
                self.height,
            );
            let pps_nal = build_h264_pps_nal();
            if let Some((p, d)) = create_packed_header_buffers(
                self.va,
                self.display,
                self.context,
                VA_ENC_PACKED_HEADER_SEQUENCE,
                &sps_nal,
            ) {
                buffers.push(p);
                buffers.push(d);
            }
            if let Some((p, d)) = create_packed_header_buffers(
                self.va,
                self.display,
                self.context,
                VA_ENC_PACKED_HEADER_PICTURE,
                &pps_nal,
            ) {
                buffers.push(p);
                buffers.push(d);
            }
        }

        let st = unsafe { (self.va.vaBeginPicture)(self.display, self.context, input_surface) };
        if st != VA_STATUS_SUCCESS {
            self.destroy_buffers(&buffers);
            return None;
        }

        let st = unsafe {
            (self.va.vaRenderPicture)(
                self.display,
                self.context,
                buffers.as_mut_ptr(),
                buffers.len() as i32,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (self.va.vaEndPicture)(self.display, self.context);
            }
            self.destroy_buffers(&buffers);
            return None;
        }

        let st = unsafe { (self.va.vaEndPicture)(self.display, self.context) };
        if st != VA_STATUS_SUCCESS {
            self.destroy_buffers(&buffers);
            return None;
        }

        // Wait for encode
        let st = unsafe { (self.va.vaSyncSurface)(self.display, input_surface) };
        if st != VA_STATUS_SUCCESS {
            self.destroy_buffers(&buffers);
            return None;
        }

        // Read bitstream
        let mut nal_data = self.read_coded_buffer()?;

        self.destroy_buffers(&buffers);

        // Update state
        self.frame_num += 1;
        self.cur_ref_idx = recon_idx;

        if nal_data.is_empty() {
            None
        } else {
            // AMD VA-API outputs slice NALs with header byte 0x00 instead of
            // the correct H.264 NAL header.  Patch the first NAL header.
            // Find the slice NAL (skip any SPS/PPS that the driver included
            // from our packed headers).
            let mut pos = 0;
            while let Some(sc) = find_annex_b_start(&nal_data[pos..]) {
                let abs = pos + sc;
                let hdr_pos = abs + if nal_data[abs + 2] == 1 { 3 } else { 4 };
                if hdr_pos < nal_data.len() {
                    let nal_type = nal_data[hdr_pos] & 0x1f;
                    // Only patch slice NALs (type 1 or 5), not SPS/PPS
                    if nal_type == 0 {
                        nal_data[hdr_pos] = if is_idr {
                            0x65 // nal_ref_idc=3, nal_unit_type=5 (IDR)
                        } else {
                            0x41 // nal_ref_idc=2, nal_unit_type=1 (non-IDR)
                        };
                    }
                }
                pos = hdr_pos + 1;
            }
            // Check if driver included SPS from packed headers.
            // If not, prepend SPS+PPS on IDR frames.
            if is_idr {
                let has_sps = {
                    let mut found = false;
                    let mut p = 0;
                    while let Some(sc) = find_annex_b_start(&nal_data[p..]) {
                        let abs = p + sc;
                        let hp = abs + if nal_data[abs + 2] == 1 { 3 } else { 4 };
                        if hp < nal_data.len() && (nal_data[hp] & 0x1f) == 7 {
                            found = true;
                            break;
                        }
                        p = hp + 1;
                    }
                    found
                };
                if !has_sps {
                    let mut out = build_h264_sps_nal(
                        self.width_in_mbs,
                        self.height_in_mbs,
                        self.width,
                        self.height,
                    );
                    out.extend_from_slice(&build_h264_pps_nal());
                    out.extend_from_slice(&nal_data);
                    return Some((out, true));
                }
            }
            Some((nal_data, is_idr))
        }
    }

    fn create_sps_buffer(&self) -> Option<VABufferID> {
        let mut sps = [0u8; SPS_SIZE];

        // seq_parameter_set_id (offset 0, u8)
        w8(&mut sps, 0, 0);
        // level_idc — pick the minimum H.264 level that can handle the
        // configured resolution (MaxFS = width_mbs * height_mbs).
        let max_fs = self.width_in_mbs as u32 * self.height_in_mbs as u32;
        let level_idc: u8 = if max_fs <= 1620 {
            31 // Level 3.1: 1280×720
        } else if max_fs <= 8192 {
            40 // Level 4.0: 2048×1080
        } else if max_fs <= 22080 {
            50 // Level 5.0: 3672×1536
        } else if max_fs <= 36864 {
            51 // Level 5.1: 4096×2160
        } else {
            52 // Level 5.2: 4096×2304
        };
        w8(&mut sps, 1, level_idc);
        // intra_period (offset 4, u32)
        w32(&mut sps, 4, 120);
        // intra_idr_period (offset 8, u32)
        w32(&mut sps, 8, 120);
        // ip_period (offset 12, u32)
        w32(&mut sps, 12, 1);
        // bits_per_second (offset 16, u32)
        w32(&mut sps, 16, 0); // VBR
        // max_num_ref_frames (offset 20, u32)
        w32(&mut sps, 20, 1);
        // picture_width_in_mbs (offset 24, u16)
        w16(&mut sps, 24, self.width_in_mbs);
        // picture_height_in_mbs (offset 26, u16)
        w16(&mut sps, 26, self.height_in_mbs);

        // seq_fields (offset 28, u32 bitfield):
        //   chroma_format_idc: bits 0-1 = 1 (4:2:0)
        //   frame_mbs_only_flag: bit 2 = 1
        //   direct_8x8_inference_flag: bit 5 = 1
        //   log2_max_frame_num_minus4: bits 6-9 = 0
        //   pic_order_cnt_type: bits 10-11 = 2
        //   log2_max_pic_order_cnt_lsb_minus4: bits 12-15 = 0
        let seq_fields: u32 = 1         // chroma_format_idc = 1
            | (1 << 2)                   // frame_mbs_only_flag
            | (1 << 5)                   // direct_8x8_inference_flag
            | (0 << 6)                   // log2_max_frame_num_minus4 = 0
            | (2 << 10); // pic_order_cnt_type = 2
        w32(&mut sps, 28, seq_fields);

        // Frame cropping for odd dimensions
        let crop_w = self.width_in_mbs as u32 * 16;
        let crop_h = self.height_in_mbs as u32 * 16;
        if crop_w != self.width || crop_h != self.height {
            w8(&mut sps, 1068, 1); // frame_cropping_flag (offset 1068, u8... actually it's at a weird offset)
            // frame_crop_right_offset (offset 1076, u32) — in chroma samples
            w32(&mut sps, 1076, (crop_w - self.width) / 2);
            // frame_crop_bottom_offset (offset 1084, u32)
            w32(&mut sps, 1084, (crop_h - self.height) / 2);
        }

        let mut buf_id: VABufferID = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncSequenceParameterBufferType,
                SPS_SIZE as u32,
                1,
                sps.as_mut_ptr() as *mut c_void,
                &mut buf_id,
            )
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        Some(buf_id)
    }

    fn create_pps_buffer(
        &self,
        is_idr: bool,
        ref_surface: VASurfaceID,
        recon_surface: VASurfaceID,
    ) -> Option<VABufferID> {
        let mut pps = [0u8; PPS_SIZE];

        // CurrPic (offset 0, VAPictureH264 — 36 bytes)
        // CurrPic.picture_id = recon_surface (the reconstructed output)
        w32(&mut pps, 0, recon_surface);
        // CurrPic.TopFieldOrderCnt
        w32(&mut pps, 12, (self.frame_num as u32) * 2);

        // ReferenceFrames[0..16] (offset 36, 16 × VAPictureH264)
        // Initialize all to invalid
        for i in 0..16 {
            let off = 36 + i * 36;
            w32(&mut pps, off, VA_INVALID_SURFACE); // picture_id
            w32(&mut pps, off + 8, VA_PICTURE_H264_INVALID); // flags
        }
        // Set ReferenceFrames[0] to the reference surface (for P-frames)
        if !is_idr && self.frame_num > 0 {
            w32(&mut pps, 36, ref_surface);
            w32(&mut pps, 36 + 8, 0); // flags = 0 (short-term ref, frame)
            w32(&mut pps, 36 + 12, ((self.frame_num - 1) as u32) * 2); // TopFieldOrderCnt
        }

        // coded_buf (offset 612, VABufferID)
        w32(&mut pps, 612, self.coded_buf);
        // pic_parameter_set_id (offset 616, u8)
        w8(&mut pps, 616, 0);
        // seq_parameter_set_id (offset 617, u8)
        w8(&mut pps, 617, 0);
        // frame_num (offset 620, u16)
        w16(&mut pps, 620, self.frame_num);
        // pic_init_qp (offset 622, u8)
        w8(&mut pps, 622, 26);
        // num_ref_idx_l0_active_minus1 (offset 623, u8)
        w8(&mut pps, 623, 0);

        // pic_fields (offset 628, u32 bitfield):
        //   idr_pic_flag: bit 0
        //   reference_pic_flag: bits 1-2 = 1
        //   entropy_coding_mode_flag: bit 3 = 1 (CABAC)
        //   transform_8x8_mode_flag: bit 8 = 1
        //   deblocking_filter_control_present_flag: bit 9 = 1
        let mut pic_fields: u32 = 0;
        if is_idr {
            pic_fields |= 1; // idr_pic_flag
        }
        pic_fields |= 1 << 1; // reference_pic_flag = 1
        pic_fields |= 1 << 3; // entropy_coding_mode_flag = CABAC
        pic_fields |= 1 << 8; // transform_8x8_mode_flag
        pic_fields |= 1 << 9; // deblocking_filter_control_present_flag
        w32(&mut pps, 628, pic_fields);

        let mut buf_id: VABufferID = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncPictureParameterBufferType,
                PPS_SIZE as u32,
                1,
                pps.as_mut_ptr() as *mut c_void,
                &mut buf_id,
            )
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        Some(buf_id)
    }

    fn create_slice_buffer(&self, is_idr: bool, ref_surface: VASurfaceID) -> Option<VABufferID> {
        let mut slice = [0u8; SLICE_SIZE];

        let num_mbs = self.width_in_mbs as u32 * self.height_in_mbs as u32;

        // macroblock_address (offset 0, u32)
        w32(&mut slice, 0, 0);
        // num_macroblocks (offset 4, u32)
        w32(&mut slice, 4, num_mbs);
        // slice_type (offset 12, u8): 2 = I, 0 = P
        w8(&mut slice, 12, if is_idr { 2 } else { 0 });

        // RefPicList0[0..32] (offset 36, 32 × VAPictureH264)
        // Initialize all to invalid
        for i in 0..32 {
            let off = 36 + i * 36;
            w32(&mut slice, off, VA_INVALID_SURFACE);
            w32(&mut slice, off + 8, VA_PICTURE_H264_INVALID);
        }
        // RefPicList1[0..32] (offset 1188, 32 × VAPictureH264)
        for i in 0..32 {
            let off = 1188 + i * 36;
            w32(&mut slice, off, VA_INVALID_SURFACE);
            w32(&mut slice, off + 8, VA_PICTURE_H264_INVALID);
        }
        // Set RefPicList0[0] for P-frames
        if !is_idr && self.frame_num > 0 {
            w32(&mut slice, 36, ref_surface);
            w32(&mut slice, 36 + 8, 0);
            w32(&mut slice, 36 + 12, ((self.frame_num - 1) as u32) * 2);
        }

        // slice_qp_delta (offset 3119, i8)
        slice[3119] = (self.qp as i8 - 26) as u8; // QP=self.qp, pic_init_qp=26

        let mut buf_id: VABufferID = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncSliceParameterBufferType,
                SLICE_SIZE as u32,
                1,
                slice.as_mut_ptr() as *mut c_void,
                &mut buf_id,
            )
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        Some(buf_id)
    }

    fn read_coded_buffer(&self) -> Option<Vec<u8>> {
        let mut buf_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, self.coded_buf, &mut buf_ptr) };
        if st != VA_STATUS_SUCCESS {
            return None;
        }

        let mut nal_data = Vec::new();
        let mut seg_ptr = buf_ptr as *const u8;
        loop {
            if seg_ptr.is_null() {
                break;
            }
            let size = unsafe { u32::from_ne_bytes(*(seg_ptr as *const [u8; 4])) } as usize;
            let data_ptr = unsafe {
                let p = seg_ptr.add(CBS_BUF_OFF);
                *(p as *const *const u8)
            };
            if !data_ptr.is_null() && size > 0 {
                let data = unsafe { std::slice::from_raw_parts(data_ptr, size) };
                nal_data.extend_from_slice(data);
            }
            let next = unsafe {
                let p = seg_ptr.add(CBS_NEXT_OFF);
                *(p as *const *const u8)
            };
            seg_ptr = next;
        }

        unsafe {
            (self.va.vaUnmapBuffer)(self.display, self.coded_buf);
        }
        Some(nal_data)
    }

    fn destroy_buffers(&self, buffers: &[VABufferID]) {
        for &buf in buffers {
            unsafe {
                (self.va.vaDestroyBuffer)(self.display, buf);
            }
        }
    }
}

impl Drop for VaapiDirectEncoder {
    fn drop(&mut self) {
        // Drop VPP context first — it shares our VA display handle and must
        // be destroyed before vaTerminate() invalidates the display.
        self.vpp.take();
        unsafe {
            (self.va.vaDestroyBuffer)(self.display, self.coded_buf);
            (self.va.vaDestroyContext)(self.display, self.context);
            (self.va.vaDestroySurfaces)(
                self.display,
                self.surfaces.as_mut_ptr(),
                TOTAL_SURFACES as i32,
            );
            (self.va.vaDestroyConfig)(self.display, self.config);
            (self.va.vaTerminate)(self.display);
        }
    }
}

// ---------------------------------------------------------------------------
// VA-API AV1 encoder
// ---------------------------------------------------------------------------
//
// Intentionally minimal: all-intra, single-tile-group, profile 0,
// 8-bit 4:2:0, no rate control. The output is AV1 low-overhead OBU stream,
// matching the existing software AV1 path.

const VAProfileAV1Profile0: i32 = 32;

const VA_PADDING_LOW: usize = 4;
const VA_PADDING_HIGH: usize = 16;

const AV1_NUM_SURFACES: usize = 3; // 2 recon (double-buffered ref) + 1 input

#[repr(C)]
struct Av1PackedHeaderParamBuffer {
    type_: u32,
    bit_length: u32,
    has_emulation_bytes: u8,
    _pad: [u8; 3],
    va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
struct VAEncSequenceParameterBufferAV1 {
    seq_profile: u8,
    seq_level_idx: u8,
    seq_tier: u8,
    hierarchical_flag: u8,
    intra_period: u32,
    ip_period: u32,
    bits_per_second: u32,
    seq_fields: u32,
    order_hint_bits_minus_1: u8,
    _pad0: [u8; 3],
    va_reserved: [u32; VA_PADDING_HIGH],
}

#[repr(C)]
struct VARefFrameCtrlAV1 {
    value: u32,
}

#[repr(C)]
struct VAEncSegParamAV1 {
    seg_flags: u8,
    segment_number: u8,
    _pad0: [u8; 2],
    feature_data: [[i16; 8]; 8],
    feature_mask: [u8; 8],
    va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
struct VAEncWarpedMotionParamsAV1 {
    wmtype: i32,
    wmmat: [i32; 8],
    invalid: u8,
    _pad0: [u8; 3],
    va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
struct VAEncPictureParameterBufferAV1 {
    frame_width_minus_1: u16,
    frame_height_minus_1: u16,
    reconstructed_frame: VASurfaceID,
    coded_buf: VABufferID,
    reference_frames: [VASurfaceID; 8],
    ref_frame_idx: [u8; 7],
    hierarchical_level_plus_1: u8,
    primary_ref_frame: u8,
    order_hint: u8,
    refresh_frame_flags: u8,
    reserved8bits1: u8,
    ref_frame_ctrl_l0: VARefFrameCtrlAV1,
    ref_frame_ctrl_l1: VARefFrameCtrlAV1,
    picture_flags: u32,
    seg_id_block_size: u8,
    num_tile_groups_minus1: u8,
    temporal_id: u8,
    filter_level: [u8; 2],
    filter_level_u: u8,
    filter_level_v: u8,
    loop_filter_flags: u8,
    superres_scale_denominator: u8,
    interpolation_filter: u8,
    ref_deltas: [i8; 8],
    mode_deltas: [i8; 2],
    base_qindex: u8,
    y_dc_delta_q: i8,
    u_dc_delta_q: i8,
    u_ac_delta_q: i8,
    v_dc_delta_q: i8,
    v_ac_delta_q: i8,
    min_base_qindex: u8,
    max_base_qindex: u8,
    qmatrix_flags: u16,
    reserved16bits1: u16,
    mode_control_flags: u32,
    segments: VAEncSegParamAV1,
    tile_cols: u8,
    tile_rows: u8,
    reserved16bits2: u16,
    width_in_sbs_minus_1: [u16; 63],
    height_in_sbs_minus_1: [u16; 63],
    context_update_tile_id: u16,
    cdef_damping_minus_3: u8,
    cdef_bits: u8,
    cdef_y_strengths: [u8; 8],
    cdef_uv_strengths: [u8; 8],
    loop_restoration_flags: u16,
    wm: [VAEncWarpedMotionParamsAV1; 7],
    bit_offset_qindex: u32,
    bit_offset_segmentation: u32,
    bit_offset_loopfilter_params: u32,
    bit_offset_cdef_params: u32,
    size_in_bits_cdef_params: u32,
    byte_offset_frame_hdr_obu_size: u32,
    size_in_bits_frame_hdr_obu: u32,
    tile_group_obu_hdr_info: u8,
    number_skip_frames: u8,
    reserved16bits3: u16,
    skip_frames_reduced_size: i32,
    va_reserved: [u32; VA_PADDING_HIGH],
}

#[repr(C)]
struct VAEncTileGroupBufferAV1 {
    tg_start: u8,
    tg_end: u8,
    _pad0: [u8; 2],
    va_reserved: [u32; VA_PADDING_LOW],
}

struct PackedData {
    writes: Vec<(u64, usize)>,
    outstanding_bits: usize,
}

impl PackedData {
    fn new() -> Self {
        Self {
            writes: Vec::new(),
            outstanding_bits: 0,
        }
    }
    fn write(&mut self, val: u64, bits: usize) {
        self.writes.push((val, bits));
        self.outstanding_bits += bits;
    }
    fn write_bool(&mut self, val: bool) {
        self.write(u64::from(val), 1);
    }
    fn write_obu_header(&mut self, obu_type: u8, extension_flag: bool, has_size: bool) {
        self.write_bool(false);
        self.write(obu_type as u64, 4);
        self.write_bool(extension_flag);
        self.write_bool(has_size);
        self.write_bool(false);
    }
    fn encode_leb128(&mut self, mut value: u32, fixed_size: Option<usize>) {
        for i in 0..fixed_size.unwrap_or(5) {
            let mut cur = value & 0x7f;
            value >>= 7;
            if value != 0 || fixed_size.is_some() && i + 1 < fixed_size.unwrap() {
                cur |= 0x80;
            }
            self.write(cur as u64, 8);
            if value == 0 && fixed_size.is_none() {
                break;
            }
        }
    }
    fn flush(mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cur = 0u8;
        let mut rem = 8usize;
        for (val, mut bits) in self.writes.drain(..) {
            while bits > 0 {
                if rem >= bits {
                    // `rem >= bits` so the value fits in the current byte.
                    // Mask to `bits` width, shift into position.
                    let mask = if bits >= 64 {
                        u64::MAX
                    } else {
                        (1u64 << bits) - 1
                    };
                    cur |= ((val & mask) as u8) << (rem - bits);
                    rem -= bits;
                    bits = 0;
                } else {
                    // Extract the top `rem` bits from the remaining `bits` and
                    // place at position 0 of cur.
                    cur |= ((val >> (bits - rem)) as u8) & (((1u16 << rem) - 1) as u8);
                    bits -= rem;
                    rem = 0;
                }
                if rem == 0 {
                    out.push(cur);
                    cur = 0;
                    rem = 8;
                }
            }
        }
        if rem != 8 {
            out.push(cur);
        }
        out
    }
}

#[derive(Default)]
struct PicParamOffsets {
    q_idx_bit_offset: u32,
    segmentation_bit_offset: u32,
    loop_filter_params_bit_offset: u32,
    cdef_params_bit_offset: u32,
    cdef_params_size_bits: u32,
    frame_hdr_obu_size_byte_offset: u32,
    frame_hdr_obu_size_bits: u32,
}

fn compute_level(coded_w: u32, coded_h: u32, framerate: u32) -> u8 {
    let samples_per_second = coded_w as u64 * coded_h as u64 * framerate as u64;
    const SPECS: &[(u8, u32, u32, u64)] = &[
        (0, 2048, 1152, 5529600),
        (1, 2816, 1152, 10454400),
        (4, 4352, 2448, 24969600),
        (5, 5504, 3096, 39938400),
        (8, 6144, 3456, 77856768),
        (9, 6144, 3456, 155713536),
        (12, 8192, 4352, 273715200),
        (13, 8192, 4352, 547430400),
        (16, 16384, 8704, 1176502272),
    ];
    for &(level, max_w, max_h, max_rate) in SPECS {
        if coded_w <= max_w && coded_h <= max_h && samples_per_second <= max_rate {
            return level;
        }
    }
    16
}

pub struct VaapiAv1Encoder {
    va: &'static gpu_libs::VaFns,
    display: VADisplay,
    config: VAConfigID,
    context: VAContextID,
    surfaces: [VASurfaceID; AV1_NUM_SURFACES],
    coded_buf: VABufferID,
    width: u32,
    height: u32,
    source_width: u32,
    source_height: u32,
    level_idx: u8,
    frame_num: u32,
    force_idr: bool,
    cur_ref_idx: usize,
    base_qindex: u8,
    _verbose: bool,
    _drm_fd: OwnedFd,
    vpp: Option<VppContext>,
}

unsafe impl Send for VaapiAv1Encoder {}

impl VaapiAv1Encoder {
    pub fn try_new(
        width: u32,
        height: u32,
        source_width: u32,
        source_height: u32,
        vaapi_device: &str,
        base_qindex: u8,
        verbose: bool,
    ) -> Result<Self, String> {
        let va = gpu_libs::va().map_err(|e| format!("VA-API: {e}"))?;
        let va_drm = gpu_libs::va_drm().map_err(|e| format!("VA-DRM: {e}"))?;
        let drm_fd = {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(vaapi_device)
                .map_err(|e| format!("failed to open {vaapi_device}: {e}"))?;
            OwnedFd::from(file)
        };
        let display = unsafe { (va_drm.vaGetDisplayDRM)(drm_fd.as_raw_fd()) };
        if display.is_null() {
            return Err("vaGetDisplayDRM returned null".into());
        }
        let mut major = 0i32;
        let mut minor = 0i32;
        let st = unsafe { (va.vaInitialize)(display, &mut major, &mut minor) };
        if st != VA_STATUS_SUCCESS {
            return Err(format!("vaInitialize failed: {st}"));
        }

        let mut entrypoints = [0i32; 16];
        let mut num_ep = 0i32;
        unsafe {
            (va.vaQueryConfigEntrypoints)(
                display,
                VAProfileAV1Profile0,
                entrypoints.as_mut_ptr(),
                &mut num_ep,
            );
        }
        let ep_slice = &entrypoints[..num_ep as usize];
        let entrypoint = if ep_slice.contains(&VAEntrypointEncSliceLP) {
            VAEntrypointEncSliceLP
        } else if ep_slice.contains(&VAEntrypointEncSlice) {
            VAEntrypointEncSlice
        } else {
            unsafe {
                (va.vaTerminate)(display);
            }
            return Err("AV1 encode not supported on this VA-API device".into());
        };

        let mut config: VAConfigID = 0;
        let st = unsafe {
            (va.vaCreateConfig)(
                display,
                VAProfileAV1Profile0,
                entrypoint,
                ptr::null_mut(),
                0,
                &mut config,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateConfig(AV1) failed: {st}"));
        }

        let mut surfaces = [0u32; AV1_NUM_SURFACES];
        let st = unsafe {
            (va.vaCreateSurfaces)(
                display,
                VA_RT_FORMAT_YUV420,
                width,
                height,
                surfaces.as_mut_ptr(),
                AV1_NUM_SURFACES as u32,
                ptr::null_mut(),
                0,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroyConfig)(display, config);
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateSurfaces(AV1) failed: {st}"));
        }

        let mut context: VAContextID = 0;
        let st = unsafe {
            (va.vaCreateContext)(
                display,
                config,
                width as i32,
                height as i32,
                0x00000002,
                surfaces.as_mut_ptr(),
                AV1_NUM_SURFACES as i32,
                &mut context,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroySurfaces)(display, surfaces.as_mut_ptr(), AV1_NUM_SURFACES as i32);
                (va.vaDestroyConfig)(display, config);
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateContext(AV1) failed: {st}"));
        }

        let mut coded_buf: VABufferID = 0;
        let st = unsafe {
            (va.vaCreateBuffer)(
                display,
                context,
                VAEncCodedBufferType,
                width * height * 2,
                1,
                ptr::null_mut(),
                &mut coded_buf,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroyContext)(display, context);
                (va.vaDestroySurfaces)(display, surfaces.as_mut_ptr(), AV1_NUM_SURFACES as i32);
                (va.vaDestroyConfig)(display, config);
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateBuffer(coded,AV1) failed: {st}"));
        }

        let level_idx = compute_level(width, height, 60);
        if verbose {
            eprintln!(
                "[vaapi-direct] initialized AV1 Profile0 encoder for {width}x{height} (ep={entrypoint})"
            );
        }
        // VPP at encoder (padded) dimensions — the NV12 surfaces must match
        // the encoder context's resolution (64-pixel aligned for AV1).
        // Using source dimensions here would produce undersized NV12 surfaces
        // causing bottom-of-frame corruption and potential encoder hangs on
        // AMD VAAPI drivers.
        let vpp = unsafe { VppContext::try_new(va, display, width, height, verbose) };

        Ok(Self {
            va,
            display,
            config,
            context,
            surfaces,
            coded_buf,
            width,
            height,
            source_width,
            source_height,
            level_idx,
            frame_num: 0,
            force_idr: false,
            cur_ref_idx: 0,
            base_qindex,
            _verbose: verbose,
            _drm_fd: drm_fd,
            vpp,
        })
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    /// Encode directly from a DMA-BUF fd (zero-copy GPU path).
    #[allow(clippy::too_many_arguments)]
    pub fn encode_dmabuf_fd(
        &mut self,
        fd: std::os::fd::RawFd,
        fourcc: u32,
        modifier: u64,
        stride: u32,
        offset: u32,
        src_width: u32,
        src_height: u32,
    ) -> Option<(Vec<u8>, bool)> {
        let vpp = self.vpp.as_mut()?;
        let nv12_surf = unsafe {
            vpp.convert_dmabuf(fd, fourcc, modifier, stride, offset, src_width, src_height)?
        };
        self.encode_surface(nv12_surf)
    }

    /// Encode from a pre-allocated VASurface (true zero-copy path).
    pub fn encode_va_surface(&mut self, surface_id: VASurfaceID) -> Option<(Vec<u8>, bool)> {
        let vpp = self.vpp.as_mut()?;
        let nv12_surf = unsafe { vpp.convert_surface(surface_id)? };
        self.encode_surface(nv12_surf)
    }

    pub fn export_vpp_surfaces(&self) -> Vec<ExportedVaSurface> {
        match &self.vpp {
            Some(vpp) => vpp.export_surfaces(),
            None => Vec::new(),
        }
    }

    pub fn va_display_usize(&self) -> usize {
        match &self.vpp {
            Some(vpp) => vpp.va_display_usize(),
            None => 0,
        }
    }

    pub fn encode_nv12(
        &mut self,
        y_data: &[u8],
        uv_data: &[u8],
        y_stride: usize,
        uv_stride: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let input_surface = self.surfaces[2]; // index 0,1 = recon, 2 = input
        self.upload_nv12(input_surface, y_data, uv_data, y_stride, uv_stride)?;
        self.encode_surface(input_surface)
    }

    pub fn encode_bgra_padded(
        &mut self,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let input_surface = self.surfaces[2]; // index 0,1 = recon, 2 = input
        self.upload_bgra(input_surface, bgra, src_w, src_h)?;
        self.encode_surface(input_surface)
    }

    fn upload_nv12(
        &self,
        surface: VASurfaceID,
        y_data: &[u8],
        uv_data: &[u8],
        src_y_stride: usize,
        src_uv_stride: usize,
    ) -> Option<()> {
        let mut image = [0u8; VA_IMAGE_SIZE];
        let st = unsafe {
            (self.va.vaDeriveImage)(self.display, surface, image.as_mut_ptr() as *mut c_void)
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        let image_id = r32(&image, VAIMG_ID_OFF);
        let buf_id = r32(&image, VAIMG_BUF_OFF);
        let y_pitch = r32(&image, VAIMG_PITCHES_OFF) as usize;
        let uv_pitch = r32(&image, VAIMG_PITCHES_OFF + 4) as usize;
        let y_offset = r32(&image, VAIMG_OFFSETS_OFF) as usize;
        let uv_offset = r32(&image, VAIMG_OFFSETS_OFF + 4) as usize;
        let mut map_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, buf_id, &mut map_ptr) };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (self.va.vaDestroyImage)(self.display, image_id);
            }
            return None;
        }
        let w = self.width as usize;
        let h = self.height as usize;
        let dst = map_ptr as *mut u8;
        unsafe {
            for row in 0..h {
                let sr = row.min(h - 1);
                let src_start = sr * src_y_stride;
                let dst_start = y_offset + row * y_pitch;
                let copy_len = w.min(y_data.len() - src_start);
                ptr::copy_nonoverlapping(
                    y_data.as_ptr().add(src_start),
                    dst.add(dst_start),
                    copy_len,
                );
            }
            let uv_h = h / 2;
            for row in 0..uv_h {
                let src_start = row * src_uv_stride;
                let dst_start = uv_offset + row * uv_pitch;
                let copy_len = w.min(uv_data.len() - src_start);
                ptr::copy_nonoverlapping(
                    uv_data.as_ptr().add(src_start),
                    dst.add(dst_start),
                    copy_len,
                );
            }
            (self.va.vaUnmapBuffer)(self.display, buf_id);
            (self.va.vaDestroyImage)(self.display, image_id);
        }
        Some(())
    }

    fn upload_bgra(
        &self,
        surface: VASurfaceID,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<()> {
        let mut image = [0u8; VA_IMAGE_SIZE];
        let st = unsafe {
            (self.va.vaDeriveImage)(self.display, surface, image.as_mut_ptr() as *mut c_void)
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        let image_id = r32(&image, VAIMG_ID_OFF);
        let buf_id = r32(&image, VAIMG_BUF_OFF);
        let y_pitch = r32(&image, VAIMG_PITCHES_OFF) as usize;
        let uv_pitch = r32(&image, VAIMG_PITCHES_OFF + 4) as usize;
        let y_offset = r32(&image, VAIMG_OFFSETS_OFF) as usize;
        let uv_offset = r32(&image, VAIMG_OFFSETS_OFF + 4) as usize;
        let mut map_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, buf_id, &mut map_ptr) };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (self.va.vaDestroyImage)(self.display, image_id);
            }
            return None;
        }
        let enc_w = self.width as usize;
        let enc_h = self.height as usize;
        let dst = map_ptr as *mut u8;
        unsafe {
            for row in 0..enc_h {
                let sr = row.min(src_h - 1);
                let dst_row = dst.add(y_offset + row * y_pitch);
                for col in 0..enc_w {
                    let sc = col.min(src_w - 1);
                    let i = (sr * src_w + sc) * 4;
                    let r = bgra[i + 2] as i32;
                    let g = bgra[i + 1] as i32;
                    let b = bgra[i] as i32;
                    let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                    *dst_row.add(col) = y.clamp(0, 255) as u8;
                }
            }
            let chroma_h = enc_h / 2;
            let chroma_w = enc_w / 2;
            for cy in 0..chroma_h {
                let dst_row = dst.add(uv_offset + cy * uv_pitch);
                for cx in 0..chroma_w {
                    let row = cy * 2;
                    let col = cx * 2;
                    let mut u_sum = 0i32;
                    let mut v_sum = 0i32;
                    for dy in 0..2usize {
                        for dx in 0..2usize {
                            let sr = (row + dy).min(src_h - 1);
                            let sc = (col + dx).min(src_w - 1);
                            let i = (sr * src_w + sc) * 4;
                            let r = bgra[i + 2] as i32;
                            let g = bgra[i + 1] as i32;
                            let b = bgra[i] as i32;
                            u_sum += ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                            v_sum += ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                        }
                    }
                    *dst_row.add(cx * 2) = (u_sum / 4).clamp(0, 255) as u8;
                    *dst_row.add(cx * 2 + 1) = (v_sum / 4).clamp(0, 255) as u8;
                }
            }
            (self.va.vaUnmapBuffer)(self.display, buf_id);
            (self.va.vaDestroyImage)(self.display, image_id);
        }
        Some(())
    }

    fn encode_surface(&mut self, input_surface: VASurfaceID) -> Option<(Vec<u8>, bool)> {
        let is_key = self.force_idr || self.frame_num == 0;
        if is_key {
            self.frame_num = 0;
            self.force_idr = false;
        }
        let mut pic_offsets = PicParamOffsets::default();
        let seq_param = self.create_sequence_param();
        let td = self.temporal_delimiter_obu();
        let seq_hdr = self.sequence_header_obu();
        let mut pic_param = self.make_picture_param(is_key);

        // Byte offset from start of all packed data to the frame OBU size field.
        // Chromium: offsets.frame_hdr_obu_size_byte_offset = td.len() [+ seq.len()]
        pic_offsets.frame_hdr_obu_size_byte_offset = td.len() as u32;
        if is_key {
            pic_offsets.frame_hdr_obu_size_byte_offset += seq_hdr.len() as u32;
        }

        let frame_obu = self.frame_obu(&pic_param, &mut pic_offsets);

        // Bit offsets are relative to the start of ALL packed data, not just
        // the frame OBU.  Add the preceding OBU bytes.
        let preceding_bits = (td.len() as u32 + if is_key { seq_hdr.len() as u32 } else { 0 }) * 8;
        pic_param.bit_offset_qindex = pic_offsets.q_idx_bit_offset + preceding_bits;
        pic_param.bit_offset_segmentation = pic_offsets.segmentation_bit_offset + preceding_bits;
        pic_param.bit_offset_loopfilter_params =
            pic_offsets.loop_filter_params_bit_offset + preceding_bits;
        pic_param.bit_offset_cdef_params = pic_offsets.cdef_params_bit_offset + preceding_bits;
        pic_param.size_in_bits_cdef_params = pic_offsets.cdef_params_size_bits;
        pic_param.byte_offset_frame_hdr_obu_size = pic_offsets.frame_hdr_obu_size_byte_offset;
        pic_param.size_in_bits_frame_hdr_obu = pic_offsets.frame_hdr_obu_size_bits + preceding_bits;
        let tile_group = VAEncTileGroupBufferAV1 {
            tg_start: 0,
            tg_end: 0,
            _pad0: [0; 2],
            va_reserved: [0; VA_PADDING_LOW],
        };

        let mut buffer_ids = Vec::new();
        buffer_ids.push(self.create_buffer(VAEncSequenceParameterBufferType, &seq_param)?);
        buffer_ids.extend(self.create_av1_packed_buffers(&td)?);
        if is_key {
            buffer_ids.extend(self.create_av1_packed_buffers(&seq_hdr)?);
        }
        buffer_ids.extend(self.create_av1_packed_buffers(&frame_obu)?);
        buffer_ids.push(self.create_buffer(VAEncPictureParameterBufferType, &pic_param)?);
        buffer_ids.push(self.create_buffer(VAEncSliceParameterBufferType, &tile_group)?);

        let st = unsafe { (self.va.vaBeginPicture)(self.display, self.context, input_surface) };
        if st != VA_STATUS_SUCCESS {
            self.destroy_buffers(&buffer_ids);
            return None;
        }
        let st = unsafe {
            (self.va.vaRenderPicture)(
                self.display,
                self.context,
                buffer_ids.as_mut_ptr(),
                buffer_ids.len() as i32,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (self.va.vaEndPicture)(self.display, self.context);
            }
            self.destroy_buffers(&buffer_ids);
            return None;
        }
        let st = unsafe { (self.va.vaEndPicture)(self.display, self.context) };
        if st != VA_STATUS_SUCCESS {
            self.destroy_buffers(&buffer_ids);
            return None;
        }
        let st = unsafe { (self.va.vaSyncSurface)(self.display, input_surface) };
        if st != VA_STATUS_SUCCESS {
            self.destroy_buffers(&buffer_ids);
            return None;
        }
        let coded = self.read_coded_buffer();
        self.destroy_buffers(&buffer_ids);
        self.frame_num = self.frame_num.wrapping_add(1);

        // The packed header buffers (TD, seq header, frame OBU) we submitted
        // via vaRenderPicture should be included in the coded buffer by the
        // driver, with bit offsets patched to reflect actual encoding params.
        // Return the coded buffer contents directly — the driver builds the
        // complete AV1 low-overhead bitstream for us.
        coded.map(|data| {
            // Advance reference: the reconstructed surface becomes the next ref
            let recon_idx = if is_key {
                0
            } else {
                (self.cur_ref_idx + 1) % 2
            };
            self.cur_ref_idx = recon_idx;

            (data, is_key)
        })
    }

    fn create_buffer<T>(&self, ty: i32, obj: &T) -> Option<VABufferID> {
        let mut id = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                ty,
                std::mem::size_of::<T>() as u32,
                1,
                obj as *const _ as *mut c_void,
                &mut id,
            )
        };
        (st == VA_STATUS_SUCCESS).then_some(id)
    }

    fn create_av1_packed_buffers(&self, data: &[u8]) -> Option<Vec<VABufferID>> {
        let param = Av1PackedHeaderParamBuffer {
            type_: VA_ENC_PACKED_HEADER_PICTURE,
            bit_length: (data.len() * 8) as u32,
            has_emulation_bytes: 0,
            _pad: [0; 3],
            va_reserved: [0; VA_PADDING_LOW],
        };
        let mut p = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncPackedHeaderParameterBufferType,
                std::mem::size_of::<Av1PackedHeaderParamBuffer>() as u32,
                1,
                &param as *const _ as *mut c_void,
                &mut p,
            )
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        let mut d = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncPackedHeaderDataBufferType,
                data.len() as u32,
                1,
                data.as_ptr() as *mut c_void,
                &mut d,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (self.va.vaDestroyBuffer)(self.display, p);
            }
            return None;
        }
        Some(vec![p, d])
    }

    fn create_sequence_param(&self) -> VAEncSequenceParameterBufferAV1 {
        let mut seq: VAEncSequenceParameterBufferAV1 = unsafe { std::mem::zeroed() };
        seq.seq_profile = 0;
        seq.seq_level_idx = self.level_idx;
        seq.seq_tier = 0;
        seq.hierarchical_flag = 0;
        seq.intra_period = 120;
        seq.ip_period = 1;
        seq.bits_per_second = 0;
        seq.order_hint_bits_minus_1 = 7;
        // seq_fields: enable_order_hint(bit 8)=1, enable_cdef(bit 12)=1,
        //             subsampling_x(bit 17)=1, subsampling_y(bit 18)=1
        seq.seq_fields = (1 << 8) | (1 << 12) | (1 << 17) | (1 << 18);
        seq
    }

    fn destroy_buffers(&self, bufs: &[VABufferID]) {
        for &b in bufs {
            unsafe {
                (self.va.vaDestroyBuffer)(self.display, b);
            }
        }
    }

    fn temporal_delimiter_obu(&self) -> Vec<u8> {
        let mut p = PackedData::new();
        p.write_obu_header(2, false, true);
        p.encode_leb128(0, None);
        p.flush()
    }

    fn sequence_header_obu(&self) -> Vec<u8> {
        let packed = self.pack_sequence_header();
        let mut obu = PackedData::new();
        obu.write_obu_header(1, false, true);
        obu.encode_leb128(packed.len() as u32, None);
        let mut out = obu.flush();
        out.extend_from_slice(&packed);
        out
    }

    fn pack_sequence_header(&self) -> Vec<u8> {
        let mut ret = PackedData::new();
        ret.write(0, 3); // profile 0
        ret.write_bool(false); // still_picture
        ret.write_bool(false); // reduced still picture
        ret.write_bool(false); // no timing info
        ret.write_bool(false); // no initial display delay
        ret.write(0, 5); // one operating point
        ret.write(0, 12); // operating_point_idc
        ret.write(self.level_idx as u64, 5);
        if self.level_idx > 7 {
            ret.write_bool(false);
        }
        ret.write(15, 4);
        ret.write(15, 4);
        ret.write((self.source_width - 1) as u64, 16);
        ret.write((self.source_height - 1) as u64, 16);
        ret.write_bool(false); // no frame ids
        ret.write_bool(false); // 128x128 sb
        ret.write_bool(false); // filter intra
        ret.write_bool(false); // intra edge filter
        ret.write_bool(false); // interintra compound
        ret.write_bool(false); // masked compound
        ret.write_bool(false); // warped motion
        ret.write_bool(false); // dual filter
        ret.write_bool(true); // order hint
        ret.write_bool(false); // jnt comp
        ret.write_bool(false); // ref frame mvs
        ret.write_bool(true); // seq choose screen content tools
        ret.write_bool(false); // seq choose integer mv
        ret.write_bool(false); // force integer mv
        ret.write(7, 3); // order_hint_bits_minus_1
        ret.write_bool(false); // superres
        ret.write_bool(true); // cdef
        ret.write_bool(false); // restoration
        ret.write_bool(false); // high bitdepth
        ret.write_bool(false); // monochrome
        ret.write_bool(false); // no color description
        ret.write_bool(false); // no color range
        ret.write(0, 2); // chroma sample position
        ret.write_bool(true); // separate_uv_delta_q
        ret.write_bool(false); // film grain
        ret.write_bool(true); // trailing bit
        ret.flush()
    }

    fn make_picture_param(&self, is_key: bool) -> VAEncPictureParameterBufferAV1 {
        let recon_idx = if is_key {
            0
        } else {
            (self.cur_ref_idx + 1) % 2
        };

        let mut pic: VAEncPictureParameterBufferAV1 = unsafe { std::mem::zeroed() };
        pic.frame_width_minus_1 = (self.source_width - 1) as u16;
        pic.frame_height_minus_1 = (self.source_height - 1) as u16;
        pic.reconstructed_frame = self.surfaces[recon_idx];
        pic.coded_buf = self.coded_buf;
        pic.reference_frames.fill(u32::MAX);
        pic.ref_frame_idx.fill(0);
        if !is_key {
            pic.reference_frames[0] = self.surfaces[self.cur_ref_idx];
            pic.ref_frame_ctrl_l0 = VARefFrameCtrlAV1 { value: 1 };
        }
        pic.hierarchical_level_plus_1 = 0;
        pic.primary_ref_frame = if is_key { 7 } else { 0 };
        pic.order_hint = (self.frame_num & 0xff) as u8;
        pic.refresh_frame_flags = if is_key { 0xFF } else { 1 };
        pic.picture_flags = 0;
        pic.picture_flags |= if is_key { 0 } else { 1 }; // frame_type
        pic.picture_flags |= 1 << 8; // reduced_tx_set
        pic.picture_flags |= 1 << 9; // enable_frame_obu
        pic.seg_id_block_size = 0;
        pic.num_tile_groups_minus1 = 0;
        pic.temporal_id = 0;
        pic.filter_level = [15, 15];
        pic.filter_level_u = 8;
        pic.filter_level_v = 8;
        pic.loop_filter_flags = 0;
        pic.superres_scale_denominator = 0;
        pic.interpolation_filter = 0;
        pic.ref_deltas.fill(0);
        pic.mode_deltas.fill(0);
        pic.base_qindex = self.base_qindex;
        // Lock min=max=base so the driver can't adjust QP — our manually-
        // built frame header hardcodes base_qindex and the decoder must
        // dequantize at exactly this value.
        pic.min_base_qindex = self.base_qindex;
        pic.max_base_qindex = self.base_qindex;
        pic.mode_control_flags = 0;
        // Disable delta_q and delta_lf so the driver doesn't insert
        // per-superblock adjustments that our frame header doesn't signal.
        pic.mode_control_flags |= 2 << 7; // tx_mode = TX_MODE_SELECT
        pic.tile_cols = 1;
        pic.tile_rows = 1;
        pic.width_in_sbs_minus_1[0] = (self.width / 64 - 1) as u16;
        pic.height_in_sbs_minus_1[0] = (self.height / 64 - 1) as u16;
        pic.context_update_tile_id = 0;
        pic.cdef_damping_minus_3 = 0;
        pic.cdef_bits = 0; // 1 CDEF strength (no per-block variation)
        pic.tile_group_obu_hdr_info = 0b00000010; // has_size_field=1
        pic
    }

    fn frame_obu(
        &self,
        pic: &VAEncPictureParameterBufferAV1,
        offsets: &mut PicParamOffsets,
    ) -> Vec<u8> {
        let hdr = self.pack_frame_header(pic, offsets);
        let mut obu = PackedData::new();
        obu.write_obu_header(6, false, true); // frame OBU
        obu.encode_leb128(hdr.len() as u32, Some(4));
        offsets.q_idx_bit_offset += obu.outstanding_bits as u32;
        offsets.segmentation_bit_offset += obu.outstanding_bits as u32;
        offsets.loop_filter_params_bit_offset += obu.outstanding_bits as u32;
        offsets.cdef_params_bit_offset += obu.outstanding_bits as u32;
        offsets.frame_hdr_obu_size_bits += obu.outstanding_bits as u32;
        let mut out = obu.flush();
        out.extend_from_slice(&hdr);
        out
    }

    fn pack_frame_header(
        &self,
        pic: &VAEncPictureParameterBufferAV1,
        offsets: &mut PicParamOffsets,
    ) -> Vec<u8> {
        let frame_type = pic.picture_flags & 0x3;
        let is_key = frame_type == 0;

        let mut ret = PackedData::new();
        ret.write_bool(false); // show_existing_frame
        ret.write(frame_type as u64, 2); // frame_type
        ret.write_bool(true); // show_frame

        if !is_key {
            ret.write_bool(false); // error_resilient_mode
        }

        ret.write(((pic.picture_flags >> 2) & 1) as u64, 1); // disable_cdf_update
        // seq_choose_screen_content_tools=1 ⇒
        //   seq_force_screen_content_tools = SELECT_SCREEN_CONTENT_TOOLS
        // so allow_screen_content_tools is signaled for every frame.
        ret.write_bool(false); // allow_screen_content_tools
        // (allow_screen_content_tools=0 ⇒ force_integer_mv not signaled)
        ret.write_bool(false); // frame_size_override_flag
        ret.write(pic.order_hint as u64, 8);

        if !is_key {
            if true
            /* !error_resilient */
            {
                ret.write(0, 3); // primary_ref_frame = 0
            }
            // refresh_frame_flags
            ret.write(pic.refresh_frame_flags as u64, 8);
            // ref_frame_idx[0..7] — all point to slot 0
            ret.write_bool(false); // frame_refs_short_signaling = false
            for _ in 0..7 {
                ret.write(0, 3); // ref_frame_idx = 0
            }
            // frame_size_with_refs(): signal found_ref=1 for the first
            // reference so the decoder reuses its dimensions.  No need
            // to write frame_size() or additional found_ref flags.
            ret.write_bool(true); // found_ref = 1
            ret.write_bool(true); // render_and_frame_size_same
            ret.write_bool(false); // allow_high_precision_mv
            ret.write_bool(false); // filter not switchable
            ret.write(0, 2); // interpolation_filter = 0
            ret.write_bool(false); // motion not switchable
        } else {
            // KEY frame: frame_size() ⇒ frame_size_override_flag
            // already written above (0 ⇒ use seq header dims).
            ret.write_bool(true); // render_and_frame_size_same
        }

        ret.write(((pic.picture_flags >> 7) & 1) as u64, 1); // disable_frame_end_update_cdf

        // tile info
        ret.write_bool(true); // uniform tile spacing
        ret.write_bool(false); // don't increment log2 tile cols
        ret.write_bool(false); // don't increment log2 tile rows
        offsets.q_idx_bit_offset = ret.outstanding_bits as u32;
        ret.write(pic.base_qindex as u64, 8);
        ret.write_bool(false); // no dc y delta q
        ret.write_bool(false); // uv delta q same
        ret.write_bool(false); // no dc u delta q
        ret.write_bool(false); // no ac u delta q
        ret.write_bool(false); // no qmatrix
        offsets.segmentation_bit_offset = ret.outstanding_bits as u32;
        ret.write_bool(false); // segmentation disabled
        ret.write_bool(false); // no delta q present
        offsets.loop_filter_params_bit_offset = ret.outstanding_bits as u32;
        ret.write(pic.filter_level[0] as u64, 6);
        ret.write(pic.filter_level[1] as u64, 6);
        ret.write(pic.filter_level_u as u64, 6);
        ret.write(pic.filter_level_v as u64, 6);
        ret.write(0, 3); // sharpness
        ret.write(0, 1); // no mode/ref delta
        offsets.cdef_params_bit_offset = ret.outstanding_bits as u32;
        ret.write(pic.cdef_damping_minus_3 as u64, 2);
        ret.write(pic.cdef_bits as u64, 2);
        // cdef_bits=0 → 1 strength entry (2^0=1)
        let num_cdef = 1usize << (pic.cdef_bits as usize);
        for i in 0..num_cdef {
            // cdef_y_strengths[i] = pri * 4 + sec (Chromium packing)
            let ys = pic.cdef_y_strengths[i] as u64;
            ret.write(ys >> 2, 4); // y_pri_strength
            ret.write(ys & 3, 2); // y_sec_strength
            let us = pic.cdef_uv_strengths[i] as u64;
            ret.write(us >> 2, 4); // uv_pri_strength
            ret.write(us & 3, 2); // uv_sec_strength
        }
        offsets.cdef_params_size_bits =
            ret.outstanding_bits as u32 - offsets.cdef_params_bit_offset;
        ret.write_bool(true); // tx mode select

        if !is_key {
            ret.write_bool(false); // reference_select = single ref
        }

        ret.write_bool(true); // reduced tx

        if !is_key {
            // global_motion: is_global[LAST..ALTREF] = false
            for _ in 0..7 {
                ret.write_bool(false);
            }
        }

        offsets.frame_hdr_obu_size_bits = ret.outstanding_bits as u32;
        ret.flush()
    }

    fn read_coded_buffer(&self) -> Option<Vec<u8>> {
        let mut map_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, self.coded_buf, &mut map_ptr) };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        let mut out = Vec::new();
        let mut seg_ptr = map_ptr as *const u8;
        loop {
            if seg_ptr.is_null() {
                break;
            }
            let size = unsafe { r32(std::slice::from_raw_parts(seg_ptr, 4), 0) as usize };
            let data_ptr = unsafe { *(seg_ptr.add(CBS_BUF_OFF) as *const *const u8) };
            if !data_ptr.is_null() && size > 0 {
                let data = unsafe { std::slice::from_raw_parts(data_ptr, size) };
                out.extend_from_slice(data);
            }
            let next = unsafe { *(seg_ptr.add(CBS_NEXT_OFF) as *const *const u8) };
            seg_ptr = next;
        }
        unsafe {
            (self.va.vaUnmapBuffer)(self.display, self.coded_buf);
        }
        if out.is_empty() { None } else { Some(out) }
    }
}

impl Drop for VaapiAv1Encoder {
    fn drop(&mut self) {
        // Drop VPP first — it shares our VA display handle.
        self.vpp.take();
        unsafe {
            (self.va.vaDestroyBuffer)(self.display, self.coded_buf);
            (self.va.vaDestroyContext)(self.display, self.context);
            (self.va.vaDestroySurfaces)(
                self.display,
                self.surfaces.as_mut_ptr(),
                AV1_NUM_SURFACES as i32,
            );
            (self.va.vaDestroyConfig)(self.display, self.config);
            (self.va.vaTerminate)(self.display);
        }
    }
}
