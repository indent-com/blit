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
#[allow(dead_code)]
const VA_RT_FORMAT_YUV420_10: u32 = 0x00000100;
#[allow(dead_code)]
const VA_RT_FORMAT_YUV444: u32 = 0x00000004;

// Buffer types
const VAEncCodedBufferType: i32 = 21;
const VAEncSequenceParameterBufferType: i32 = 22;
const VAEncPictureParameterBufferType: i32 = 23;
const VAEncSliceParameterBufferType: i32 = 24;
const VAEncPackedHeaderParameterBufferType: i32 = 25;
const VAEncPackedHeaderDataBufferType: i32 = 26;
const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 1;
const VA_ENC_PACKED_HEADER_PICTURE: u32 = 2;
const VAEncMiscParameterBufferType: i32 = 27;

// Misc parameter sub-types
const VAEncMiscParameterTypeFrameRate: u32 = 0;
const VAEncMiscParameterTypeQualityLevel: u32 = 6;

// Realtime defaults — hardcoded here because the encoder isn't plumbed with
// the client's actual pacing rate.  The frame-rate hint helps the driver
// pace internal stages; the actual submission rate doesn't have to match.
const REALTIME_FPS: u32 = 60;
/// AMD radeonsi maps quality_level to AMF preset: 0=balanced, 1-2=quality,
/// 3-7=speed.  7 is the fastest preset.
const REALTIME_QUALITY_LEVEL: u32 = 7;

