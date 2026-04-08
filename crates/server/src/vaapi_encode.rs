//! Direct VA-API H.264 and H.265/HEVC encoders — no ffmpeg dependency.
//!
//! Uses `dlopen("libva.so")` and `dlopen("libva-drm.so")` at runtime.
//! Implements Constrained Baseline profile H.264 and Main profile HEVC
//! encoding via the VA-API EncSliceLP (Low Power) or EncSlice entrypoint.
//!
//! The parameter buffer structs are accessed via raw byte arrays at
//! verified offsets rather than `#[repr(C)]` struct translation, since
//! the VA-API header structs contain complex bitfields and large padding
//! arrays that are fragile to replicate in Rust.

#![allow(non_upper_case_globals, clippy::identity_op, dead_code)]

use crate::gpu_libs::{
    self, VA_STATUS_SUCCESS, VABufferID, VAConfigID, VAContextID, VADisplay, VASurfaceID,
};
use std::ffi::c_void;
use std::os::fd::{AsRawFd, OwnedFd};
use std::ptr;

// ---------------------------------------------------------------------------
// VA-API constants
// ---------------------------------------------------------------------------

// Profiles
const VAProfileH264ConstrainedBaseline: i32 = 6;
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

// Surface sentinel
const VA_INVALID_SURFACE: u32 = 0xFFFF_FFFF;
const VA_PICTURE_H264_INVALID: u32 = 0x01;

// Struct sizes (from va_enc_h264.h on VA-API 1.23, verified via offsetof)
const SPS_SIZE: usize = 1132;
const PPS_SIZE: usize = 648;
const SLICE_SIZE: usize = 3140;
const VA_IMAGE_SIZE: usize = 120;

// Coded buffer segment
const CBS_SIZE_OFF: usize = 0;
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

// ---------------------------------------------------------------------------
// VPP context — BGRA DMA-BUF → NV12 VASurface on the GPU
// ---------------------------------------------------------------------------

/// VA-API Video Processing Pipeline context.
/// Shares the VADisplay with the encoder; convert_dmabuf() takes a BGRA
/// DMA-BUF fd and returns an NV12 VASurface ready to be encoded.
struct VppContext {
    va: &'static crate::gpu_libs::VaFns,
    display: VADisplay,
    config: u32,
    context: u32,
    /// Pool of NV12 output surfaces (round-robin).
    nv12_surfaces: [u32; 4],
    next_surf: usize,
    width: u32,
    height: u32,
}

impl VppContext {
    /// Try to create a VPP context on an existing VADisplay.
    /// Returns None if VAEntrypointVideoProc is unavailable.
    unsafe fn try_new(
        va: &'static crate::gpu_libs::VaFns,
        display: VADisplay,
        width: u32,
        height: u32,
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

        eprintln!("[vaapi-vpp] initialized {width}x{height} BGRA→NV12 VPP");
        Some(Self {
            va,
            display,
            config,
            context,
            nv12_surfaces,
            next_surf: 0,
            width,
            height,
        })
    }