#[repr(C)]
struct VAEncMiscParameterFrameRate {
    framerate: u32,
    framerate_flags: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct VAEncMiscParameterBufferQualityLevel {
    quality_level: u32,
    va_reserved: [u32; 4],
}

/// Allocate a `VAEncMiscParameterBufferType` containing `inner`.  The driver
/// expects a 4-byte type tag followed by the inner struct in a single buffer.
fn create_misc_param_buffer<T>(
    va: &gpu_libs::VaFns,
    display: VADisplay,
    context: VAContextID,
    sub_type: u32,
    inner: T,
) -> Option<VABufferID> {
    #[repr(C)]
    struct Wrapper<T> {
        type_: u32,
        inner: T,
    }
    let wrapper = Wrapper {
        type_: sub_type,
        inner,
    };
    let mut buf_id: VABufferID = 0;
    let st = unsafe {
        (va.vaCreateBuffer)(
            display,
            context,
            VAEncMiscParameterBufferType,
            std::mem::size_of::<Wrapper<T>>() as u32,
            1,
            &wrapper as *const _ as *mut c_void,
            &mut buf_id,
        )
    };
    if st == VA_STATUS_SUCCESS {
        Some(buf_id)
    } else {
        None
    }
}

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

/// Cached result of `vaDeriveImage` on the encoder's input surface.
/// The surface is fixed for the encoder's lifetime, so the derived image
/// metadata (pitches, offsets, buffer ID) never changes.  Caching avoids
/// two VA-API driver round-trips per frame (vaDeriveImage + vaDestroyImage).
struct CachedDerivedImage {
    image_id: u32,
    buf_id: u32,
    y_pitch: usize,
    uv_pitch: usize,
    y_offset: usize,
    uv_offset: usize,
}

// VA-API fourcc values differ from DRM fourcc values.
// DRM uses little-endian channel order in the name; VA-API uses memory order.
// DRM AR24 (ARGB8888) = [B,G,R,A] in memory = VA_FOURCC_BGRA.
/// VADRMPRIMESurfaceDescriptor — used for NV12 surface export.
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

// ---------------------------------------------------------------------------
// VPP context — allocates NV12 surfaces for Vulkan compute, GBM BGRA buffers
// ---------------------------------------------------------------------------

/// Number of NV12 output surfaces in the VPP round-robin pool.  Must be
/// large enough so that a surface handed to the encoder is not overwritten
/// by a later VPP conversion before the encode finishes.  6 gives ample
/// headroom for pipelined VPP→encode with the vaSyncSurface elided.
const NUM_NV12_SURFACES: usize = 6;

/// Number of BGRA input surfaces exported to the compositor for the
/// zero-copy path.  The compositor renders into these via Vulkan while
/// VPP reads a previously rendered surface.  5 allows two frames of
/// pipeline depth (1 encoding + 1 rendered + 1 rendering + 2 free).
const NUM_BGRA_SURFACES: usize = 5;

/// GBM-allocated LINEAR buffer for zero-copy compositor→encoder sharing.
pub(crate) struct GbmExportedBuffer {
    pub fd: std::os::fd::OwnedFd,
    pub stride: u32,
    pub width: u32,
    pub height: u32,
}

/// NV12 buffer for the Vulkan compute shader → VA-API encoder zero-copy path.
/// VA-API allocates the surface, exports a DMA-BUF fd for Vulkan to write into.
pub(crate) struct GbmNv12Buffer {
    pub fd: std::sync::Arc<std::os::fd::OwnedFd>,
    pub stride: u32,
    pub uv_offset: u32,
    /// DRM format modifier (0 = linear, nonzero = tiled).
    pub modifier: u64,
    /// VA surface — encoder reads directly, no PRIME import needed.
    pub va_surface: VASurfaceID,
}

pub(crate) struct VppContext {
    va: &'static crate::gpu_libs::VaFns,
    display: VADisplay,
    config: u32,
    context: u32,
    /// Pool of NV12 output surfaces for VPP.
    nv12_surfaces: [u32; NUM_NV12_SURFACES],
    /// Encoder-padded NV12 output dimensions.
    enc_width: u32,
    enc_height: u32,
    #[allow(dead_code)]
    bgra_width: u32,
    #[allow(dead_code)]
    bgra_height: u32,
    /// GBM-allocated LINEAR BGRA buffers for zero-copy path.
    pub(crate) gbm_buffers: Vec<GbmExportedBuffer>,
    /// GBM-allocated NV12 buffers — compute shader writes, encoder reads.
    pub(crate) gbm_nv12_buffers: Vec<GbmNv12Buffer>,
    #[allow(dead_code)]
    verbose: bool,
}

impl VppContext {
    /// Try to create a VPP context on an existing VADisplay.
    /// Returns None if VAEntrypointVideoProc is unavailable.
    ///
    /// `width`/`height` are the NV12 output dimensions (encoder-aligned).
    /// `bgra_width`/`bgra_height` are the BGRA input surface dimensions
    /// (source resolution).  The compositor matches against these when
    /// deciding whether to use the zero-copy external output path, so they
    /// must equal the compositor's physical output size.
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn try_new(
        va: &'static crate::gpu_libs::VaFns,
        display: VADisplay,
        width: u32,
        height: u32,
        bgra_width: u32,
        bgra_height: u32,
        drm_fd: std::os::fd::RawFd,
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
        let mut nv12_surfaces = [0u32; NUM_NV12_SURFACES];
        let st = unsafe {
            (va.vaCreateSurfaces)(
                display,
                VA_RT_FORMAT_YUV420,
                width,
                height,
                nv12_surfaces.as_mut_ptr(),
                NUM_NV12_SURFACES as u32,
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
                NUM_NV12_SURFACES as i32,
                &mut context,
            )
        };
        if st != crate::gpu_libs::VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroySurfaces)(
                    display,
                    nv12_surfaces.as_mut_ptr(),
                    NUM_NV12_SURFACES as i32,
                );
                (va.vaDestroyConfig)(display, config);
            }
            return None;
        }

        // Allocate LINEAR BGRA buffers via GBM.  The compositor imports
        // these into Vulkan and renders directly into them.  The encoder
        // Allocate LINEAR BGRA buffers via GBM for zero-copy sharing.
        // The compositor renders into these via Vulkan; the compute shader
        // converts BGRA→NV12 into VA-API-owned surfaces.
        let mut gbm_buffers = Vec::new();
        if let Ok(gbm) = crate::gpu_libs::gbm() {
            let gbm_fd = unsafe { libc::dup(drm_fd) };
            if gbm_fd >= 0 {
                let dev = unsafe { (gbm.gbm_create_device)(gbm_fd) };
                if !dev.is_null() {
                    for i in 0..NUM_BGRA_SURFACES {
                        let bo = unsafe {
                            (gbm.gbm_bo_create)(
                                dev,
                                bgra_width,
                                bgra_height,
                                crate::gpu_libs::GBM_FORMAT_ARGB8888,
                                crate::gpu_libs::GBM_BO_USE_RENDERING
                                    | crate::gpu_libs::GBM_BO_USE_LINEAR,
                            )
                        };
                        if bo.is_null() {
                            if verbose {
                                eprintln!("[vaapi-vpp] gbm_bo_create failed for buffer {i}");
                            }
                            break;
                        }
                        let fd = unsafe { (gbm.gbm_bo_get_fd)(bo) };
                        let stride = unsafe { (gbm.gbm_bo_get_stride)(bo) };
                        if fd < 0 {
                            break;
                        }
                        if verbose && i == 0 {
                            eprintln!(
                                "[vaapi-vpp] GBM buffer: {bgra_width}x{bgra_height} stride={stride} LINEAR",
                            );
                        }
                        gbm_buffers.push(GbmExportedBuffer {
                            fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) },
                            stride,
                            width: bgra_width,
                            height: bgra_height,
                        });
                        // GBM BO intentionally leaked — must outlive the fd.
                    }
                    // GBM device intentionally leaked — must outlive BOs.
                }
            }
        }

        if verbose {
            eprintln!(
                "[vaapi-vpp] initialized {bgra_width}x{bgra_height} → {width}x{height} VPP ({} GBM buffers)",
                gbm_buffers.len()
            );
        }
        Some(Self {
            va,
            display,
            config,
            context,
            nv12_surfaces,
            enc_width: width,
            enc_height: height,
            bgra_width,
            bgra_height,
            gbm_buffers,
            gbm_nv12_buffers: Vec::new(),
            verbose,
        })
    }

    /// Allocate NV12 surfaces in VA-API (driver picks optimal layout),
    /// export as DMA-BUFs for the Vulkan compute shader to write into.
    /// The encoder reads the VA surface directly — no PRIME import needed.
    /// If the driver uses a tiled layout (modifier ≠ 0), the compute shader
    /// VA-API allocates NV12 surfaces (linear or tiled), exports as DMA-BUFs.
    /// The compositor imports them into Vulkan — as buffers (linear) or
    /// images (tiled, using VK_EXT_image_drm_format_modifier).
    pub(crate) fn allocate_nv12_buffers(&mut self, _drm_fd: std::os::fd::RawFd, count: usize) {
        // Allocate at encoder-padded dimensions so the full surface is valid.
        let w = self.enc_width;
        let h = self.enc_height;
        if !self.try_vaapi_nv12_export(w, h, count) {
            eprintln!(
                "[vaapi-vpp] NV12 export unavailable {w}x{h} — \
                 falling back to CPU BGRA→NV12",
            );
        }
    }

    /// Destroy VA surfaces that were allocated for compute but not yet
    /// pushed into `gbm_nv12_buffers` (cleanup on early error).
    fn destroy_nv12_compute_surfaces(&self, surfaces: &mut [u32]) {
        // Only destroy surfaces not already tracked in gbm_nv12_buffers.
        let tracked: std::collections::HashSet<u32> =
            self.gbm_nv12_buffers.iter().map(|b| b.va_surface).collect();
        let to_destroy: Vec<u32> = surfaces
            .iter()
            .copied()
            .filter(|s| *s != 0 && !tracked.contains(s))
            .collect();
        if !to_destroy.is_empty() {
            let mut buf = to_destroy;
            unsafe {
                (self.va.vaDestroySurfaces)(self.display, buf.as_mut_ptr(), buf.len() as i32);
            }
        }
    }

    /// Try VA-API allocate → export. Returns true on success.
    fn try_vaapi_nv12_export(&mut self, w: u32, h: u32, count: usize) -> bool {
        let va = self.va;
        let mut surfaces = vec![0u32; count];
        let st = unsafe {
            (va.vaCreateSurfaces)(
                self.display,
                VA_RT_FORMAT_YUV420,
                w,
                h,
                surfaces.as_mut_ptr(),
                count as u32,
                ptr::null_mut(),
                0,
            )
        };
        if st != VA_STATUS_SUCCESS {
            eprintln!("[vaapi-vpp] NV12 vaCreateSurfaces failed: st={st} {w}x{h}");
            return false;
        }

        for (i, &surf) in surfaces.iter().enumerate() {
            let mut desc: VADRMPRIMESurfaceDescriptor = unsafe { std::mem::zeroed() };
            let st = unsafe {
                (va.vaExportSurfaceHandle)(
                    self.display,
                    surf,
                    0x40000000, // VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2
                    0x07,       // VA_EXPORT_SURFACE_READ_WRITE
                    &mut desc as *mut _ as *mut c_void,
                )
            };
            if st != VA_STATUS_SUCCESS || desc.num_layers == 0 {
                eprintln!(
                    "[vaapi-vpp] NV12 export failed: st={st} layers={}",
                    desc.num_layers,
                );
                if desc.objects[0].fd >= 0 && st == VA_STATUS_SUCCESS {
                    unsafe { libc::close(desc.objects[0].fd) };
                }
                self.destroy_nv12_compute_surfaces(&mut surfaces);
                return false;
            }

            let fd = desc.objects[0].fd;
            let modifier = desc.objects[0].drm_format_modifier;

            // NV12 export layout varies by driver:
            //   Intel (iHD): 1 layer, 2 planes — offset[0]=Y, offset[1]=UV
            //   AMD (radeonsi): 2 layers, 1 plane each — layer 0=Y, layer 1=UV
            let (stride, uv_offset) = if desc.num_layers >= 2 {
                // AMD: 2 layers × 1 plane
                (desc.layers[0].pitch[0], desc.layers[1].offset[0])
            } else if desc.layers[0].num_planes >= 2 {
                // Intel: 1 layer × 2 planes
                (desc.layers[0].pitch[0], desc.layers[0].offset[1])
            } else {
                eprintln!(
                    "[vaapi-vpp] NV12 export: unexpected layout layers={} planes={}",
                    desc.num_layers, desc.layers[0].num_planes,
                );
                unsafe { libc::close(fd) };
                self.destroy_nv12_compute_surfaces(&mut surfaces);
                return false;
            };

            if self.verbose && i == 0 {
                eprintln!(
                    "[vaapi-vpp] NV12 export: {w}x{h} stride={stride} \
                     uv_offset={uv_offset} modifier=0x{modifier:016x} va_surface={surf}",
                );
            }
            self.gbm_nv12_buffers.push(GbmNv12Buffer {
                fd: std::sync::Arc::new(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }),
                stride,
                uv_offset,
                modifier,
                va_surface: surf,
            });
        }
        eprintln!(
            "[vaapi-vpp] exported {} NV12 surfaces for compute {w}x{h}",
            self.gbm_nv12_buffers.len(),
        );
        true
    }

    /// Get the raw VADisplay pointer (as usize for Send safety).
    #[allow(dead_code)]
    pub(crate) fn va_display_usize(&self) -> usize {
        self.display as usize
    }
}

// Dead code below removed: export_surfaces, upload_and_convert_bgra,
// convert_surface, convert_surface_flipped, convert_dmabuf, prime_import.
// The compute NV12 path (allocate_nv12_buffers + Vulkan compute shader)
// replaces the VPP BGRA→NV12 PRIME import pipeline entirely.

impl Drop for VppContext {
    fn drop(&mut self) {
        unsafe {
            let va = self.va;
            // Destroy compute NV12 surfaces (VA-API-allocated, exported to Vulkan).
            let mut compute_surfs: Vec<u32> = self
                .gbm_nv12_buffers
                .drain(..)
                .map(|b| {
                    drop(b.fd);
                    b.va_surface
                })
                .collect();
            if !compute_surfs.is_empty() {
                (va.vaDestroySurfaces)(
                    self.display,
                    compute_surfs.as_mut_ptr(),
                    compute_surfs.len() as i32,
                );
            }
            (va.vaDestroyContext)(self.display, self.context);
            (va.vaDestroySurfaces)(
                self.display,
                self.nv12_surfaces.as_mut_ptr(),
                NUM_NV12_SURFACES as i32,
            );
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
/// Number of input surfaces — 2 for pipelining: while the GPU encodes one,
/// the CPU can be uploading the next.
const NUM_INPUT_SURFACES: usize = 2;
const TOTAL_SURFACES: usize = NUM_REF_SURFACES + NUM_INPUT_SURFACES;
/// Coded output buffers — one per in-flight frame.
const NUM_CODED_BUFFERS: usize = 2;

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

/// BGRA → NV12 fast path: source and encode dimensions match exactly,
/// no per-pixel clamping needed.  Processes 2×2 input blocks at a time so
/// each chroma sample reuses the four BGRA pixels just loaded for Y.
///
/// Caller must ensure `src.len() >= src_w * src_h * 4` and that `dst` has
/// `y_offset + (src_h-1)*y_pitch + src_w` bytes for Y plus chroma room.
#[allow(clippy::too_many_arguments)]
unsafe fn bgra_to_nv12_fast(
    src: &[u8],
    dst: *mut u8,
    y_offset: usize,
    uv_offset: usize,
    y_pitch: usize,
    uv_pitch: usize,
    src_w: usize,
    src_h: usize,
) {
    debug_assert!(src.len() >= src_w * src_h * 4);
    let src_ptr = src.as_ptr();
    let chroma_h = src_h / 2;
    let chroma_w = src_w / 2;
    let row_stride = src_w * 4;

    for cy in 0..chroma_h {
        let row0 = cy * 2;
        let src_row0 = unsafe { src_ptr.add(row0 * row_stride) };
        let src_row1 = unsafe { src_ptr.add((row0 + 1) * row_stride) };
        let dst_y0 = unsafe { dst.add(y_offset + row0 * y_pitch) };
        let dst_y1 = unsafe { dst.add(y_offset + (row0 + 1) * y_pitch) };
        let dst_uv = unsafe { dst.add(uv_offset + cy * uv_pitch) };

        for cx in 0..chroma_w {
            let off = cx * 8; // 2 BGRA pixels = 8 bytes
            // Load 4 BGRA pixels (top-left, top-right, bottom-left, bottom-right).
            let b00 = unsafe { *src_row0.add(off) } as i32;
            let g00 = unsafe { *src_row0.add(off + 1) } as i32;
            let r00 = unsafe { *src_row0.add(off + 2) } as i32;
            let b01 = unsafe { *src_row0.add(off + 4) } as i32;
            let g01 = unsafe { *src_row0.add(off + 5) } as i32;
            let r01 = unsafe { *src_row0.add(off + 6) } as i32;
            let b10 = unsafe { *src_row1.add(off) } as i32;
            let g10 = unsafe { *src_row1.add(off + 1) } as i32;
            let r10 = unsafe { *src_row1.add(off + 2) } as i32;
            let b11 = unsafe { *src_row1.add(off + 4) } as i32;
            let g11 = unsafe { *src_row1.add(off + 5) } as i32;
            let r11 = unsafe { *src_row1.add(off + 6) } as i32;

            // BT.601 limited-range Y for each pixel.
            let y00 = ((66 * r00 + 129 * g00 + 25 * b00 + 128) >> 8) + 16;
            let y01 = ((66 * r01 + 129 * g01 + 25 * b01 + 128) >> 8) + 16;
            let y10 = ((66 * r10 + 129 * g10 + 25 * b10 + 128) >> 8) + 16;
            let y11 = ((66 * r11 + 129 * g11 + 25 * b11 + 128) >> 8) + 16;
            unsafe {
                *dst_y0.add(cx * 2) = y00.clamp(0, 255) as u8;
                *dst_y0.add(cx * 2 + 1) = y01.clamp(0, 255) as u8;
                *dst_y1.add(cx * 2) = y10.clamp(0, 255) as u8;
                *dst_y1.add(cx * 2 + 1) = y11.clamp(0, 255) as u8;
            }

            // U/V are linear in (R,G,B), so averaging the four BGR samples
            // first and computing one U/V is mathematically equivalent to
            // averaging the four U/V results — and ~4× cheaper.
            let avg_r = (r00 + r01 + r10 + r11) >> 2;
            let avg_g = (g00 + g01 + g10 + g11) >> 2;
            let avg_b = (b00 + b01 + b10 + b11) >> 2;
            let u = ((-38 * avg_r - 74 * avg_g + 112 * avg_b + 128) >> 8) + 128;
            let v = ((112 * avg_r - 94 * avg_g - 18 * avg_b + 128) >> 8) + 128;
            unsafe {
                *dst_uv.add(cx * 2) = u.clamp(0, 255) as u8;
                *dst_uv.add(cx * 2 + 1) = v.clamp(0, 255) as u8;
            }
        }
    }
}

/// BGRA → NV12 with per-pixel edge clamping.  Used when the encoder is
/// rounded up to even dimensions and the source isn't.
#[allow(clippy::too_many_arguments)]
unsafe fn bgra_to_nv12_padded(
    src: &[u8],
    dst: *mut u8,
    y_offset: usize,
    uv_offset: usize,
    y_pitch: usize,
    uv_pitch: usize,
    src_w: usize,
    src_h: usize,
    enc_w: usize,
    enc_h: usize,
) {
    for row in 0..enc_h {
        let sr = row.min(src_h - 1);
        let dst_row = unsafe { dst.add(y_offset + row * y_pitch) };
        for col in 0..enc_w {
            let sc = col.min(src_w - 1);
            let i = (sr * src_w + sc) * 4;
            let r = src[i + 2] as i32;
            let g = src[i + 1] as i32;
            let b = src[i] as i32;
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            unsafe { *dst_row.add(col) = y.clamp(0, 255) as u8 };
        }
    }
    let chroma_h = enc_h / 2;
    let chroma_w = enc_w / 2;
    for cy in 0..chroma_h {
        let dst_row = unsafe { dst.add(uv_offset + cy * uv_pitch) };
        for cx in 0..chroma_w {
            let row = cy * 2;
            let col = cx * 2;
            let mut r_sum = 0i32;
            let mut g_sum = 0i32;
            let mut b_sum = 0i32;
            for dy in 0..2usize {
                for dx in 0..2usize {
                    let sr = (row + dy).min(src_h - 1);
                    let sc = (col + dx).min(src_w - 1);
                    let i = (sr * src_w + sc) * 4;
                    r_sum += src[i + 2] as i32;
                    g_sum += src[i + 1] as i32;
                    b_sum += src[i] as i32;
                }
            }
            let avg_r = r_sum >> 2;
            let avg_g = g_sum >> 2;
            let avg_b = b_sum >> 2;
            let u = ((-38 * avg_r - 74 * avg_g + 112 * avg_b + 128) >> 8) + 128;
            let v = ((112 * avg_r - 94 * avg_g - 18 * avg_b + 128) >> 8) + 128;
            unsafe {
                *dst_row.add(cx * 2) = u.clamp(0, 255) as u8;
                *dst_row.add(cx * 2 + 1) = v.clamp(0, 255) as u8;
            }
        }
    }
}

/// In-flight encode submitted to the GPU but not yet drained.  The next
/// `encode_surface` call syncs on its `input_surface` and reads `coded_buf`.
struct PendingFrame {
    input_surface: VASurfaceID,
    coded_buf: VABufferID,
    /// Whether this submission is an IDR — needed for NAL header patching
    /// and SPS/PPS prepend checks when we eventually drain its bitstream.
    was_idr: bool,
}

pub struct VaapiDirectEncoder {
    va: &'static gpu_libs::VaFns,
    display: VADisplay,
    config: VAConfigID,
    context: VAContextID,
    surfaces: [VASurfaceID; TOTAL_SURFACES],
    coded_bufs: [VABufferID; NUM_CODED_BUFFERS],
    /// Next coded buffer slot to use for the upcoming submission.
    next_coded_slot: usize,
    /// Next input surface slot for upload_bgra/upload_nv12.
    next_input_slot: usize,
    /// In-flight encode awaiting drain on the next call.
    pending: Option<PendingFrame>,
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
    pub(crate) _drm_fd: OwnedFd,
    /// Optional VA-API VPP context for zero-copy DMA-BUF import.
    /// Present when VAEntrypointVideoProc is supported by the driver.
    pub(crate) vpp: Option<VppContext>,
    /// Cached vaDeriveImage per input surface slot.
    cached_input_images: [Option<CachedDerivedImage>; NUM_INPUT_SURFACES],
}

unsafe impl Send for VaapiDirectEncoder {}

impl VaapiDirectEncoder {
    pub fn try_new(
        width: u32,
        height: u32,
        vaapi_device: &str,
        qp: u8,
        verbose: bool,
        chroma: crate::surface_encoder::ChromaSubsampling,
    ) -> Result<Self, String> {
        if chroma.is_444() {
            return Err("VA-API H.264 4:4:4 encoding is not yet supported".into());
        }

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

        // Create config — leave rate control at driver default (CQP).  CBR
        // caused visible quality saccades on screen content as the rate
        // controller hit its QP ceiling during motion bursts.
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

        // Coded buffers (output bitstream) — one per pipeline slot, allocated
        // generously at ~1 byte per pixel.  While the GPU writes one, the CPU
        // reads the other.
        let coded_buf_size = width * height;
        let mut coded_bufs = [0u32; NUM_CODED_BUFFERS];
        for slot in 0..NUM_CODED_BUFFERS {
            let st = unsafe {
                (va.vaCreateBuffer)(
                    display,
                    context,
                    VAEncCodedBufferType,
                    coded_buf_size,
                    1,
                    ptr::null_mut(),
                    &mut coded_bufs[slot],
                )
            };
            if st != VA_STATUS_SUCCESS {
                unsafe {
                    for &prev in coded_bufs.iter().take(slot) {
                        (va.vaDestroyBuffer)(display, prev);
                    }
                    (va.vaDestroyContext)(display, context);
                    (va.vaDestroySurfaces)(display, surfaces.as_mut_ptr(), TOTAL_SURFACES as i32);
                    (va.vaDestroyConfig)(display, config);
                    (va.vaTerminate)(display);
                }
                return Err(format!("vaCreateBuffer(coded) failed: {st}"));
            }
        }

        let width_in_mbs = width.div_ceil(16) as u16;
        let height_in_mbs = height.div_ceil(16) as u16;

        if verbose {
            eprintln!(
                "[vaapi-direct] initialized H.264 encoder for {width}x{height} \
                 (ep={entrypoint}, qp={qp})"
            );
        }
        Ok(Self {
            va,
            display,
            config,
            context,
            surfaces,
            coded_bufs,
            next_coded_slot: 0,
            next_input_slot: 0,
            pending: None,
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
            vpp: unsafe {
                VppContext::try_new(
                    va,
                    display,
                    width,
                    height,
                    width,
                    height,
                    drm_fd.as_raw_fd(),
                    verbose,
                )
            },
            _drm_fd: drm_fd,
            cached_input_images: [None, None],
        })
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    pub fn gbm_buffers(&self) -> &[GbmExportedBuffer] {
        match &self.vpp {
            Some(vpp) => &vpp.gbm_buffers,
            None => &[],
        }
    }

    pub fn gbm_nv12_buffers(&self) -> &[GbmNv12Buffer] {
        match &self.vpp {
            Some(vpp) => &vpp.gbm_nv12_buffers,
            None => &[],
        }
    }

    /// Get the VADisplay as usize.
    #[allow(dead_code)]
    pub fn va_display_usize(&self) -> usize {
        match &self.vpp {
            Some(vpp) => vpp.va_display_usize(),
            None => 0,
        }
    }

    /// Get or create a cached derived image for the given input surface slot.
    fn derive_input_image(&mut self, slot: usize) -> Option<&CachedDerivedImage> {
        if self.cached_input_images[slot].is_none() {
            let surface = self.surfaces[NUM_REF_SURFACES + slot];
            let mut image = [0u8; VA_IMAGE_SIZE];
            let st = unsafe {
                (self.va.vaDeriveImage)(self.display, surface, image.as_mut_ptr() as *mut c_void)
            };
            if st != VA_STATUS_SUCCESS {
                return None;
            }
            self.cached_input_images[slot] = Some(CachedDerivedImage {
                image_id: r32(&image, VAIMG_ID_OFF),
                buf_id: r32(&image, VAIMG_BUF_OFF),
                y_pitch: r32(&image, VAIMG_PITCHES_OFF) as usize,
                uv_pitch: r32(&image, VAIMG_PITCHES_OFF + 4) as usize,
                y_offset: r32(&image, VAIMG_OFFSETS_OFF) as usize,
                uv_offset: r32(&image, VAIMG_OFFSETS_OFF + 4) as usize,
            });
        }
        self.cached_input_images[slot].as_ref()
    }

    /// Encode an NV12 frame (Y + UV interleaved planes).
    pub fn encode_nv12(
        &mut self,
        y_data: &[u8],
        uv_data: &[u8],
        y_stride: usize,
        uv_stride: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let slot = self.next_input_slot;
        self.next_input_slot = (slot + 1) % NUM_INPUT_SURFACES;
        self.upload_nv12(slot, y_data, uv_data, y_stride, uv_stride)?;
        let input_surface = self.surfaces[NUM_REF_SURFACES + slot];
        self.encode_surface(input_surface)
    }

    /// Encode from BGRA pixels — converts to NV12 and uploads.
    pub fn encode_bgra_padded(
        &mut self,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let slot = self.next_input_slot;
        self.next_input_slot = (slot + 1) % NUM_INPUT_SURFACES;
        self.upload_bgra(slot, bgra, src_w, src_h)?;
        let input_surface = self.surfaces[NUM_REF_SURFACES + slot];
        self.encode_surface(input_surface)
    }

    fn upload_nv12(
        &mut self,
        slot: usize,
        y_data: &[u8],
        uv_data: &[u8],
        src_y_stride: usize,
        src_uv_stride: usize,
    ) -> Option<()> {
        let img = self.derive_input_image(slot)?;
        let buf_id = img.buf_id;
        let y_pitch = img.y_pitch;
        let uv_pitch = img.uv_pitch;
        let y_offset = img.y_offset;
        let uv_offset = img.uv_offset;

        let mut map_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, buf_id, &mut map_ptr) };
        if st != VA_STATUS_SUCCESS {
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
            (self.va.vaUnmapBuffer)(self.display, buf_id);
        }
        Some(())
    }

    fn upload_bgra(&mut self, slot: usize, bgra: &[u8], src_w: usize, src_h: usize) -> Option<()> {
        let img = self.derive_input_image(slot)?;
        let buf_id = img.buf_id;
        let y_pitch = img.y_pitch;
        let uv_pitch = img.uv_pitch;
        let y_offset = img.y_offset;
        let uv_offset = img.uv_offset;

        let mut map_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, buf_id, &mut map_ptr) };
        if st != VA_STATUS_SUCCESS {
            return None;
        }

        let enc_w = self.width as usize;
        let enc_h = self.height as usize;
        let dst = map_ptr as *mut u8;

        unsafe {
            if src_w == enc_w && src_h == enc_h && src_w >= 2 && src_h >= 2 {
                bgra_to_nv12_fast(
                    bgra, dst, y_offset, uv_offset, y_pitch, uv_pitch, src_w, src_h,
                );
            } else {
                bgra_to_nv12_padded(
                    bgra, dst, y_offset, uv_offset, y_pitch, uv_pitch, src_w, src_h, enc_w, enc_h,
                );
            }
            (self.va.vaUnmapBuffer)(self.display, buf_id);
        }
        Some(())
    }