    /// Import a BGRA/XRGB DMA-BUF, run VPP BGRA→NV12, return the NV12 surface.
    /// The returned VASurfaceID is from the internal pool; it must be consumed
    /// (encoded) before the next call.
    /// Uses VADRMPRIMESurfaceDescriptor (PRIME_2) for modifier-aware import.
    ///
    /// `src_width`/`src_height` are the actual DMA-BUF dimensions (may differ
    /// from the VPP output size).  VPP scales as needed.
    #[allow(clippy::too_many_arguments)]
    unsafe fn convert_dmabuf(
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

        // Translate DRM fourcc → VA-API fourcc.
        let va_fourcc = drm_fourcc_to_va(fourcc)?;
        // AMD's Mesa VA-API only supports BGRA/BGRX for PRIME RGB surface
        // import.  Map RGBA/RGBX → BGRA/BGRX.  The R/B channels swap in
        // the VPP BGRA→NV12 conversion, producing slightly off chroma, but
        // the import succeeds and we get zero-copy GPU encoding.
        // Also remap the DRM layer fourcc for the same reason — AMD rejects
        // ABGR8888/XBGR8888 in the PRIME_2 descriptor.
        let (surface_fourcc, layer_drm_fourcc) = match va_fourcc {
            VA_FOURCC_RGBA => (VA_FOURCC_BGRA, blit_compositor::drm_fourcc::ARGB8888),
            VA_FOURCC_RGBX => (VA_FOURCC_BGRX, blit_compositor::drm_fourcc::XRGB8888),
            _ => (va_fourcc, fourcc),
        };

        // Use PRIME_2 (VADRMPRIMESurfaceDescriptor) — supports explicit modifiers
        // and is required by Mesa radeonsi for DMA-BUF import.
        // The descriptor uses DRM fourcc in the layer; the surface uses VA fourcc.
        //
        // Use lseek to get the actual DMA-BUF size; some drivers allocate with
        // extra GPU padding so stride*height underestimates.
        let actual_size = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
        let buf_size = if actual_size > 0 {
            actual_size as u32
        } else {
            stride * import_h
        };

        // Previously, a readlink guard rejected non-DRM fds (e.g. Vulkan WSI
        // anonymous /dmabuf).  Removed — let vaCreateSurfaces attempt PRIME_2
        // import for any fd.  If the driver can't import it, it returns an
        // error and the caller falls back to CPU readback.

        // VA-API PRIME import only works with DRM GEM-backed fds (from a
        // GPU render node).  Anonymous DMA-BUF heap fds ("/dmabuf:") are
        // CPU-accessible but not importable by VA-API — skip early so the
        // caller falls through to the CPU mmap fallback without the
        // overhead of a failed vaCreateSurfaces call.
        {
            let mut link_buf = [0u8; 256];
            let path = format!("/proc/self/fd/{fd}\0");
            let n = unsafe {
                libc::readlink(
                    path.as_ptr() as *const _,
                    link_buf.as_mut_ptr() as *mut _,
                    255,
                )
            };
            if n > 0 {
                let link = &link_buf[..n as usize];
                if !link.starts_with(b"/dev/dri/") {
                    return None;
                }
            }
        }

        let mut desc = VADRMPRIMESurfaceDescriptor {
            fourcc: surface_fourcc,
            width: import_w,
            height: import_h,
            num_objects: 1,
            objects: [
                DRMObject {
                    fd,
                    size: buf_size,
                    // Pass the modifier through as-is.  Linear (0x0) is the
                    // correct modifier for Vulkan WSI buffers on AMD.
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
                    type_: 0, // VAGenericValueTypeInteger
                    value: VAGenericValueInner {
                        i: VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 as i32,
                    },
                },
            },
            VASurfaceAttrib {
                type_: VA_SURFACE_ATTRIB_EXTERNAL_BUFFERS,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VAGenericValue {
                    type_: 2, // VAGenericValueTypePointer
                    value: VAGenericValueInner {
                        p: &mut desc as *mut _ as *mut c_void,
                    },
                },
            },
        ];
        let mut bgra_surf = 0u32;
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
            // Fallback: PRIME_1 (legacy) import — doesn't carry modifier info
            // but works with more fd types on some drivers.
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

        let nv12_surf = self.nv12_surfaces[self.next_surf];
        self.next_surf = (self.next_surf + 1) % self.nv12_surfaces.len();

        // Build VPP pipeline param buffer: bgra_surf → nv12_surf.
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
                // params is on the stack; VA-API copies on vaCreateBuffer
                &params as *const _ as *mut c_void,
                &mut buf_id,
            )
        };
        if st != crate::gpu_libs::VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroySurfaces)(self.display, &mut bgra_surf, 1);
            }
            return None;
        }

        // Submit VPP.
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
            (va.vaDestroySurfaces)(self.display, &mut bgra_surf, 1);
        }

        if ok { Some(nv12_surf) } else { None }
    }
}