    /// Submit a frame to the GPU and drain the bitstream of the *previous*
    /// submission.  Returns `None` on the very first call (nothing to drain
    /// yet) or on submit failure.  The 1-frame pipeline depth lets the CPU
    /// upload of frame N+1 overlap with the GPU encode of frame N.
    pub(crate) fn encode_surface(&mut self, input_surface: VASurfaceID) -> Option<(Vec<u8>, bool)> {
        let is_idr = self.force_idr || self.frame_num == 0;
        if is_idr {
            self.frame_num = 0;
            self.idr_num += 1;
            self.force_idr = false;
        }

        let ref_surface = self.surfaces[self.cur_ref_idx];
        let recon_idx = (self.cur_ref_idx + 1) % NUM_REF_SURFACES;
        let recon_surface = self.surfaces[recon_idx];

        // Pick this submission's coded buffer slot.
        let coded_slot = self.next_coded_slot;
        let coded_buf = self.coded_bufs[coded_slot];
        self.next_coded_slot = (coded_slot + 1) % NUM_CODED_BUFFERS;

        // Build parameter buffers.
        let sps_buf = self.create_sps_buffer()?;
        let pps_buf = self.create_pps_buffer(is_idr, ref_surface, recon_surface, coded_buf)?;
        let slice_buf = self.create_slice_buffer(is_idr, ref_surface)?;

        let mut buffers: Vec<VABufferID> = vec![sps_buf, pps_buf, slice_buf];

        // Realtime tuning misc parameters.  AMD's VA-API backend defaults to
        // its slowest preset; QualityLevel flips it to "speed" for much lower
        // encode latency.  FrameRate hints the driver's internal pacing.
        if let Some(b) = create_misc_param_buffer(
            self.va,
            self.display,
            self.context,
            VAEncMiscParameterTypeQualityLevel,
            VAEncMiscParameterBufferQualityLevel {
                quality_level: REALTIME_QUALITY_LEVEL,
                va_reserved: [0; 4],
            },
        ) {
            buffers.push(b);
        }
        if let Some(b) = create_misc_param_buffer(
            self.va,
            self.display,
            self.context,
            VAEncMiscParameterTypeFrameRate,
            VAEncMiscParameterFrameRate {
                framerate: REALTIME_FPS,
                framerate_flags: 0,
                va_reserved: [0; 4],
            },
        ) {
            buffers.push(b);
        }

        // Submit packed SPS + PPS NALs on IDR frames so the driver includes
        // them in the coded buffer.
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

        // Submit (Begin/Render/End queue commands; do not block).
        let submit_ok = unsafe {
            let st = (self.va.vaBeginPicture)(self.display, self.context, input_surface);
            if st != VA_STATUS_SUCCESS {
                false
            } else {
                let st2 = (self.va.vaRenderPicture)(
                    self.display,
                    self.context,
                    buffers.as_mut_ptr(),
                    buffers.len() as i32,
                );
                let st3 = (self.va.vaEndPicture)(self.display, self.context);
                st2 == VA_STATUS_SUCCESS && st3 == VA_STATUS_SUCCESS
            }
        };
        // Param buffers are consumed by vaRenderPicture; the driver no longer
        // needs them after vaEndPicture even if encode hasn't finished.
        self.destroy_buffers(&buffers);

        // Drain the previous submission's bitstream — this is where we block
        // on the GPU.  The CPU upload for the next frame can begin as soon as
        // we return.
        let result = self.drain_pending();

        if submit_ok {
            self.frame_num += 1;
            self.cur_ref_idx = recon_idx;
            self.pending = Some(PendingFrame {
                input_surface,
                coded_buf,
                was_idr: is_idr,
            });
        }
        // If submit failed: pending stays empty (or whatever drain_pending
        // left it as — it's now None).  Next call retries with same state.

        result
    }

    /// Sync the in-flight frame and read its bitstream.  Returns `None` if
    /// nothing is pending or the readback fails.
    fn drain_pending(&mut self) -> Option<(Vec<u8>, bool)> {
        let pending = self.pending.take()?;
        let st = unsafe { (self.va.vaSyncSurface)(self.display, pending.input_surface) };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        let mut nal_data = self.read_coded_buffer(pending.coded_buf)?;
        if nal_data.is_empty() {
            return None;
        }

        // AMD VA-API outputs slice NALs with header byte 0x00 instead of
        // the correct H.264 NAL header.  Patch the first slice NAL header.
        let mut pos = 0;
        while let Some(sc) = find_annex_b_start(&nal_data[pos..]) {
            let abs = pos + sc;
            let hdr_pos = abs + if nal_data[abs + 2] == 1 { 3 } else { 4 };
            if hdr_pos < nal_data.len() {
                let nal_type = nal_data[hdr_pos] & 0x1f;
                if nal_type == 0 {
                    nal_data[hdr_pos] = if pending.was_idr {
                        0x65 // nal_ref_idc=3, nal_unit_type=5 (IDR)
                    } else {
                        0x41 // nal_ref_idc=2, nal_unit_type=1 (non-IDR)
                    };
                }
            }
            pos = hdr_pos + 1;
        }

        if pending.was_idr {
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
        Some((nal_data, pending.was_idr))
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
        // intra_period (offset 4, u32) — set to max so the driver never
        // auto-inserts keyframes; blit uses explicit force_idr instead.
        // 0 means "all intra" in VA-API, so use a large value.
        w32(&mut sps, 4, 0x7FFF_FFFF);
        // intra_idr_period (offset 8, u32) — same as intra_period.
        w32(&mut sps, 8, 0x7FFF_FFFF);
        // ip_period (offset 12, u32)
        w32(&mut sps, 12, 1);
        // bits_per_second (offset 16, u32) — 0 means CQP / driver default.
        w32(&mut sps, 16, 0);
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
        coded_buf: VABufferID,
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
        w32(&mut pps, 612, coded_buf);
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

    fn read_coded_buffer(&self, coded_buf: VABufferID) -> Option<Vec<u8>> {
        let mut buf_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, coded_buf, &mut buf_ptr) };
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
            (self.va.vaUnmapBuffer)(self.display, coded_buf);
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
        // Sync any in-flight encode before tearing down the surfaces it
        // references — otherwise the GPU could be writing into freed memory.
        if let Some(pending) = self.pending.take() {
            unsafe {
                (self.va.vaSyncSurface)(self.display, pending.input_surface);
            }
        }
        // Drop VPP context first — it shares our VA display handle and must
        // be destroyed before vaTerminate() invalidates the display.
        self.vpp.take();
        for slot in 0..NUM_INPUT_SURFACES {
            if let Some(img) = self.cached_input_images[slot].take() {
                unsafe {
                    (self.va.vaDestroyImage)(self.display, img.image_id);
                }
            }
        }
        unsafe {
            for &buf in &self.coded_bufs {
                (self.va.vaDestroyBuffer)(self.display, buf);
            }
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
    pub(crate) _drm_fd: OwnedFd,
    pub(crate) vpp: Option<VppContext>,
    /// Cached vaDeriveImage for the input surface (surfaces[2]).
    /// Avoids per-frame vaDeriveImage/vaDestroyImage driver calls on the
    /// fallback (non-zero-copy) encode path.
    cached_input_image: Option<CachedDerivedImage>,
}

unsafe impl Send for VaapiAv1Encoder {}

impl VaapiAv1Encoder {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        width: u32,
        height: u32,
        source_width: u32,
        source_height: u32,
        vaapi_device: &str,
        base_qindex: u8,
        verbose: bool,
        chroma: crate::surface_encoder::ChromaSubsampling,
    ) -> Result<Self, String> {
        if chroma.is_444() {
            return Err("VA-API AV1 4:4:4 encoding is not yet supported".into());
        }
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
        // NV12 surfaces must match the encoder context's resolution (64-pixel
        // aligned for AV1).  BGRA surfaces are created at the *source*
        // resolution so the compositor's external-output dimension check
        // passes and the zero-copy path is used — eliminating the staging
        // readback memcpy and CPU BGRA→NV12 conversion entirely.
        let vpp = unsafe {
            VppContext::try_new(
                va,
                display,
                width,
                height,
                source_width,
                source_height,
                drm_fd.as_raw_fd(),
                verbose,
            )
        };

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
            cached_input_image: None,
        })
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    /// Get or create a cached derived image for the input surface.
    fn derive_input_image(&mut self) -> Option<&CachedDerivedImage> {
        if self.cached_input_image.is_none() {
            let surface = self.surfaces[2];
            let mut image = [0u8; VA_IMAGE_SIZE];
            let st = unsafe {
                (self.va.vaDeriveImage)(self.display, surface, image.as_mut_ptr() as *mut c_void)
            };
            if st != VA_STATUS_SUCCESS {
                return None;
            }
            self.cached_input_image = Some(CachedDerivedImage {
                image_id: r32(&image, VAIMG_ID_OFF),
                buf_id: r32(&image, VAIMG_BUF_OFF),
                y_pitch: r32(&image, VAIMG_PITCHES_OFF) as usize,
                uv_pitch: r32(&image, VAIMG_PITCHES_OFF + 4) as usize,
                y_offset: r32(&image, VAIMG_OFFSETS_OFF) as usize,
                uv_offset: r32(&image, VAIMG_OFFSETS_OFF + 4) as usize,
            });
        }
        self.cached_input_image.as_ref()
    }