impl Drop for VppContext {
    fn drop(&mut self) {
        unsafe {
            let va = self.va;
            (va.vaDestroyContext)(self.display, self.context);
            (va.vaDestroySurfaces)(self.display, self.nv12_surfaces.as_mut_ptr(), 4);
            (va.vaDestroyConfig)(self.display, self.config);
        }
    }
}

// ---------------------------------------------------------------------------
// Standalone DMA-BUF → RGBA readback via VPP (used by capture path)
// ---------------------------------------------------------------------------

/// Import a DMA-BUF fd via VA-API, then read back the pixels as RGBA.
/// Opens a temporary VA display, imports the buffer, and uses vaDeriveImage
/// + vaMapBuffer for CPU readback.  Returns None on any failure.
#[allow(clippy::too_many_arguments)]
pub fn vpp_readback_dmabuf(
    vaapi_device: &str,
    fd: std::os::fd::RawFd,
    fourcc: u32,
    modifier: u64,
    stride: u32,
    offset: u32,
    width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    let va = crate::gpu_libs::va()?;
    let va_drm = crate::gpu_libs::va_drm()?;
    let va_fourcc = drm_fourcc_to_va(fourcc)?;

    // Open render node + init display
    let drm_fd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(vaapi_device)
        .ok()?;
    use std::os::fd::AsRawFd;
    let display = unsafe { (va_drm.vaGetDisplayDRM)(drm_fd.as_raw_fd()) };
    if display.is_null() {
        return None;
    }
    let mut major = 0i32;
    let mut minor = 0i32;
    let st = unsafe { (va.vaInitialize)(display, &mut major, &mut minor) };
    if st != crate::gpu_libs::VA_STATUS_SUCCESS {
        return None;
    }

    // Build PRIME_2 descriptor
    let actual_size = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
    let buf_size = if actual_size > 0 {
        actual_size as u32
    } else {
        stride * height
    };
    let mut desc = VADRMPRIMESurfaceDescriptor {
        fourcc: va_fourcc,
        width,
        height,
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
                drm_format: fourcc,
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
    let mut surf = 0u32;
    let st = unsafe {
        (va.vaCreateSurfaces)(
            display,
            VA_RT_FORMAT_RGB32,
            width,
            height,
            &mut surf,
            1,
            attribs.as_ptr() as *mut c_void,
            2,
        )
    };
    if st != crate::gpu_libs::VA_STATUS_SUCCESS {
        unsafe { (va.vaTerminate)(display) };
        return None;
    }

    // Sync + derive image + map buffer
    unsafe { (va.vaSyncSurface)(display, surf) };
    let mut image = [0u8; VA_IMAGE_SIZE];
    let st = unsafe { (va.vaDeriveImage)(display, surf, image.as_mut_ptr() as *mut c_void) };
    if st != crate::gpu_libs::VA_STATUS_SUCCESS {
        unsafe {
            (va.vaDestroySurfaces)(display, &mut surf, 1);
            (va.vaTerminate)(display);
        }
        return None;
    }
    let image_buf = u32::from_ne_bytes(image[VAIMG_BUF_OFF..VAIMG_BUF_OFF + 4].try_into().unwrap());
    let pitch = u32::from_ne_bytes(
        image[VAIMG_PITCHES_OFF..VAIMG_PITCHES_OFF + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let img_offset = u32::from_ne_bytes(
        image[VAIMG_OFFSETS_OFF..VAIMG_OFFSETS_OFF + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let image_id = u32::from_ne_bytes(image[VAIMG_ID_OFF..VAIMG_ID_OFF + 4].try_into().unwrap());

    let mut map_ptr: *mut c_void = std::ptr::null_mut();
    let st = unsafe { (va.vaMapBuffer)(display, image_buf, &mut map_ptr) };
    if st != crate::gpu_libs::VA_STATUS_SUCCESS || map_ptr.is_null() {
        unsafe {
            (va.vaDestroyImage)(display, image_id);
            (va.vaDestroySurfaces)(display, &mut surf, 1);
            (va.vaTerminate)(display);
        }
        return None;
    }

    // Read pixels — VA-API BGRA → RGBA
    let w = width as usize;
    let h = height as usize;
    let row_bytes = w * 4;
    let slice = unsafe { std::slice::from_raw_parts(map_ptr as *const u8, pitch * h + img_offset) };
    let is_bgr = matches!(va_fourcc, VA_FOURCC_BGRA | VA_FOURCC_BGRX);
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let src = &slice[img_offset + row * pitch..img_offset + row * pitch + row_bytes];
        if is_bgr {
            for px in src.chunks_exact(4) {
                rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
        } else {
            for px in src.chunks_exact(4) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
    }

    unsafe {
        (va.vaUnmapBuffer)(display, image_buf);
        (va.vaDestroyImage)(display, image_id);
        (va.vaDestroySurfaces)(display, &mut surf, 1);
        (va.vaTerminate)(display);
    }

    Some(rgba)
}

/// Like `vpp_readback_dmabuf` but returns BGRA instead of RGBA.
#[allow(clippy::too_many_arguments)]
pub fn vpp_readback_dmabuf_as_bgra(
    vaapi_device: &str,
    fd: std::os::fd::RawFd,
    fourcc: u32,
    modifier: u64,
    stride: u32,
    offset: u32,
    width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    let mut rgba = vpp_readback_dmabuf(
        vaapi_device,
        fd,
        fourcc,
        modifier,
        stride,
        offset,
        width,
        height,
    )?;
    // RGBA → BGRA
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Some(rgba)
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
    // profile_idc
    w.write_bits(66, 8);
    // constraint_set0_flag=1, constraint_set1_flag=1, others=0, reserved=0
    w.write_bits(0b11000000, 8);
    // level_idc
    w.write_bits(level_idc as u32, 8);
    // seq_parameter_set_id
    w.write_ue(0);
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

/// Build an Annex B PPS NAL for Constrained Baseline H.264.
fn build_h264_pps_nal() -> Vec<u8> {
    let mut w = BitstreamWriter::new();
    // pic_parameter_set_id
    w.write_ue(0);
    // seq_parameter_set_id
    w.write_ue(0);
    // entropy_coding_mode_flag (0 = CAVLC)
    w.write_bit(0);
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
    _drm_fd: OwnedFd,
    /// Optional VA-API VPP context for zero-copy DMA-BUF import.
    /// Present when VAEntrypointVideoProc is supported by the driver.
    vpp: Option<VppContext>,
}

unsafe impl Send for VaapiDirectEncoder {}

impl VaapiDirectEncoder {
    pub fn try_new(width: u32, height: u32, vaapi_device: &str) -> Result<Self, String> {
        let va = gpu_libs::va().ok_or("libva.so not found")?;
        let va_drm = gpu_libs::va_drm().ok_or("libva-drm.so not found")?;

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
                VAProfileH264ConstrainedBaseline,
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
                VAProfileH264ConstrainedBaseline,
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

        eprintln!(
            "[vaapi-direct] initialized H.264 CB encoder for {width}x{height} (ep={entrypoint})"
        );

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
            _drm_fd: drm_fd,
            // Try to init VPP on the same display for zero-copy DMA-BUF import.
            vpp: unsafe { VppContext::try_new(va, display, width, height) },
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

        let mut buffers = [sps_buf, pps_buf, slice_buf];

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
            if let Some(pos) = find_annex_b_start(&nal_data) {
                let hdr_pos = pos + if nal_data[pos + 2] == 1 { 3 } else { 4 };
                if hdr_pos < nal_data.len() {
                    nal_data[hdr_pos] = if is_idr {
                        0x65 // nal_ref_idc=3, nal_unit_type=5 (IDR)
                    } else {
                        0x41 // nal_ref_idc=2, nal_unit_type=1 (non-IDR)
                    };
                }
            }

            if is_idr {
                // Prepend SPS + PPS NALs so the browser decoder can initialize.
                // AMD VA-API doesn't include these in the coded buffer.
                let mut out = build_h264_sps_nal(
                    self.width_in_mbs,
                    self.height_in_mbs,
                    self.width,
                    self.height,
                );
                out.extend_from_slice(&build_h264_pps_nal());
                out.extend_from_slice(&nal_data);
                Some((out, true))
            } else {
                Some((nal_data, false))
            }
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
        //   deblocking_filter_control_present_flag: bit 9 = 1
        let mut pic_fields: u32 = 0;
        if is_idr {
            pic_fields |= 1; // idr_pic_flag
        }
        pic_fields |= 1 << 1; // reference_pic_flag = 1
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
        slice[3119] = (23i8 - 26) as u8; // QP=23, pic_init_qp=26, delta = -3

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

// ===========================================================================
// VA-API HEVC (H.265) Main profile encoder
// ===========================================================================

// Profiles
const VAProfileHEVCMain: i32 = 17;

// Struct sizes (from va_enc_hevc.h on libva 2.23, verified via offsetof)
const HEVC_SPS_SIZE: usize = 116;
const HEVC_PPS_SIZE: usize = 576;
const HEVC_SLICE_SIZE: usize = 1076;
const HEVC_PIC_SIZE: usize = 28; // sizeof(VAPictureHEVC)

// VAPictureHEVC offsets
const HEVC_PIC_ID: usize = 0; // VASurfaceID (u32)
const HEVC_PIC_POC: usize = 4; // pic_order_cnt (i32)
const HEVC_PIC_FLAGS: usize = 8; // flags (u32)

// VAPictureHEVC flags
const VA_PICTURE_HEVC_INVALID: u32 = 0x01;
const VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE: u32 = 0x10;

pub struct VaapiHevcEncoder {
    va: &'static gpu_libs::VaFns,
    display: VADisplay,
    config: VAConfigID,
    context: VAContextID,
    surfaces: [VASurfaceID; TOTAL_SURFACES],
    coded_buf: VABufferID,
    width: u32,
    height: u32,
    ctu_size: u32,
    width_in_ctus: u32,
    height_in_ctus: u32,
    frame_num: u32,
    idr_num: u32,
    force_idr: bool,
    cur_ref_idx: usize,
    log2_min_cb_minus3: u8,
    log2_diff_max_min_cb: u8,
    _drm_fd: OwnedFd,
    vpp: Option<VppContext>,
}

unsafe impl Send for VaapiHevcEncoder {}

impl VaapiHevcEncoder {
    pub fn try_new(width: u32, height: u32, vaapi_device: &str) -> Result<Self, String> {
        let va = gpu_libs::va().ok_or("libva.so not found")?;
        let va_drm = gpu_libs::va_drm().ok_or("libva-drm.so not found")?;

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

        // Probe for EncSliceLP or EncSlice on HEVCMain
        let mut entrypoints = [0i32; 16];
        let mut num_ep = 0i32;
        unsafe {
            (va.vaQueryConfigEntrypoints)(
                display,
                VAProfileHEVCMain,
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
            return Err("HEVC encode not supported on this VA-API device".into());
        };

        // Create config
        let mut config: VAConfigID = 0;
        let st = unsafe {
            (va.vaCreateConfig)(
                display,
                VAProfileHEVCMain,
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
            return Err(format!("vaCreateConfig(HEVC) failed: {st}"));
        }

        // Create surfaces
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
            return Err(format!("vaCreateSurfaces(HEVC) failed: {st}"));
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
            return Err(format!("vaCreateContext(HEVC) failed: {st}"));
        }

        // Coded buffer
        let coded_buf_size = width * height;
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
            return Err(format!("vaCreateBuffer(coded,HEVC) failed: {st}"));
        }

        // HEVC uses CTUs.  Most HW supports 32 or 64.  We use 32 (log2=5)
        // which is the most widely supported (Intel, AMD).
        //   log2_min_luma_coding_block_size_minus3 = 0 → min CB = 8
        //   log2_diff_max_min_luma_coding_block_size = 2 → max CB = 32 = CTU
        let ctu_size = 32u32;
        let log2_min_cb_minus3: u8 = 0; // min CB log2 = 3 → 8
        let log2_diff_max_min_cb: u8 = 2; // max CB log2 = 5 → 32

        let width_in_ctus = width.div_ceil(ctu_size);
        let height_in_ctus = height.div_ceil(ctu_size);

        eprintln!(
            "[vaapi-direct] initialized HEVC Main encoder for {width}x{height} (ep={entrypoint}, ctu={ctu_size})"
        );

        Ok(Self {
            va,
            display,
            config,
            context,
            surfaces,
            coded_buf,
            width,
            height,
            ctu_size,
            width_in_ctus,
            height_in_ctus,
            frame_num: 0,
            idr_num: 0,
            force_idr: false,
            cur_ref_idx: 0,
            log2_min_cb_minus3,
            log2_diff_max_min_cb,
            _drm_fd: drm_fd,
            vpp: unsafe { VppContext::try_new(va, display, width, height) },
        })
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

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

    pub fn encode_nv12(
        &mut self,
        y_data: &[u8],
        uv_data: &[u8],
        y_stride: usize,
        uv_stride: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let input_surface = self.surfaces[NUM_REF_SURFACES];
        self.upload_nv12(input_surface, y_data, uv_data, y_stride, uv_stride)?;
        self.encode_surface(input_surface)
    }

    pub fn encode_bgra_padded(
        &mut self,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let input_surface = self.surfaces[NUM_REF_SURFACES];
        self.upload_bgra(input_surface, bgra, src_w, src_h)?;
        self.encode_surface(input_surface)
    }

    // --- Surface upload (reuse identical logic from H.264 encoder) ---

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
        }

        unsafe {
            (self.va.vaUnmapBuffer)(self.display, buf_id);
            (self.va.vaDestroyImage)(self.display, image_id);
        }
        Some(())
    }

    // --- Encode pipeline ---

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

        let sps_buf = self.create_hevc_sps()?;
        let pps_buf = self.create_hevc_pps(is_idr, ref_surface, recon_surface)?;
        let slice_buf = self.create_hevc_slice(is_idr, ref_surface)?;

        let mut buffers = [sps_buf, pps_buf, slice_buf];

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

        let st = unsafe { (self.va.vaSyncSurface)(self.display, input_surface) };
        if st != VA_STATUS_SUCCESS {
            self.destroy_buffers(&buffers);
            return None;
        }

        let nal_data = self.read_coded_buffer()?;
        self.destroy_buffers(&buffers);

        self.frame_num += 1;
        self.cur_ref_idx = recon_idx;

        if nal_data.is_empty() {
            None
        } else {
            Some((nal_data, is_idr))
        }
    }

    // --- VAPictureHEVC helpers ---

    fn write_hevc_pic(buf: &mut [u8], off: usize, surface: VASurfaceID, poc: i32, flags: u32) {
        w32(buf, off + HEVC_PIC_ID, surface);
        buf[off + HEVC_PIC_POC..off + HEVC_PIC_POC + 4].copy_from_slice(&poc.to_ne_bytes());
        w32(buf, off + HEVC_PIC_FLAGS, flags);
        // va_reserved[4] stays zeroed
    }

    fn write_hevc_pic_invalid(buf: &mut [u8], off: usize) {
        Self::write_hevc_pic(buf, off, VA_INVALID_SURFACE, 0, VA_PICTURE_HEVC_INVALID);
    }

    // --- Parameter buffers ---

    fn create_hevc_sps(&self) -> Option<VABufferID> {
        let mut sps = [0u8; HEVC_SPS_SIZE];

        // general_profile_idc = 1 (Main)                          @ 0
        w8(&mut sps, 0, 1);
        // general_level_idc = 120 (level 4.0, supports 2048×1080) @ 1
        w8(&mut sps, 1, 120);
        // general_tier_flag = 0 (Main tier)                        @ 2
        // intra_period                                             @ 4
        w32(&mut sps, 4, 120);
        // intra_idr_period                                         @ 8
        w32(&mut sps, 8, 120);
        // ip_period (1 = no B-frames, IP only)                     @ 12
        w32(&mut sps, 12, 1);
        // bits_per_second = 0 (VBR / CQP)                         @ 16
        // pic_width_in_luma_samples                                @ 20
        w16(&mut sps, 20, self.width as u16);
        // pic_height_in_luma_samples                               @ 22
        w16(&mut sps, 22, self.height as u16);

        // seq_fields bitfield                                      @ 24
        //   chroma_format_idc      : bits 0-1  = 1 (4:2:0)
        //   amp_enabled_flag       : bit 11    = 1
        //   sps_temporal_mvp_enabled_flag : bit 15 = 1
        //   low_delay_seq          : bit 16    = 1 (IP only)
        let seq_fields: u32 = 1 // chroma_format_idc = 1
            | (1 << 11)         // amp_enabled_flag
            | (1 << 15)         // sps_temporal_mvp_enabled_flag
            | (1 << 16); // low_delay_seq
        w32(&mut sps, 24, seq_fields);

        // log2_min_luma_coding_block_size_minus3                   @ 28
        w8(&mut sps, 28, self.log2_min_cb_minus3);
        // log2_diff_max_min_luma_coding_block_size                 @ 29
        w8(&mut sps, 29, self.log2_diff_max_min_cb);
        // log2_min_transform_block_size_minus2 = 0 → min TB = 4   @ 30
        w8(&mut sps, 30, 0);
        // log2_diff_max_min_transform_block_size = 3 → max TB = 32 @ 31
        w8(&mut sps, 31, 3);
        // max_transform_hierarchy_depth_inter = 2                  @ 32
        w8(&mut sps, 32, 2);
        // max_transform_hierarchy_depth_intra = 2                  @ 33
        w8(&mut sps, 33, 2);

        let mut buf_id: VABufferID = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncSequenceParameterBufferType,
                HEVC_SPS_SIZE as u32,
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

    fn create_hevc_pps(
        &self,
        is_idr: bool,
        ref_surface: VASurfaceID,
        recon_surface: VASurfaceID,
    ) -> Option<VABufferID> {
        let mut pps = [0u8; HEVC_PPS_SIZE];
        let poc = self.frame_num as i32 * 2;

        // decoded_curr_pic (VAPictureHEVC)                         @ 0
        Self::write_hevc_pic(&mut pps, 0, recon_surface, poc, 0);

        // reference_frames[15] (VAPictureHEVC × 15)                @ 28
        for i in 0..15u32 {
            let off = 28 + (i as usize) * HEVC_PIC_SIZE;
            Self::write_hevc_pic_invalid(&mut pps, off);
        }
        // Set reference_frames[0] for P-frames
        if !is_idr && self.frame_num > 0 {
            let ref_poc = (self.frame_num as i32 - 1) * 2;
            Self::write_hevc_pic(
                &mut pps,
                28,
                ref_surface,
                ref_poc,
                VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
            );
        }

        // coded_buf                                                @ 448
        w32(&mut pps, 448, self.coded_buf);
        // collocated_ref_pic_index                                 @ 452
        w8(&mut pps, 452, if is_idr { 0xFF } else { 0 });
        // pic_init_qp                                              @ 454
        w8(&mut pps, 454, 26);
        // num_ref_idx_l0_default_active_minus1                     @ 502
        w8(&mut pps, 502, 0);
        // nal_unit_type: IDR=19 (IDR_W_RADL), P=1 (TRAIL_R)      @ 505
        w8(&mut pps, 505, if is_idr { 19 } else { 1 });

        // pic_fields bitfield                                      @ 508
        //   idr_pic_flag          : bit 0
        //   coding_type           : bits 1-3 (1=I, 2=P)
        //   reference_pic_flag    : bit 4    = 1
        //   cu_qp_delta_enabled_flag : bit 10 = 1
        //   pps_loop_filter_across_slices_enabled_flag : bit 15 = 1
        let coding_type: u32 = if is_idr { 1 } else { 2 };
        let mut pic_fields: u32 = 0;
        if is_idr {
            pic_fields |= 1; // idr_pic_flag
        }
        pic_fields |= coding_type << 1; // coding_type
        pic_fields |= 1 << 4; // reference_pic_flag
        pic_fields |= 1 << 10; // cu_qp_delta_enabled_flag
        pic_fields |= 1 << 15; // pps_loop_filter_across_slices_enabled_flag
        w32(&mut pps, 508, pic_fields);

        let mut buf_id: VABufferID = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncPictureParameterBufferType,
                HEVC_PPS_SIZE as u32,
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

    fn create_hevc_slice(&self, is_idr: bool, ref_surface: VASurfaceID) -> Option<VABufferID> {
        let mut slice = [0u8; HEVC_SLICE_SIZE];

        let num_ctus = self.width_in_ctus * self.height_in_ctus;

        // slice_segment_address = 0                                @ 0
        // num_ctu_in_slice                                         @ 4
        w32(&mut slice, 4, num_ctus);
        // slice_type: 2 = I, 0 = B, 1 = P                         @ 8
        w8(&mut slice, 8, if is_idr { 2 } else { 1 });
        // num_ref_idx_l0_active_minus1                             @ 10
        w8(&mut slice, 10, 0);

        // ref_pic_list0[15] (VAPictureHEVC × 15)                  @ 12
        for i in 0..15u32 {
            let off = 12 + (i as usize) * HEVC_PIC_SIZE;
            Self::write_hevc_pic_invalid(&mut slice, off);
        }
        // ref_pic_list1[15]                                        @ 432
        for i in 0..15u32 {
            let off = 432 + (i as usize) * HEVC_PIC_SIZE;
            Self::write_hevc_pic_invalid(&mut slice, off);
        }

        // Set ref_pic_list0[0] for P-frames
        if !is_idr && self.frame_num > 0 {
            let ref_poc = (self.frame_num as i32 - 1) * 2;
            Self::write_hevc_pic(
                &mut slice,
                12,
                ref_surface,
                ref_poc,
                VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
            );
        }

        // max_num_merge_cand = 5                                   @ 1034
        w8(&mut slice, 1034, 5);
        // slice_qp_delta (i8): QP=23, pic_init_qp=26 → delta=-3   @ 1035
        slice[1035] = (-3i8) as u8;

        // slice_fields bitfield                                    @ 1040
        //   last_slice_of_pic_flag              : bit 0  = 1
        //   slice_temporal_mvp_enabled_flag     : bit 4  = 1
        //   num_ref_idx_active_override_flag    : bit 7  = 1 (for P)
        //   slice_loop_filter_across_slices_enabled_flag : bit 12 = 1
        //   collocated_from_l0_flag             : bit 13 = 1
        let mut slice_fields: u32 = 0;
        slice_fields |= 1; // last_slice_of_pic_flag
        slice_fields |= 1 << 4; // slice_temporal_mvp_enabled_flag
        if !is_idr {
            slice_fields |= 1 << 7; // num_ref_idx_active_override_flag
        }
        slice_fields |= 1 << 12; // slice_loop_filter_across_slices_enabled_flag
        slice_fields |= 1 << 13; // collocated_from_l0_flag
        w32(&mut slice, 1040, slice_fields);

        let mut buf_id: VABufferID = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncSliceParameterBufferType,
                HEVC_SLICE_SIZE as u32,
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

impl Drop for VaapiHevcEncoder {
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