    pub fn gbm_buffers(&self) -> &[GbmExportedBuffer] {
        match &self.vpp {
            Some(vpp) => &vpp.gbm_buffers,
            None => &[],
        }
    }

    pub fn gbm_nv12_buffers(&self) -> &[GbmNv12Buffer] {
        match &self.vpp {
            Some(vpp) => &vpp.gbm_nv12_buffers,
            None => &[],
        }
    }

    #[allow(dead_code)]
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
        self.upload_nv12(y_data, uv_data, y_stride, uv_stride)?;
        let input_surface = self.surfaces[2];
        self.encode_surface(input_surface)
    }

    pub fn encode_bgra_padded(
        &mut self,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<(Vec<u8>, bool)> {
        self.upload_bgra(bgra, src_w, src_h)?;
        let input_surface = self.surfaces[2];
        self.encode_surface(input_surface)
    }

    fn upload_nv12(
        &mut self,
        y_data: &[u8],
        uv_data: &[u8],
        src_y_stride: usize,
        src_uv_stride: usize,
    ) -> Option<()> {
        let img = self.derive_input_image()?;
        let buf_id = img.buf_id;
        let y_pitch = img.y_pitch;
        let uv_pitch = img.uv_pitch;
        let y_offset = img.y_offset;
        let uv_offset = img.uv_offset;
        let mut map_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, buf_id, &mut map_ptr) };
        if st != VA_STATUS_SUCCESS {
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
        }
        Some(())
    }

    fn upload_bgra(&mut self, bgra: &[u8], src_w: usize, src_h: usize) -> Option<()> {
        let img = self.derive_input_image()?;
        let buf_id = img.buf_id;
        let y_pitch = img.y_pitch;
        let uv_pitch = img.uv_pitch;
        let y_offset = img.y_offset;
        let uv_offset = img.uv_offset;
        let mut map_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, buf_id, &mut map_ptr) };
        if st != VA_STATUS_SUCCESS {
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
        }
        Some(())
    }

    pub(crate) fn encode_surface(&mut self, input_surface: VASurfaceID) -> Option<(Vec<u8>, bool)> {
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
        // Max value so the driver never auto-inserts keyframes; blit
        // uses explicit force_idr instead.  0 means "all intra" in
        // VA-API, so use a large value.
        seq.intra_period = 0x7FFF_FFFF;
        seq.ip_period = 1;
        seq.bits_per_second = 0;
        seq.order_hint_bits_minus_1 = 7;
        // seq_fields bitfield (from va_enc_av1.h):
        //   bit  8: enable_order_hint = 1
        //   bit 12: enable_cdef = 1
        //   bit 14-15: bit_depth_minus8 = 2 (for 10-bit)
        //   bit 17: subsampling_x = 1
        //   bit 18: subsampling_y = 1
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
        ret.write_bool(false); // high bitdepth (8-bit)
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
        // Scale loop filter with QP.  At low base_qindex the encoder
        // produces few blocking artefacts, so aggressive deblocking just
        // blurs sharp edges (especially text).  Calibrated so QP 80 gives
        // the same [15, 15, 8, 8] as before, while QP 1 (Ultra) gives ~0.
        let qp = self.base_qindex as u32;
        let lf_y = ((qp * 15) / 80).min(63) as u8;
        let lf_uv = ((qp * 8) / 80).min(63) as u8;
        pic.filter_level = [lf_y, lf_y];
        pic.filter_level_u = lf_uv;
        pic.filter_level_v = lf_uv;
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
        if let Some(img) = self.cached_input_image.take() {
            unsafe {
                (self.va.vaDestroyImage)(self.display, img.image_id);
            }
        }
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
