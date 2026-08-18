//! Headless Wayland compositor using `wayland-server` directly.
//!
//! Handles
//! wl_compositor, wl_subcompositor, xdg_shell, wl_shm, wl_seat,
//! wl_output, and zwp_linux_dmabuf_v1.  Pixel data is read on every
//! commit and sent to the server via `CompositorEvent::SurfaceCommit`.

use crate::input_region::{self, RegionOp};
use crate::pointer_focus::{
    ButtonRouting, button_routing, focus_transition, keyboard_focus_after_popup_close,
};
use crate::positioner::PositionerGeometry;
use crate::touch_pacer::TouchPacer;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, LoopSignal, PostAction};
use rustc_hash::{FxHashMap, FxHashSet};
use wayland_protocols::wp::cursor_shape::v1::server::wp_cursor_shape_device_v1::{
    self, WpCursorShapeDeviceV1,
};
use wayland_protocols::wp::cursor_shape::v1::server::wp_cursor_shape_manager_v1::{
    self, WpCursorShapeManagerV1,
};
use wayland_protocols::wp::fractional_scale::v1::server::wp_fractional_scale_manager_v1::{
    self, WpFractionalScaleManagerV1,
};
use wayland_protocols::wp::fractional_scale::v1::server::wp_fractional_scale_v1::WpFractionalScaleV1;
use wayland_protocols::wp::linux_drm_syncobj::v1::server::wp_linux_drm_syncobj_manager_v1::{
    self, WpLinuxDrmSyncobjManagerV1,
};
use wayland_protocols::wp::linux_drm_syncobj::v1::server::wp_linux_drm_syncobj_surface_v1::{
    self, WpLinuxDrmSyncobjSurfaceV1,
};
use wayland_protocols::wp::linux_drm_syncobj::v1::server::wp_linux_drm_syncobj_timeline_v1::{
    self, WpLinuxDrmSyncobjTimelineV1,
};
use wayland_protocols::wp::presentation_time::server::wp_presentation::{
    self, WpPresentation,
};
use wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::{
    Kind as WpPresentationFeedbackKind, WpPresentationFeedback,
};
use wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_buffer_params_v1::{
    self, ZwpLinuxBufferParamsV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1;
use wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_v1::{
    self, ZwpLinuxDmabufV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::server::zwp_confined_pointer_v1::ZwpConfinedPointerV1;
use wayland_protocols::wp::pointer_constraints::zv1::server::zwp_locked_pointer_v1::ZwpLockedPointerV1;
use wayland_protocols::wp::pointer_constraints::zv1::server::zwp_pointer_constraints_v1::{
    self, ZwpPointerConstraintsV1,
};
use wayland_protocols::wp::primary_selection::zv1::server::zwp_primary_selection_device_manager_v1::{
    self, ZwpPrimarySelectionDeviceManagerV1,
};
use wayland_protocols::wp::primary_selection::zv1::server::zwp_primary_selection_device_v1::{
    self, ZwpPrimarySelectionDeviceV1,
};
use wayland_protocols::wp::primary_selection::zv1::server::zwp_primary_selection_offer_v1::{
    self, ZwpPrimarySelectionOfferV1,
};
use wayland_protocols::wp::primary_selection::zv1::server::zwp_primary_selection_source_v1::{
    self, ZwpPrimarySelectionSourceV1,
};
use wayland_protocols::wp::relative_pointer::zv1::server::zwp_relative_pointer_manager_v1::{
    self, ZwpRelativePointerManagerV1,
};
use wayland_protocols::wp::relative_pointer::zv1::server::zwp_relative_pointer_v1::ZwpRelativePointerV1;
use wayland_protocols::wp::text_input::zv3::server::zwp_text_input_manager_v3::{
    self, ZwpTextInputManagerV3,
};
use wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::{
    self, ZwpTextInputV3,
};
use wayland_protocols::wp::viewporter::server::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::server::wp_viewporter::{self, WpViewporter};
use wayland_protocols::xdg::activation::v1::server::xdg_activation_token_v1::{
    self, XdgActivationTokenV1,
};
use wayland_protocols::xdg::activation::v1::server::xdg_activation_v1::{
    self, XdgActivationV1,
};
use wayland_protocols::xdg::decoration::zv1::server::zxdg_decoration_manager_v1::{
    self, ZxdgDecorationManagerV1,
};
use wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::{
    self, ZxdgToplevelDecorationV1,
};
use wayland_protocols::xdg::foreign::zv2::server::zxdg_exported_v2::{
    self, ZxdgExportedV2,
};
use wayland_protocols::xdg::foreign::zv2::server::zxdg_exporter_v2::{
    self, ZxdgExporterV2,
};
use wayland_protocols::xdg::shell::server::xdg_popup::{self, XdgPopup};
use wayland_protocols::xdg::shell::server::xdg_positioner::XdgPositioner;
use wayland_protocols::xdg::shell::server::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::server::xdg_toplevel::{self, XdgToplevel};
use wayland_protocols::xdg::shell::server::xdg_wm_base::{self, XdgWmBase};

const MAX_FOREIGN_EXPORTS: usize = 4096;
use wayland_protocols::xdg::toplevel_drag::v1::server::xdg_toplevel_drag_manager_v1::{
    self, XdgToplevelDragManagerV1,
};
use wayland_protocols::xdg::toplevel_drag::v1::server::xdg_toplevel_drag_v1::{
    self, XdgToplevelDragV1,
};
use wayland_server::backend::ObjectId;
use wayland_server::backend::{ClientId, GlobalId};
use wayland_server::protocol::wl_buffer::WlBuffer;
use wayland_server::protocol::wl_callback::WlCallback;
use wayland_server::protocol::wl_compositor::WlCompositor;
use wayland_server::protocol::wl_data_device::{self, WlDataDevice};
use wayland_server::protocol::wl_data_device_manager::{self, DndAction, WlDataDeviceManager};
use wayland_server::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_server::protocol::wl_data_source::{self, WlDataSource};
use wayland_server::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_server::protocol::wl_output::{self, WlOutput};
use wayland_server::protocol::wl_pointer::{self, WlPointer};
use wayland_server::protocol::wl_region::WlRegion;
use wayland_server::protocol::wl_seat::{self, WlSeat};
use wayland_server::protocol::wl_shm::{self, WlShm};
use wayland_server::protocol::wl_shm_pool::WlShmPool;
use wayland_server::protocol::wl_subcompositor::WlSubcompositor;
use wayland_server::protocol::wl_subsurface::WlSubsurface;
use wayland_server::protocol::wl_surface::WlSurface;
use wayland_server::protocol::wl_touch::{self, WlTouch};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
};

// ---------------------------------------------------------------------------
// Public types (re-exported from lib.rs)
// ---------------------------------------------------------------------------

/// Pixel data in its native format, avoiding unnecessary colorspace conversions.
#[derive(Clone)]
pub enum PixelData {
    Bgra(Arc<Vec<u8>>),
    Rgba(Arc<Vec<u8>>),
    /// The frame exists only on the GPU: a Vulkan Video encoder owns the
    /// surface and nothing asked for CPU pixels, so the staging readback
    /// was skipped rather than copied into a `Vec` no one would read.
    ///
    /// It still carries a commit.  The server's surface state machine runs
    /// off `SurfaceCommit` — sizes, generation counter, and the
    /// `last_pixels` entry that gates encoder creation — so a surface that
    /// published nothing at all would never get an encoder in the first
    /// place.  Consumers that need real pixels must treat this as "no
    /// frame this tick", not as an empty one.
    GpuOnly,
    Nv12 {
        data: Arc<Vec<u8>>,
        y_stride: usize,
        uv_stride: usize,
    },
    DmaBuf {
        fd: Arc<OwnedFd>,
        fourcc: u32,
        modifier: u64,
        stride: u32,
        offset: u32,
        /// When true the image origin is bottom-left (OpenGL convention).
        /// The Vulkan renderer flips the V texture coordinate to display
        /// the image right-side-up.
        y_invert: bool,
    },
    /// NV12 in a single DMA-BUF (Y at offset 0, UV at uv_offset) —
    /// zero-copy from Vulkan compute shader to VA-API encoder.
    Nv12DmaBuf {
        fd: Arc<OwnedFd>,
        stride: u32,
        uv_offset: u32,
        width: u32,
        height: u32,
        /// Optional sync_fd exported from the Vulkan fence that guards the
        /// BGRA→NV12 compute dispatch.  The consumer (encoder) must poll()
        /// this fd before reading the NV12 data.  `None` when implicit
        /// DMA-BUF fencing handles synchronisation (linear buffers).
        sync_fd: Option<Arc<OwnedFd>>,
    },
    /// NV12 in a single Vulkan allocation exported as `OPAQUE_FD` — the
    /// NVENC zero-copy path.
    ///
    /// Deliberately not `Nv12DmaBuf`, which it otherwise resembles. This fd
    /// is *not* a `dma_buf`: CUDA refuses those (`CUDA_ERROR_UNKNOWN`) and
    /// only accepts an `OPAQUE_FD` handle. Two consequences follow, and both
    /// are why the variants stay apart. `Nv12DmaBuf`'s consumer resolves fds
    /// by inode against the VA-API surfaces it exported, which this fd will
    /// never match; and an `OPAQUE_FD` allocation carries none of the
    /// implicit fencing a `dma_buf` does, so `sync_fd` here is load-bearing
    /// rather than an optimisation.
    Nv12OpaqueFd {
        fd: Arc<OwnedFd>,
        /// Process-unique id for the allocation behind `fd`. The consumer
        /// caches its CUDA import and NVENC registration against this
        /// rather than against the fd number, which the kernel recycles
        /// once a buffer is closed — a stale hit there would point NVENC at
        /// freed VRAM.
        buf_id: u64,
        stride: u32,
        uv_offset: u32,
        width: u32,
        height: u32,
        /// Plane layout: false = NV12 (Y + interleaved half-height UV at
        /// `uv_offset`); true = planar YUV444 (U at `uv_offset`, V one
        /// full plane later).  The consumer must register the matching
        /// NVENC buffer format — a mismatch reads chroma from the wrong
        /// rows and NVENC rejects or garbles the picture.
        is_444: bool,
        /// sync_file exported from the fence guarding the BGRA→NV12 compute
        /// dispatch. The consumer MUST poll this before reading: nothing
        /// else orders CUDA's reads against the compositor's writes, and an
        /// unsynchronised read tears intermittently — worst at high frame
        /// rates, i.e. exactly where a short test looks fine.
        sync_fd: Option<Arc<OwnedFd>>,
    },
    /// VA-API surface ready for VPP/encode — zero-copy path.
    VaSurface {
        surface_id: u32,
        va_display: usize,
        _fd: Arc<OwnedFd>,
    },
}

/// A bitstream a compositor-resident encoder produced for exactly one
/// client.
///
/// Unlike `PixelData`, this is never shared between subscribers: Vulkan
/// Video owns one encoder per `(surface_id, client_id)`, so each viewer
/// gets its own GOP and its own quantizer.
pub struct EncodedFrame {
    pub surface_id: u16,
    pub client_id: u64,
    pub width: u32,
    pub height: u32,
    pub data: Arc<Vec<u8>>,
    pub is_keyframe: bool,
    /// Codec flag matching `SURFACE_FRAME_CODEC_*` constants.
    pub codec_flag: u8,
}

/// A DMA-BUF fd exported from a VA-API surface for use as a GPU
/// renderer output target.  The compositor renders into the EGL FBO
/// backed by this fd; the encoder references the VA-API surface by ID.
/// Per-plane offset + pitch for multi-plane DMA-BUF import (e.g. AMD DCC).
#[derive(Clone, Copy, Default)]
pub struct ExternalOutputPlane {
    pub offset: u32,
    pub pitch: u32,
}

pub struct ExternalOutputBuffer {
    pub fd: Arc<OwnedFd>,
    pub fourcc: u32,
    pub modifier: u64,
    pub stride: u32,
    pub offset: u32,
    pub width: u32,
    pub height: u32,
    pub va_surface_id: u32,
    pub va_display: usize,
    /// All planes for this buffer (main surface + optional metadata planes).
    pub planes: Vec<ExternalOutputPlane>,
    /// NV12 output for the compute shader.  When present, the compositor
    /// imports it into Vulkan (as buffer if linear, as image if tiled),
    /// writes NV12 via compute, and returns Nv12DmaBuf.
    pub nv12_fd: Option<Arc<OwnedFd>>,
    pub nv12_stride: u32,
    pub nv12_uv_offset: u32,
    /// DRM format modifier for the NV12 surface (0 = linear).
    pub nv12_modifier: u64,
    /// NV12 surface dimensions (may be larger than width×height due to
    /// encoder alignment, e.g. AV1 64-pixel superblock alignment).
    pub nv12_width: u32,
    pub nv12_height: u32,
}

pub mod drm_fourcc {
    pub const ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
    pub const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
    pub const ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
    pub const XBGR8888: u32 = u32::from_le_bytes(*b"XB24");
    pub const NV12: u32 = u32::from_le_bytes(*b"NV12");
}

impl PixelData {
    pub fn to_rgba(&self, width: u32, height: u32) -> Vec<u8> {
        let w = width as usize;
        let h = height as usize;
        match self {
            // No CPU readback: the allocation behind an OPAQUE_FD is
            // DEVICE_LOCAL VRAM and the fd is an NVIDIA-internal handle,
            // not a dma_buf — there is nothing to mmap. Callers that need
            // pixels on the CPU (thumbnails, the software downscale) must
            // not be handed this variant in the first place; the server
            // only routes it to an NVENC encoder, which reads it on the
            // GPU. Returning empty rather than panicking keeps a
            // mis-route to a black frame instead of a crash, and
            // `is_empty()` below reports it honestly.
            // Same for a GPU-only commit: it deliberately carries no
            // pixels, and the server never routes it to a CPU consumer.
            PixelData::Nv12OpaqueFd { .. } | PixelData::GpuOnly => Vec::new(),
            PixelData::Rgba(data) => data.as_ref().clone(),
            PixelData::Bgra(data) => {
                let mut rgba = Vec::with_capacity(w * h * 4);
                for px in data.chunks_exact(4) {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
                rgba
            }
            PixelData::Nv12 {
                data,
                y_stride,
                uv_stride,
            } => {
                let y_plane_size = *y_stride * h;
                let uv_h = h.div_ceil(2);
                let uv_plane_size = *uv_stride * uv_h;
                if data.len() < y_plane_size + uv_plane_size {
                    return Vec::new();
                }
                let y_plane = &data[..y_plane_size];
                let uv_plane = &data[y_plane_size..];
                let mut rgba = Vec::with_capacity(w * h * 4);
                for row in 0..h {
                    for col in 0..w {
                        let y = y_plane[row * y_stride + col];
                        let uv_idx = (row / 2) * uv_stride + (col / 2) * 2;
                        if uv_idx + 1 >= uv_plane.len() {
                            rgba.extend_from_slice(&[0, 0, 0, 255]);
                            continue;
                        }
                        let u = uv_plane[uv_idx];
                        let v = uv_plane[uv_idx + 1];
                        let [r, g, b] = yuv420_to_rgb(y, u, v);
                        rgba.extend_from_slice(&[r, g, b, 255]);
                    }
                }
                rgba
            }
            PixelData::DmaBuf {
                fd,
                fourcc,
                stride,
                offset,
                ..
            } => {
                let raw = fd.as_raw_fd();
                let stride_usize = *stride as usize;
                let plane_offset = *offset as usize;
                let map_size = plane_offset + stride_usize * h;
                if map_size == 0 {
                    return Vec::new();
                }
                // Best-effort DMA-BUF sync: try a non-blocking poll to see
                // if the implicit GPU fence is signaled.  If it is, bracket
                // the read with SYNC_START/SYNC_END for cache coherency.
                // If poll fails (fd doesn't support it, e.g. Vulkan WSI) or
                // the fence isn't ready yet, skip the sync and read anyway —
                // a slightly stale frame is far better than a black surface.
                const DMA_BUF_SYNC_READ: u64 = 1;
                const DMA_BUF_SYNC_START: u64 = 0;
                const DMA_BUF_SYNC_END: u64 = 4;
                const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x40086200;
                let did_sync = {
                    let mut pfd = libc::pollfd {
                        fd: raw,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
                    if ready > 0 {
                        let s: u64 = DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ;
                        unsafe { libc::ioctl(raw, DMA_BUF_IOCTL_SYNC as _, &s) };
                        true
                    } else {
                        false
                    }
                };
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        map_size,
                        libc::PROT_READ,
                        libc::MAP_SHARED,
                        raw,
                        0,
                    )
                };
                if ptr == libc::MAP_FAILED {
                    if did_sync {
                        let s: u64 = DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ;
                        unsafe { libc::ioctl(raw, DMA_BUF_IOCTL_SYNC as _, &s) };
                    }
                    return Vec::new();
                }
                let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, map_size) };
                let row_bytes = w * 4;
                let mut pixels = Vec::with_capacity(w * h * 4);
                for row in 0..h {
                    let start = plane_offset + row * stride_usize;
                    if start + row_bytes <= slice.len() {
                        pixels.extend_from_slice(&slice[start..start + row_bytes]);
                    }
                }
                let is_bgr_mem = matches!(*fourcc, drm_fourcc::ARGB8888 | drm_fourcc::XRGB8888);
                let force_alpha = matches!(*fourcc, drm_fourcc::XRGB8888 | drm_fourcc::XBGR8888);
                for px in pixels.chunks_exact_mut(4) {
                    if is_bgr_mem {
                        px.swap(0, 2);
                    }
                    if force_alpha {
                        px[3] = 255;
                    }
                }
                unsafe { libc::munmap(ptr, map_size) };
                if did_sync {
                    let s: u64 = DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ;
                    unsafe { libc::ioctl(raw, DMA_BUF_IOCTL_SYNC as _, &s) };
                }
                pixels
            }
            PixelData::Nv12DmaBuf {
                fd,
                stride,
                uv_offset,
                width: nv12_w,
                height: nv12_h,
                sync_fd,
            } => {
                // The compositor writes BGRA → NV12 from a Vulkan compute
                // shader into this DMA-BUF.  Wait on the fence (if any) so
                // we don't CPU-read a half-written buffer.  Without this,
                // thumbnails (scaled subscriptions, which need CPU RGBA for
                // the software downscale) get garbage or stale pixels.
                if let Some(sync) = sync_fd {
                    let mut pfd = libc::pollfd {
                        fd: sync.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // Up to 10 ms: at 60 fps we have ~16 ms of budget; we
                    // must not block the server delivery tick for longer
                    // than one frame's worth of time.
                    unsafe {
                        libc::poll(&mut pfd, 1, 10);
                    }
                }
                let nw = *nv12_w as usize;
                let nh = *nv12_h as usize;
                let stride_usize = *stride as usize;
                let uv_off = *uv_offset as usize;
                let y_plane_size = stride_usize * nh;
                let uv_h = nh.div_ceil(2);
                let uv_plane_size = stride_usize * uv_h;
                let map_size = uv_off + uv_plane_size;
                if map_size == 0 || nw == 0 || nh == 0 {
                    return Vec::new();
                }
                let raw = fd.as_raw_fd();
                const DMA_BUF_SYNC_READ: u64 = 1;
                const DMA_BUF_SYNC_START: u64 = 0;
                const DMA_BUF_SYNC_END: u64 = 4;
                const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x40086200;
                let s_start: u64 = DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ;
                let did_sync = unsafe { libc::ioctl(raw, DMA_BUF_IOCTL_SYNC as _, &s_start) == 0 };
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        map_size,
                        libc::PROT_READ,
                        libc::MAP_SHARED,
                        raw,
                        0,
                    )
                };
                if ptr == libc::MAP_FAILED {
                    if did_sync {
                        let s_end: u64 = DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ;
                        unsafe { libc::ioctl(raw, DMA_BUF_IOCTL_SYNC as _, &s_end) };
                    }
                    return Vec::new();
                }
                let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, map_size) };
                let y_plane = &slice[..y_plane_size.min(slice.len())];
                let uv_plane = &slice[uv_off.min(slice.len())..];
                // The caller asks for (w, h) — typically matches (nw, nh)
                // but we guard anyway.
                let out_w = w.min(nw);
                let out_h = h.min(nh);
                let mut rgba = Vec::with_capacity(w * h * 4);
                for row in 0..out_h {
                    for col in 0..out_w {
                        let y_idx = row * stride_usize + col;
                        let uv_idx = (row / 2) * stride_usize + (col / 2) * 2;
                        if y_idx >= y_plane.len() || uv_idx + 1 >= uv_plane.len() {
                            rgba.extend_from_slice(&[0, 0, 0, 255]);
                            continue;
                        }
                        let y = y_plane[y_idx];
                        let u = uv_plane[uv_idx];
                        let v = uv_plane[uv_idx + 1];
                        let [r, g, b] = yuv420_to_rgb(y, u, v);
                        rgba.extend_from_slice(&[r, g, b, 255]);
                    }
                    // Pad row if caller asked for more width than we have.
                    for _ in out_w..w {
                        rgba.extend_from_slice(&[0, 0, 0, 255]);
                    }
                }
                for _ in out_h..h {
                    for _ in 0..w {
                        rgba.extend_from_slice(&[0, 0, 0, 255]);
                    }
                }
                unsafe { libc::munmap(ptr, map_size) };
                if did_sync {
                    let s_end: u64 = DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ;
                    unsafe { libc::ioctl(raw, DMA_BUF_IOCTL_SYNC as _, &s_end) };
                }
                rgba
            }
            PixelData::VaSurface { .. } => Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            PixelData::Bgra(v) | PixelData::Rgba(v) => v.is_empty(),
            PixelData::Nv12 { data, .. } => data.is_empty(),
            // `GpuOnly` is not an empty frame — it is a real commit whose
            // pixels live on the GPU. Reporting it empty would make
            // `composite_toplevel_into_pending` drop it, and a surface that
            // publishes nothing never gets an encoder at all.
            PixelData::DmaBuf { .. }
            | PixelData::VaSurface { .. }
            | PixelData::Nv12DmaBuf { .. }
            | PixelData::Nv12OpaqueFd { .. }
            | PixelData::GpuOnly => false,
        }
    }

    pub fn is_dmabuf(&self) -> bool {
        matches!(self, PixelData::DmaBuf { .. })
    }

    pub fn is_va_surface(&self) -> bool {
        matches!(self, PixelData::VaSurface { .. })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum CursorImage {
    Named(String),
    Custom {
        /// Surface-local (logical) hotspot, straight from
        /// `wl_pointer.set_cursor`.
        hotspot_x: u16,
        hotspot_y: u16,
        /// Dimensions of `rgba`, in buffer pixels.
        width: u16,
        height: u16,
        /// The cursor surface's `buffer_scale`.  `width / scale` is the cursor's
        /// logical size — the same space the hotspot is in.  A consumer must
        /// scale artwork and hotspot by one factor or the hotspot drifts off the
        /// artwork on every HiDPI surface.
        scale: u16,
        rgba: Vec<u8>,
    },
    Hidden,
}

pub enum CompositorEvent {
    SurfaceCreated {
        surface_id: u16,
        title: String,
        app_id: String,
        parent_id: u16,
        width: u16,
        height: u16,
    },
    SurfaceDestroyed {
        surface_id: u16,
    },
    /// A client asked to activate (raise/focus) a surface via
    /// xdg_activation_v1; forwarded so the frontend can raise the pane.
    SurfaceActivated {
        surface_id: u16,
    },
    /// The focused Wayland client committed `zwp_text_input_v3` state for
    /// one toplevel. `requested` is true only for a freshly committed
    /// `enable`; metadata-only commits must not reopen a keyboard the user
    /// dismissed.
    SurfaceTextInput {
        surface_id: u16,
        enabled: bool,
        requested: bool,
        hint: u32,
        purpose: u32,
        /// Where the app draws the text under edit, in the composited
        /// frame's physical pixels (the same space as surface pointer
        /// positions).  `None` while the app has named no rectangle.
        cursor_rect: Option<(i32, i32, i32, i32)>,
    },
    SurfaceCommit {
        surface_id: u16,
        width: u32,
        height: u32,
        pixels: PixelData,
        /// CLOCK_MONOTONIC milliseconds at commit time so the server can
        /// stamp surface frames with the source's presentation timing
        /// rather than the (jittery) encode-delivery wall clock.
        timestamp_ms: u32,
        /// Microseconds within `timestamp_ms`, preserving sub-ms cadence.
        timestamp_sub_us: u16,
        /// This frame is for the pixel cache only — an on-demand BGRA
        /// readback published while an NV12 zero-copy stream owns the
        /// same key.  Encoders must not consume it: it would re-encode
        /// a frame the stream already has, through NVENC's own ARGB
        /// conversion whose rounding differs from the zero-copy
        /// shader's — a visible one-frame shift for pure waste.
        encoder_skip: bool,
    },
    /// A compositor-resident encoder produced a bitstream for one client.
    /// Carries its own timestamp for the same reason `SurfaceCommit` does.
    SurfaceEncoded {
        frame: EncodedFrame,
        timestamp_ms: u32,
        timestamp_sub_us: u16,
    },
    /// No Vulkan Video encoder could be created for this `(surface,
    /// client)` pair. The server may retry the codec at 4:2:0 before moving
    /// to a server-side encoder.
    VulkanEncoderUnavailable {
        surface_id: u16,
        client_id: u64,
        /// The session was built and then failed to encode, rather than
        /// never being built at all.  A driver that accepts a profile it
        /// cannot encode does so for every surface, which makes this the
        /// one refusal worth remembering beyond the pair that hit it;
        /// a session that could not be created may simply have been too
        /// large, or asked for an image another session was reading.
        after_encode_failures: bool,
    },
    SurfaceTitle {
        surface_id: u16,
        title: String,
    },
    SurfaceAppId {
        surface_id: u16,
        app_id: String,
    },
    /// The stamped identity of a toplevel's application, sent once at creation
    /// for surfaces that arrived on a per-app socket.
    ///
    /// A separate event rather than a field on `SurfaceCreated` because it
    /// applies to a minority of surfaces and because `SurfaceAppId` already
    /// established that shape for per-surface metadata.
    SurfaceOrigin {
        surface_id: u16,
        sandbox_engine: String,
        app_id: String,
        instance_id: String,
    },
    SurfaceResized {
        surface_id: u16,
        width: u16,
        height: u16,
        /// The same size in surface-logical pixels — what the Wayland
        /// client thinks its window measures, before the output scale is
        /// applied.  Physical alone cannot tell a viewer how large the
        /// window *is*: 1200x900 is a 400x300 window at 3x and a 1200x900
        /// one at 1x, and a viewer that assumes its own scale draws the
        /// former three times too big.
        logical_width: u16,
        logical_height: u16,
    },
    ClipboardContent {
        mime_type: String,
        data: Vec<u8>,
    },
    /// Clipboard authority changed.  Browser clients use this to decide
    /// whether Ctrl/Cmd+V should import the host clipboard or preserve a
    /// Wayland client's multi-MIME selection for a direct client splice.
    ClipboardOwner {
        wayland: bool,
    },
    SurfaceCursor {
        surface_id: u16,
        cursor: CursorImage,
    },
    /// The compositor retired a direct-touch sequence on its own — the
    /// contact's target unmapped, or touch was disabled.  Without this the
    /// server would keep believing `owner_id` holds a live sequence and go on
    /// refusing every other viewer's contacts.  `None` means every owner.
    TouchCancelled {
        owner_id: Option<u64>,
    },
}

/// Who a Wayland connection belongs to.
///
/// Stamped by whoever created the socket the client arrived on, never asserted
/// by the client itself — which is the whole point. `app_id` from
/// `xdg_toplevel.set_app_id` is a free-form string an application says about
/// itself, unverified and often wrong; `SO_PEERCRED` gives a pid that a
/// zygote-forking or re-execing application immediately invalidates, and a
/// passed connection fd means one socket need not mean one process at all.
///
/// The fields mirror `wp_security_context_v1` so that protocol can be wired to
/// this later, letting a third-party sandbox engine stamp its own sockets.
/// Nothing here depends on that protocol existing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppIdentity {
    /// What created the socket, e.g. `blit` for the session supervisor.
    pub sandbox_engine: String,
    /// Stable across restarts of the same application.
    pub app_id: String,
    /// Distinguishes two concurrent runs of one application.
    pub instance_id: String,
}

pub enum CompositorCommand {
    /// Adopt an already-bound listening socket whose clients are known to
    /// belong to `identity`.
    ///
    /// The caller binds the socket so the app can be spawned the instant this is
    /// sent — there is no window in which the socket is named but not yet
    /// listening. Ownership of the path stays with the caller, which unlinks it;
    /// the compositor only accepts on the fd.
    AddAppSocket {
        fd: OwnedFd,
        identity: AppIdentity,
    },
    KeyInput {
        surface_id: u16,
        keycode: u32,
        pressed: bool,
        /// Browser key event `timeStamp` in whole ms; `0` for unknown.
        time_ms: u32,
    },
    PointerMotion {
        surface_id: u16,
        x: f64,
        y: f64,
        /// Browser event `timeStamp` in whole ms; `0` for unknown.
        time_ms: u32,
    },
    PointerButton {
        surface_id: u16,
        button: u32,
        pressed: bool,
        time_ms: u32,
    },
    /// A scroll event.
    ///
    /// `dx`/`dy` are smooth distance in the composited frame's pixel
    /// space, the same space `PointerMotion` uses; they are converted to
    /// surface-logical pixels on the way out. `v120_*` is discrete wheel
    /// travel in 120ths of a detent.
    ///
    /// `source` is `None` when the sender did not classify the device, in
    /// which case no `wl_pointer.axis_source` is emitted. That is not the
    /// same as harmless: the enum's zero value is `wheel`, so an
    /// unclassified scroll still reads as a notched wheel to most
    /// toolkits.
    PointerAxis {
        surface_id: u16,
        dx: f64,
        dy: f64,
        v120_x: i16,
        v120_y: i16,
        source: Option<u8>,
        stop: bool,
        /// Browser wheel event `timeStamp` in whole ms; `0` for unknown.
        time_ms: u32,
    },
    SetTouchEnabled {
        enabled: bool,
    },
    Touch {
        owner_id: u64,
        surface_id: u16,
        phase: TouchPhase,
        /// The originating browser's `TouchEvent.timeStamp` in whole ms, in its
        /// own epoch.  Used only for the spacing between events.
        time_ms: u32,
        contacts: Vec<TouchPoint>,
    },
    SurfaceResize {
        surface_id: u16,
        width: u16,
        height: u16,
        scale_120: u16,
    },
    SurfaceFocus {
        surface_id: u16,
    },
    SurfaceClose {
        surface_id: u16,
    },
    ClipboardOffer {
        mime_type: String,
        data: Vec<u8>,
    },
    /// Begin (or retarget) a browser-initiated drag over a surface.
    /// `mimes` are what the browser can offer; the data arrives with
    /// [`CompositorCommand::DragDrop`].  Coordinates are in the composited
    /// frame's physical pixel space, as in `PointerMotion`.
    ///
    /// `planned_uri_list` is the pre-staged `text/uri-list` payload from an
    /// ENTER carrying the item plan: the staging files exist (empty) from
    /// the start, so a `receive("text/uri-list")` during hover can be
    /// answered immediately.  `None` parks every receive until the drop.
    DragEnter {
        surface_id: u16,
        x: f64,
        y: f64,
        mimes: Vec<String>,
        planned_uri_list: Option<Vec<u8>>,
    },
    /// Move an in-flight browser drag.
    DragMotion {
        surface_id: u16,
        x: f64,
        y: f64,
    },
    /// The drag left the surface; the session ends.
    DragLeave,
    /// Complete a browser drag: `offers` is the final payload map
    /// (mime → bytes), served to the target on `receive` until it finishes
    /// or destroys the offer.
    DragDrop {
        surface_id: u16,
        x: f64,
        y: f64,
        offers: Vec<(String, Vec<u8>)>,
    },
    /// Abort an in-flight browser drag (Escape / drag left the window).
    DragCancel,
    PrimaryOffer {
        mime_type: String,
        data: Vec<u8>,
    },
    Capture {
        surface_id: u16,
        scale_120: u16,
        reply: mpsc::SyncSender<Option<(u32, u32, Vec<u8>)>>,
    },
    RequestFrame {
        surface_id: u16,
        /// Absolute point on the server's fixed-rate refresh timeline.
        presentation_at: std::time::Instant,
    },
    /// Keep a native CPU-readable composite available for an internal
    /// PipeWire window ScreenCast. This is independent of browser encoders.
    SetScreenCastActive {
        surface_id: u16,
        active: bool,
    },
    /// Re-composite a toplevel from its current committed state and
    /// republish the pixels, without waiting for the client to commit.
    /// An idle Wayland app volunteers nothing, so when the server's
    /// pixel cache for a surface is empty (every prior viewer left and
    /// took the cache entry with them), a fresh subscriber would wait
    /// forever for pixels that only a composite can produce.
    Recomposite {
        surface_id: u16,
    },
    ReleaseKeys {
        keycodes: Vec<u32>,
    },
    /// List available clipboard MIME types.
    ClipboardListMimes {
        reply: mpsc::SyncSender<Vec<String>>,
    },
    /// Read clipboard content for a specific MIME type.
    ClipboardGet {
        mime_type: String,
        reply: mpsc::SyncSender<Option<Vec<u8>>>,
    },
    /// Set externally-allocated DMA-BUF fds as GPU renderer output
    /// targets for a (surface, encoder target size) pair.  Each
    /// per-client encoder owns its own pool of target-sized buffers;
    /// the compositor composites at native size, then GPU-blits
    /// (LINEAR) into each registered target so every viewer gets a
    /// zero-copy stream at its own physical viewport.  Pass an empty
    /// `buffers` to clear a target.
    ///
    /// `native_w`/`native_h` are the composite size `(target_w, target_h)`
    /// was inscribed into.  The renderer stops filling the target once the
    /// composite moves off that size, so a resize cannot deliver a frame
    /// squashed into the previous aspect.
    SetExternalOutputBuffers {
        surface_id: u32,
        target_w: u32,
        target_h: u32,
        native_w: u32,
        native_h: u32,
        buffers: Vec<ExternalOutputBuffer>,
    },
    /// Allocate a server-side BGRA "downscale target" for a per-client
    /// encoder that doesn't import GBM buffers (NVENC, software h264,
    /// software AV1).  After registration the renderer GPU-blits
    /// (LINEAR) the native composite into a target-sized BGRA image
    /// then copies it into a CPU-mapped staging buffer; the resulting
    /// frame is delivered as `PixelData::Bgra` sized at
    /// `(target_w, target_h)` so the per-client encoder consumes
    /// already-downscaled pixels.  Sending the same `(surface_id,
    /// target_w, target_h)` again reuses the buffer but re-stamps the
    /// native size, so a surface that returns to a size it held before
    /// does not inherit that visit's stale stamp.
    ///
    /// `native_w`/`native_h`: see `SetExternalOutputBuffers`.
    ///
    /// `want_nv12_opaque` asks for the NVENC zero-copy shape instead: the
    /// renderer still blits the composite into the target-sized BGRA image,
    /// but then runs the BGRA→NV12 compute pass into a Vulkan buffer
    /// exported as `OPAQUE_FD` and delivers `PixelData::Nv12OpaqueFd`,
    /// skipping the staging copy and the `Vec` that follows it. Only NVENC
    /// can consume that — CUDA is the only importer of an `OPAQUE_FD`
    /// handle — so anything else must leave this false and take the BGRA
    /// path. Best-effort: if the export fails the renderer registers the
    /// ordinary BGRA target, so the caller still gets frames.
    RegisterDownscaleTarget {
        surface_id: u32,
        target_w: u32,
        target_h: u32,
        native_w: u32,
        native_h: u32,
        want_nv12_opaque: bool,
        /// Keep publishing host-visible BGRA when a CPU reader shares this
        /// target with one or more zero-copy NVENC readers.
        want_cpu_pixels: bool,
        /// With `want_nv12_opaque`: the layout the consuming session
        /// expects — planar YUV444 for a 4:4:4 NVENC session, NV12
        /// otherwise.  Ignored when `want_nv12_opaque` is false.
        opaque_is_444: bool,
    },
    /// Re-stamp an already-registered target with the composite size it is
    /// now the right inscription of, without touching its buffers.
    ///
    /// Sent when the native moved but the target came out at the same
    /// numbers as before, so no encoder was rebuilt and neither
    /// registration command would otherwise be sent.  The buffers are still
    /// correct — only the compositor's record of what they were sized
    /// against is behind.  No-op when the target is not registered.
    RestampTarget {
        surface_id: u32,
        target_w: u32,
        target_h: u32,
        native_w: u32,
        native_h: u32,
    },
    /// The pid of the `xwayland-satellite` the server started.
    ///
    /// Its connection carries every X11 window in the session, so the
    /// compositor gives it one screen for all of them instead of the usual
    /// screen per window.
    SetXwaylandPid {
        pid: u32,
    },
    /// Tear down the BGRA downscale target previously registered for
    /// `(surface_id, target_w, target_h)`.  No-op when none exists.
    ClearDownscaleTarget {
        surface_id: u32,
        target_w: u32,
        target_h: u32,
    },
    /// Synthesize text input as key press/release sequences.
    TextInput {
        text: String,
    },
    /// Text the user is still composing, for the app to show inline until it
    /// is committed or withdrawn.  Delivered via `zwp_text_input_v3`
    /// preedit_string; `cursor` is a byte offset into `text`, and an empty
    /// `text` withdraws the composition.
    Preedit {
        text: String,
        cursor: u16,
    },
    /// Update the advertised output refresh rate (millihertz).
    SetRefreshRate {
        mhz: u32,
    },
    /// Set up a Vulkan Video encoder for one `(surface, client)` pair.
    SetVulkanEncoder {
        surface_id: u32,
        client_id: u64,
        codec: u8,
        qp: u8,
        width: u32,
        height: u32,
        /// Native composite dimensions this per-client target was inscribed
        /// into. The renderer uses them to reject stale-aspect targets after
        /// a resize, exactly like the server-side encoder paths.
        native_w: u32,
        native_h: u32,
        is_444: bool,
    },
    /// Retarget one client's encoder quantizer without rebuilding it.
    SetVulkanEncoderQp {
        surface_id: u32,
        client_id: u64,
        qp: u8,
    },
    /// Permit one compositor-resident encode for this client and surface.
    RequestVulkanFrame {
        surface_id: u32,
        client_id: u64,
    },
    /// Request a keyframe from one client's Vulkan Video encoder.
    RequestVulkanKeyframe {
        surface_id: u32,
        client_id: u64,
    },
    /// Destroy Vulkan Video encoders for a surface: one client's when
    /// `client_id` is `Some`, every client's when it is `None`.
    DestroyVulkanEncoder {
        surface_id: u32,
        client_id: Option<u64>,
    },
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchPhase {
    Down,
    Up,
    Motion,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchPoint {
    pub id: i32,
    pub x: f64,
    pub y: f64,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// A surface's mapped state, which is not a boolean: "never had content" and
/// "had content and lost it" differ in what the client is owed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MapState {
    /// No buffer has ever been committed.  Nothing is drawn, but the input
    /// fallback still routes to it — a toplevel that has not painted yet is
    /// the ordinary startup state, not an error.
    Never,
    /// Current content attached.
    Mapped,
    /// Had content and lost it to `attach(NULL)`.  It has already been sent a
    /// `wl_pointer.leave`, so it must not be entered again before it maps: an
    /// `enter` with no intervening map is what clients assert on.
    Unmapped,
}

/// Per-wl_surface state.  `pub(crate)` so render.rs can access fields.
pub(crate) struct Surface {
    pub surface_id: u16,
    pub wl_surface: WlSurface,

    // pending state
    /// Double-buffered `wl_surface.attach` state.  The outer `Option`
    /// distinguishes no attach in this commit from `attach(NULL)`, which
    /// unmaps the surface; collapsing both to `None` leaves stale pixels and
    /// pointer focus behind after clients unmap a popup or subsurface.
    pending_buffer: Option<Option<WlBuffer>>,
    pending_buffer_scale: i32,
    pending_damage: Vec<PendingDamage>,
    pending_frame_callbacks: Vec<WlCallback>,
    pending_presentation_feedbacks: Vec<WpPresentationFeedback>,
    pending_opaque: bool,
    /// Outer `Some` means `set_input_region` was called and is awaiting a
    /// commit; the inner `None` is the protocol's nil region.
    pending_input_region: Option<Option<Vec<RegionOp>>>,

    // committed state
    /// Whether the client has current content attached.  Tracked separately
    /// from `surface_meta`, which is only populated when an upload *succeeds*:
    /// a rejected buffer (unsupported dma-buf fourcc, failed SHM read) leaves
    /// a mapped surface with no meta, and treating that as an unmap would
    /// silently disinherit every descendant that does have usable content.
    pub map_state: MapState,
    pub buffer_scale: i32,
    pub is_opaque: bool,
    /// Where this surface accepts pointer input, in surface-local
    /// coordinates. `None` is the default: all of it.
    input_region: Option<Vec<RegionOp>>,

    // explicit sync (wp_linux_drm_syncobj_v1) — double-buffered per
    // commit, meaningful only alongside a newly attached buffer.
    pending_acquire_point: Option<crate::drm_syncobj::SyncPoint>,
    pending_release_point: Option<crate::drm_syncobj::SyncPoint>,
    /// The surface's syncobj object; its existence obliges every buffer
    /// commit to carry both sync points.
    syncobj_surface: Option<
        wayland_protocols::wp::linux_drm_syncobj::v1::server::wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1,
    >,

    // subsurface
    pub parent_surface_id: Option<ObjectId>,
    pending_subsurface_position: Option<(i32, i32)>,
    pub subsurface_position: (i32, i32),
    pub children: Vec<ObjectId>,

    // xdg
    xdg_surface: Option<XdgSurface>,
    xdg_toplevel: Option<XdgToplevel>,
    xdg_popup: Option<XdgPopup>,
    pub xdg_geometry: Option<(i32, i32, i32, i32)>,
    /// Whether the client asked for fullscreen and we granted it.  It changes
    /// nothing about how the pane is drawn -- it already fills its output --
    /// but the client is told, because a client that asks and hears otherwise
    /// backs its own fullscreen out again.
    xdg_fullscreen: bool,
    /// The size range the client says it can render, from
    /// `xdg_toplevel.set_min_size` / `set_max_size`, in window geometry
    /// coordinates.  Zero in a dimension means unset.  Double-buffered, so
    /// each pair is staged until commit -- a client is allowed to send a min
    /// and a max that only agree once both have landed.
    pending_min_size: (i32, i32),
    pending_max_size: (i32, i32),
    min_size: (i32, i32),
    max_size: (i32, i32),

    title: String,
    app_id: String,

    // viewport
    pending_viewport_destination: Option<(i32, i32)>,
    /// Committed viewport destination (logical size declared by client via
    /// `wp_viewport.set_destination`).  Used by fractional-scale-aware clients
    /// (e.g. Chromium) that render at physical resolution with `buffer_scale=1`
    /// and rely on the viewport to declare the logical surface size.
    pub viewport_destination: Option<(i32, i32)>,
    pending_viewport_source: Option<(f64, f64, f64, f64)>,
    /// Committed viewport source rectangle (`wp_viewport.set_source`), in
    /// surface-local coordinates — that is, buffer pixels divided by
    /// `buffer_scale`, after `buffer_transform`.
    ///
    /// This is the part of the buffer that is actually the window; the rest
    /// is whatever the client last drew there.  Chromium sets it on every
    /// shrink so it can keep an oversized buffer instead of reallocating
    /// one per resize step, and only clears it when it does eventually
    /// reallocate — which can be seconds later, or never while the window
    /// keeps moving.  Sampling the whole buffer into the destination in the
    /// meantime squashes the picture by exactly the ratio it cropped away.
    pub viewport_source: Option<(f64, f64, f64, f64)>,

    is_cursor: bool,
    cursor_hotspot: (i32, i32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingDamage {
    /// Coordinates are surface-local and still need buffer-scale conversion.
    Surface {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// Coordinates are already in buffer pixels.
    Buffer {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// Used for protocol state we do not model precisely (for example a
    /// legacy non-zero attach offset).  Over-copying is the safe fallback.
    Full,
}

fn clipped_shm_damage_rect(
    x: i64,
    y: i64,
    width: i64,
    height: i64,
    buffer_width: u32,
    buffer_height: u32,
) -> Option<crate::vulkan_render::ShmDamageRect> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let x0 = x.max(0).min(buffer_width as i64);
    let y0 = y.max(0).min(buffer_height as i64);
    let x1 = x.saturating_add(width).max(0).min(buffer_width as i64);
    let y1 = y.saturating_add(height).max(0).min(buffer_height as i64);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(crate::vulkan_render::ShmDamageRect {
        x: x0 as u32,
        y: y0 as u32,
        width: (x1 - x0) as u32,
        height: (y1 - y0) as u32,
    })
}

fn shm_damage_rects(
    pending: &[PendingDamage],
    buffer_width: u32,
    buffer_height: u32,
    buffer_scale: i32,
    viewport_source: Option<(f64, f64, f64, f64)>,
    viewport_destination: Option<(i32, i32)>,
) -> Vec<crate::vulkan_render::ShmDamageRect> {
    let full = || {
        vec![crate::vulkan_render::ShmDamageRect {
            x: 0,
            y: 0,
            width: buffer_width,
            height: buffer_height,
        }]
    };
    // A client is required to damage newly attached content, but copying the
    // full buffer here makes malformed/old clients safe and initializes a new
    // texture pool entry correctly.
    if pending.is_empty() {
        return full();
    }

    let scale = buffer_scale.max(1) as f64;
    let mut out = Vec::with_capacity(pending.len());
    for damage in pending {
        let rect = match *damage {
            PendingDamage::Full => return full(),
            PendingDamage::Buffer {
                x,
                y,
                width,
                height,
            } => clipped_shm_damage_rect(
                x as i64,
                y as i64,
                width as i64,
                height as i64,
                buffer_width,
                buffer_height,
            ),
            PendingDamage::Surface {
                x,
                y,
                width,
                height,
            } => {
                if width <= 0 || height <= 0 {
                    continue;
                }

                // wp_viewport's source rectangle is expressed after buffer
                // scale.  Map both damage edges through crop/scale and round
                // outwards so every touched source pixel is uploaded.
                let default_source = (
                    0.0,
                    0.0,
                    buffer_width as f64 / scale,
                    buffer_height as f64 / scale,
                );
                let (source_x, source_y, source_width, source_height) =
                    viewport_source.unwrap_or(default_source);
                let (surface_width, surface_height) = match viewport_destination {
                    Some((width, height)) if width > 0 && height > 0 => {
                        (width as f64, height as f64)
                    }
                    Some(_) => return full(),
                    None => (source_width, source_height),
                };
                if !source_x.is_finite()
                    || !source_y.is_finite()
                    || !source_width.is_finite()
                    || !source_height.is_finite()
                    || source_width <= 0.0
                    || source_height <= 0.0
                    || surface_width <= 0.0
                    || surface_height <= 0.0
                {
                    return full();
                }

                let x0 = (source_x + x as f64 * source_width / surface_width) * scale;
                let y0 = (source_y + y as f64 * source_height / surface_height) * scale;
                let x1 =
                    (source_x + (x as f64 + width as f64) * source_width / surface_width) * scale;
                let y1 = (source_y + (y as f64 + height as f64) * source_height / surface_height)
                    * scale;
                if !x0.is_finite() || !y0.is_finite() || !x1.is_finite() || !y1.is_finite() {
                    return full();
                }
                clipped_shm_damage_rect(
                    x0.floor() as i64,
                    y0.floor() as i64,
                    (x1.ceil() - x0.floor()) as i64,
                    (y1.ceil() - y0.floor()) as i64,
                    buffer_width,
                    buffer_height,
                )
            }
        };
        if let Some(rect) = rect {
            out.push(rect);
        }
    }
    out
}

struct ShmPool {
    resource: WlShmPool,
    fd: OwnedFd,
    inner: std::sync::Mutex<ShmPoolInner>,
}

struct ShmPoolInner {
    size: usize,
    mmap_ptr: *mut u8,
}

// Safety: the raw ptr is never shared outside the mutex; the fd and resource
// are Send by construction.
unsafe impl Send for ShmPoolInner {}

impl ShmPool {
    fn new(resource: WlShmPool, fd: OwnedFd, size: i32) -> Self {
        let sz = size.max(0) as usize;
        let ptr = if sz > 0 {
            unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    sz,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    fd.as_raw_fd(),
                    0,
                )
            }
        } else {
            libc::MAP_FAILED
        };
        ShmPool {
            resource,
            fd,
            inner: std::sync::Mutex::new(ShmPoolInner {
                size: sz,
                mmap_ptr: if ptr == libc::MAP_FAILED {
                    std::ptr::null_mut()
                } else {
                    ptr as *mut u8
                },
            }),
        }
    }

    fn resize(&self, new_size: i32) {
        let new_sz = new_size.max(0) as usize;
        let mut inner = self.inner.lock().unwrap();
        if new_sz <= inner.size {
            return;
        }
        if !inner.mmap_ptr.is_null() {
            unsafe {
                libc::munmap(inner.mmap_ptr as *mut _, inner.size);
            }
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                new_sz,
                libc::PROT_READ,
                libc::MAP_SHARED,
                self.fd.as_raw_fd(),
                0,
            )
        };
        inner.mmap_ptr = if ptr == libc::MAP_FAILED {
            std::ptr::null_mut()
        } else {
            ptr as *mut u8
        };
        inner.size = new_sz;
    }

    /// Run `f` with the mapped SHM region as a `&[u8]`, holding the pool
    /// mutex for the duration. Returns `None` if the mmap is invalid.
    /// Used by the zero-copy upload path so we can stream bytes straight
    /// from client-shared memory into Vulkan-mapped memory without going
    /// through an intermediate owned `Vec`.
    fn with_mmap<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        let inner = self.inner.lock().unwrap();
        if inner.mmap_ptr.is_null() {
            return None;
        }
        let slice = unsafe { std::slice::from_raw_parts(inner.mmap_ptr, inner.size) };
        Some(f(slice))
    }

    fn read_buffer(
        &self,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: wl_shm::Format,
    ) -> Option<(u32, u32, PixelData)> {
        let inner = self.inner.lock().unwrap();
        if inner.mmap_ptr.is_null() {
            return None;
        }
        // Client-controlled geometry. Reject non-positive/negative values
        // (a negative i32 cast to usize becomes a huge value) and use checked
        // arithmetic: a crafted stride/height/offset can otherwise wrap the
        // bounds check below and trigger an out-of-bounds read of the mmap.
        if offset < 0 || width <= 0 || height <= 0 || stride < 0 {
            return None;
        }
        let w = width as u32;
        let h = height as u32;
        let s = stride as usize;
        let off = offset as usize;
        let row_bytes = (w as usize).checked_mul(4)?;
        let needed = s
            .checked_mul(h as usize - 1)
            .and_then(|body| body.checked_add(off))
            .and_then(|n| n.checked_add(row_bytes))?;
        if needed > inner.size {
            return None;
        }
        let mut bgra = if s == row_bytes && off == 0 {
            let total = row_bytes * h as usize;
            unsafe { std::slice::from_raw_parts(inner.mmap_ptr, total) }.to_vec()
        } else {
            let mut packed = Vec::with_capacity(row_bytes * h as usize);
            for row in 0..h as usize {
                let src = unsafe {
                    std::slice::from_raw_parts(inner.mmap_ptr.add(off + row * s), row_bytes)
                };
                packed.extend_from_slice(src);
            }
            packed
        };
        if matches!(format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Xbgr8888) {
            for px in bgra.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
        if matches!(format, wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888) {
            Some((w, h, PixelData::Rgba(Arc::new(bgra))))
        } else {
            Some((w, h, PixelData::Bgra(Arc::new(bgra))))
        }
    }
}

impl Drop for ShmPool {
    fn drop(&mut self) {
        let inner = self.inner.get_mut().unwrap();
        if !inner.mmap_ptr.is_null() {
            unsafe {
                libc::munmap(inner.mmap_ptr as *mut _, inner.size);
            }
        }
    }
}

unsafe impl Send for ShmPool {}

struct ShmBufferData {
    /// Keep the pool alive for the lifetime of the buffer: wl_shm_pool.destroy
    /// does NOT invalidate buffers created from the pool (see the wl_shm_pool
    /// XML — "destruction does not affect wl_shm_pool.create_buffer"). Client
    /// processes such as Chromium routinely destroy the pool immediately
    /// after creating a buffer. Holding an Arc here keeps the mmap alive.
    pool: Arc<ShmPool>,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: wl_shm::Format,
}

struct DmaBufBufferData {
    width: i32,
    height: i32,
    fourcc: u32,
    modifier: u64,
    planes: Vec<DmaBufPlane>,
    y_invert: bool,
}

struct DmaBufPlane {
    fd: OwnedFd,
    offset: u32,
    stride: u32,
}

struct DmaBufParamsPending {
    resource: ZwpLinuxBufferParamsV1,
    planes: Vec<DmaBufPlane>,
    modifier: u64,
}

struct ClientState {
    /// Set by the backend's disconnect callback so the compositor can avoid
    /// probing every live resource after every ordinary request batch.
    cleanup_needed: Arc<AtomicBool>,
}

/// Accept one connection and register it, optionally stamped with the identity
/// of the socket it arrived on.
///
/// Shared by the session's own socket and every per-app socket so the two cannot
/// drift: a stamped client differs only in carrying an identity.
fn accept_client(
    state: &mut Compositor,
    client_stream: std::os::unix::net::UnixStream,
    identity: Option<Arc<AppIdentity>>,
    monitor_cancel: &std::os::unix::net::UnixStream,
) {
    let watched_stream = client_stream.try_clone().ok();
    match state.display_handle.insert_client(
        client_stream,
        Arc::new(ClientState {
            cleanup_needed: Arc::clone(&state.cleanup_needed),
        }),
    ) {
        Ok(client) => {
            // The peer's pid is what tells the X11 bridge apart
            // from an ordinary app.
            if let Ok(creds) = client.get_credentials(&state.display_handle)
                && creds.pid > 0
            {
                state.note_client_pid(client.id(), creds.pid as u32);
            }
            if let Some(identity) = identity {
                state.client_identity.insert(client.id(), identity);
            }
            // Offer the screen before the client reads its
            // registry: a toolkit that finds no output there
            // never opens a window at all.
            state.ensure_client_output(client.id());
            if let Some(watched_stream) = watched_stream {
                monitor_client_disconnect(
                    watched_stream,
                    monitor_cancel,
                    state.display_handle.backend_handle(),
                    client.id(),
                    state.verbose,
                );
            }
        }
        Err(e) if state.verbose => {
            eprintln!("[compositor] insert_client error: {e}");
        }
        Err(_) => {}
    }
}

/// Stop dispatching a client's stale request backlog once its socket closes.
///
/// wayland-backend drains every already-readable request before it reads EOF.
/// A killed client can leave thousands of commits in the socket, making its
/// dead surface keep rendering for seconds before cleanup runs.  POLLRDHUP is
/// raised as soon as the connection itself loses its writer, even while data
/// remains readable.  Shutting down our clone discards the socket backlog;
/// marking the backend client dead also stops requests it already decoded at
/// the next dispatch boundary.
///
/// This deliberately follows the socket rather than SO_PEERCRED: passing a
/// Wayland fd to another process remains valid, and does not produce RDHUP.
fn monitor_client_disconnect(
    watched_stream: std::os::unix::net::UnixStream,
    cancel: &std::os::unix::net::UnixStream,
    backend: wayland_server::backend::Handle,
    client_id: wayland_server::backend::ClientId,
    verbose: bool,
) {
    let Ok(cancel) = cancel.try_clone() else {
        return;
    };

    let spawn = std::thread::Builder::new()
        .name("wayland-client-disconnect".into())
        .spawn(move || {
            let mut fds = [
                libc::pollfd {
                    fd: watched_stream.as_raw_fd(),
                    events: libc::POLLRDHUP,
                    revents: 0,
                },
                libc::pollfd {
                    fd: cancel.as_raw_fd(),
                    events: libc::POLLRDHUP,
                    revents: 0,
                },
            ];
            loop {
                let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
                if ready < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return;
                }
                if fds[0].revents
                    & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)
                    != 0
                {
                    let _ = watched_stream.shutdown(std::net::Shutdown::Both);
                    backend.kill_client(
                        client_id,
                        wayland_server::backend::DisconnectReason::ConnectionClosed,
                    );
                    return;
                }
                if fds[1].revents != 0 {
                    // The compositor is stopping.  The monitor's cloned fd
                    // must not keep the client connected after Display drops.
                    let _ = watched_stream.shutdown(std::net::Shutdown::Both);
                    return;
                }
            }
        });
    if let Err(err) = spawn
        && verbose
    {
        eprintln!("[compositor] cannot monitor Wayland client disconnect: {err}");
    }
}

struct XdgSurfaceData {
    wl_surface_id: ObjectId,
}
struct XdgToplevelData {
    wl_surface_id: ObjectId,
}
struct XdgPopupData {
    wl_surface_id: ObjectId,
}
/// A client buffer the compositor still holds — plus the explicit-sync
/// release point to signal when the hold ends, for clients on
/// `wp_linux_drm_syncobj_v1`.
struct HeldBuffer {
    buf: WlBuffer,
    release: Option<crate::drm_syncobj::SyncPoint>,
}

/// A committed dma-buf waiting for its explicit-sync acquire point.
struct AwaitingBuffer {
    buf: WlBuffer,
    scale: i32,
    is_cursor: bool,
    acquire: crate::drm_syncobj::SyncPoint,
    release: Option<crate::drm_syncobj::SyncPoint>,
    /// When the commit parked, so a point that never signals cannot freeze
    /// the surface forever.
    parked_at: std::time::Instant,
}

/// How long a parked commit waits for its acquire point before it is
/// installed anyway.  A client that commits before submitting its GPU work
/// signals within a frame or two; past this it is wedged (context lost,
/// killed mid-commit), and freezing its surface — and starving its buffer
/// pool — is worse than showing a frame whose paint may be incomplete.
const ACQUIRE_PARK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

impl AwaitingBuffer {
    /// Give the buffer back without ever reading it.  The release point
    /// goes through the caller's fence gate rather than signalling here:
    /// a timeline wait is satisfied by *any* point at or above it, so
    /// signalling this one while an older release is still deferred would
    /// tell the client an in-use buffer is free.
    fn into_release(self) -> HeldBuffer {
        HeldBuffer {
            buf: self.buf,
            release: self.release,
        }
    }
}

struct SubsurfaceData {
    wl_surface_id: ObjectId,
    parent_surface_id: ObjectId,
}

// -- Clipboard / data device data types --

struct DataSourceData {
    mime_types: std::sync::Mutex<Vec<String>>,
    /// v3 action state is declared exactly once before `start_drag`.
    /// `None` means no request was made; `Some(NONE)` is a real mask and
    /// must not be confused with the pre-v3 implicit Copy action.
    dnd: std::sync::Mutex<DataSourceDndState>,
}

#[derive(Default)]
struct DataSourceDndState {
    actions: Option<DndAction>,
    /// A wl_data_source is single-use across selection and DnD.
    used: bool,
    /// The source was reserved by xdg_toplevel_drag_manager_v1. It may only
    /// be used by start_drag from then on, never as a selection.
    toplevel_drag: bool,
    /// The source drag reached a terminal event. xdg_toplevel_drag_v1 may be
    /// destroyed only after dnd_drop_performed or cancelled.
    ended: bool,
}

/// The xdg-toplevel-drag object Chromium creates before starting a tab drag.
///
/// Blit panes own window placement, so the attach offset has no compositor-
/// global position to update. Keeping the association is still required: it
/// validates the protocol lifecycle and prevents the carried window from
/// becoming its own drop target.
struct XdgToplevelDragData {
    source: WlDataSource,
    attached: std::sync::Mutex<Option<ObjectId>>,
}

/// Stored state for the external (browser/CLI) clipboard selection.
#[derive(Clone)]
struct ExternalClipboard {
    mime_type: String,
    data: Vec<u8>,
}

impl ExternalClipboard {
    /// Whether this selection answers to `mime_type`.
    ///
    /// Text picks up the conventional aliases: a client asking for
    /// `UTF8_STRING` wants the same bytes as one asking for `text/plain`.
    /// Anything else answers to its own type alone — handing a PNG to a
    /// client that asked for `text/plain` is worse than handing it nothing,
    /// because nothing in the protocol lets the client notice it did not
    /// get text.
    fn offers(&self, mime_type: &str) -> bool {
        if self.mime_type == mime_type {
            return true;
        }
        self.mime_type.starts_with("text/plain")
            && matches!(
                mime_type,
                "text/plain" | "text/plain;charset=utf-8" | "UTF8_STRING"
            )
    }

    /// Every MIME type to advertise for this selection, its own first.
    fn mime_types(&self) -> Vec<String> {
        let mut mimes = vec![self.mime_type.clone()];
        if self.mime_type.starts_with("text/plain") {
            for alias in ["text/plain", "text/plain;charset=utf-8", "UTF8_STRING"] {
                if self.mime_type != alias {
                    mimes.push(alias.to_string());
                }
            }
        }
        mimes
    }
}

struct PrimarySourceData {
    mime_types: std::sync::Mutex<Vec<String>>,
}
struct PrimaryOfferData {
    external: bool,
}

/// What backs a compositor-created `WlDataOffer`.
///
/// Clipboard offers pin the selection that was current when the offer was
/// announced.  Otherwise an old offer could paste a newer owner's bytes.
enum DataOfferKind {
    ClipboardExternal(ExternalClipboard),
    ClipboardSource {
        source: WlDataSource,
        mime_types: Vec<String>,
    },
    BrowserDrag,
    ClientDrag,
}

struct DataOfferData {
    kind: DataOfferKind,
    /// After a successful v3 `finish`, only `destroy` is legal.
    finished: std::sync::atomic::AtomicBool,
}

impl DataOfferData {
    fn new(kind: DataOfferKind) -> Self {
        Self {
            kind,
            finished: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// A drag started by a Wayland client (`wl_data_device.start_drag`).
///
/// The compositor owns the implicit grab: while the session is active,
/// pointer motion/button commands do not reach `wl_pointer` — they drive
/// enter/motion/leave/drop on whatever surface is under the point, exactly
/// as a physical-pointer drag would.  The `icon` surface from `start_drag`
/// is ignored entirely: nothing maps or positions it, so a client that
/// relies on seeing its drag icon gets none (the cursor stays the normal
/// pointer).  That is a visual limitation only; the transfer is unaffected.
struct ClientDragState {
    /// The drag source; `receive` on the target offer is forwarded to it,
    /// and it gets `dnd_drop_performed`/`dnd_finished`/`cancelled`.
    source: WlDataSource,
    /// Surface the drag started on.  Only diagnostic today — the origin is
    /// allowed to be destroyed mid-drag without affecting the session.
    origin: WlSurface,
    /// MIME types the source offered, advertised on every target offer.
    mimes: Vec<String>,
    /// The surface the drag is currently over, if any.
    target: Option<ClientDragTarget>,
    /// Actions from the source's `set_actions`; empty = never set.
    source_actions: DndAction,
    /// The button was released: the grab is over and pointer input flows
    /// normally again, while the session lives on until the target's
    /// `finish` (or offer destroy) completes the source.
    dropped: bool,
    /// The direct-touch contact this drag follows. `None` means the existing
    /// pointer grab owns it.
    touch_grab: Option<TouchDragGrab>,
}

/// The contact a touch-started drag follows.
///
/// Self-contained on purpose. `start_drag` cancels the whole `wl_touch`
/// sequence — the client is told to forget every contact, because something
/// else took the seat over — so `active_touches` is emptied and can no longer
/// be the routing table for the one contact that still drives the drag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TouchDragGrab {
    owner_id: u64,
    /// Browser contact identifier, as it arrives from the transport.
    browser_id: i32,
    /// Transport surface the contact went down on, for `client_drag_motion`.
    surface_id: u16,
}

struct ActiveTouch {
    wayland_id: i32,
    target: WlSurface,
    surface_id: u16,
    down_serial: u32,
}

/// The target side of a [`ClientDragState`]: one entered surface.
struct ClientDragTarget {
    device: WlDataDevice,
    offer: WlDataOffer,
    surface: WlSurface,
    /// MIME type from the target's `accept`, if it sent one.
    accepted_mime: Option<String>,
    /// Actions from the offer's `set_actions`; empty = never set.
    offer_actions: DndAction,
    /// The destination's single preferred action from `set_actions`.
    preferred_action: DndAction,
    /// The negotiated action last announced to both sides.
    action: Option<DndAction>,
    /// Distinguishes "no action event yet" from an announced NONE action.
    action_announced: bool,
    /// `drop` was sent; a following `finish` completes the transfer.
    dropped: bool,
}

/// State of a browser-initiated drag session (no client `wl_data_source`
/// exists — `wl_data_device.enter` allows a null source for
/// compositor-initiated drags).
struct DragSessionState {
    /// The device the drag was entered on, so motion/leave/drop go where
    /// the enter went.
    device: WlDataDevice,
    /// The offer handed to the target at `enter`, kept alive through the
    /// drop so the client can `receive` + `finish` it.
    offer: WlDataOffer,
    /// The DROP-time payload map; `Receive` serves from it by mime.  Empty
    /// until the drop lands.
    offers: Vec<(String, Vec<u8>)>,
    /// The planned `text/uri-list` payload from the ENTER's item plan, if
    /// the browser sent one: the staging files exist (empty) from the
    /// start, so `receive("text/uri-list")` is answered with this
    /// immediately — Chromium fetches it at `enter`, and only that
    /// completed fetch fires its page-level dragenter during hover.
    planned_uri_list: Option<Vec<u8>>,
    /// Receives issued before the drop landed.  Chromium fetches every
    /// supported mime at `enter` and delivers the fetched snapshot at
    /// `drop`, so an early `receive` must not be answered empty — it is
    /// parked here and written out the moment `drag_drop` fills `offers`.
    /// Wayland lets the source answer at any time.  If the session ends
    /// without a drop the OwnedFds just close (an empty read).
    parked: Vec<(String, OwnedFd)>,
    /// `drop` and its terminal `leave` were sent; the offer now exists only
    /// for the destination's post-drop `receive`/`finish` requests.
    dropped: bool,
    /// The client sent `finish` after the drop.
    finished: bool,
}

/// The staged bytes a browser-drag session serves for `mime`.
///
/// Beyond the exact match, `application/octet-stream` — which the browser
/// advertises on every ENTER but never stages under that name — falls back
/// to the single non-uri-list offer when there is exactly one: a dropped
/// file's own bytes.  With zero or several candidates the fallback is
/// ambiguous and answers empty.
fn browser_drag_bytes<'a>(offers: &'a [(String, Vec<u8>)], mime: &str) -> Option<&'a [u8]> {
    if let Some((_, bytes)) = offers.iter().find(|(m, _)| m == mime) {
        return Some(bytes);
    }
    if mime == "application/octet-stream" {
        let mut candidates = offers.iter().filter(|(m, _)| m != "text/uri-list");
        if let (Some((_, bytes)), None) = (candidates.next(), candidates.next()) {
            return Some(bytes);
        }
    }
    None
}

// -- Activation token data --
struct ActivationTokenData {
    serial: u32,
}

struct PositionerState {
    resource: XdgPositioner,
    geometry: PositionerGeometry,
}

// ---------------------------------------------------------------------------
// US-QWERTY character → evdev keycode mapping
// ---------------------------------------------------------------------------

/// Map an ASCII character to its evdev keycode under a US-QWERTY layout.
/// Returns `(keycode, needs_shift)`, or `None` for characters not on the
/// layout (non-ASCII, control chars other than \t/\n).
fn char_to_keycode(ch: char) -> Option<(u32, bool)> {
    const KEY_1: u32 = 2;
    const KEY_2: u32 = 3;
    const KEY_3: u32 = 4;
    const KEY_4: u32 = 5;
    const KEY_5: u32 = 6;
    const KEY_6: u32 = 7;
    const KEY_7: u32 = 8;
    const KEY_8: u32 = 9;
    const KEY_9: u32 = 10;
    const KEY_0: u32 = 11;
    const KEY_MINUS: u32 = 12;
    const KEY_EQUAL: u32 = 13;
    const KEY_TAB: u32 = 15;
    const KEY_Q: u32 = 16;
    const KEY_W: u32 = 17;
    const KEY_E: u32 = 18;
    const KEY_R: u32 = 19;
    const KEY_T: u32 = 20;
    const KEY_Y: u32 = 21;
    const KEY_U: u32 = 22;
    const KEY_I: u32 = 23;
    const KEY_O: u32 = 24;
    const KEY_P: u32 = 25;
    const KEY_LEFTBRACE: u32 = 26;
    const KEY_RIGHTBRACE: u32 = 27;
    const KEY_ENTER: u32 = 28;
    const KEY_A: u32 = 30;
    const KEY_S: u32 = 31;
    const KEY_D: u32 = 32;
    const KEY_F: u32 = 33;
    const KEY_G: u32 = 34;
    const KEY_H: u32 = 35;
    const KEY_J: u32 = 36;
    const KEY_K: u32 = 37;
    const KEY_L: u32 = 38;
    const KEY_SEMICOLON: u32 = 39;
    const KEY_APOSTROPHE: u32 = 40;
    const KEY_GRAVE: u32 = 41;
    const KEY_BACKSLASH: u32 = 43;
    const KEY_Z: u32 = 44;
    const KEY_X: u32 = 45;
    const KEY_C: u32 = 46;
    const KEY_V: u32 = 47;
    const KEY_B: u32 = 48;
    const KEY_N: u32 = 49;
    const KEY_M: u32 = 50;
    const KEY_COMMA: u32 = 51;
    const KEY_DOT: u32 = 52;
    const KEY_SLASH: u32 = 53;
    const KEY_SPACE: u32 = 57;

    fn letter_kc(ch: char) -> u32 {
        match ch {
            'a' => KEY_A,
            'b' => KEY_B,
            'c' => KEY_C,
            'd' => KEY_D,
            'e' => KEY_E,
            'f' => KEY_F,
            'g' => KEY_G,
            'h' => KEY_H,
            'i' => KEY_I,
            'j' => KEY_J,
            'k' => KEY_K,
            'l' => KEY_L,
            'm' => KEY_M,
            'n' => KEY_N,
            'o' => KEY_O,
            'p' => KEY_P,
            'q' => KEY_Q,
            'r' => KEY_R,
            's' => KEY_S,
            't' => KEY_T,
            'u' => KEY_U,
            'v' => KEY_V,
            'w' => KEY_W,
            'x' => KEY_X,
            'y' => KEY_Y,
            'z' => KEY_Z,
            _ => KEY_SPACE,
        }
    }

    let (kc, shift) = match ch {
        'a'..='z' => (letter_kc(ch), false),
        'A'..='Z' => (letter_kc(ch.to_ascii_lowercase()), true),
        '0' => (KEY_0, false),
        '1'..='9' => (KEY_1 + (ch as u32 - '1' as u32), false),
        ' ' => (KEY_SPACE, false),
        '-' => (KEY_MINUS, false),
        '=' => (KEY_EQUAL, false),
        '[' => (KEY_LEFTBRACE, false),
        ']' => (KEY_RIGHTBRACE, false),
        ';' => (KEY_SEMICOLON, false),
        '\'' => (KEY_APOSTROPHE, false),
        ',' => (KEY_COMMA, false),
        '.' => (KEY_DOT, false),
        '/' => (KEY_SLASH, false),
        '\\' => (KEY_BACKSLASH, false),
        '`' => (KEY_GRAVE, false),
        '\t' => (KEY_TAB, false),
        '\n' => (KEY_ENTER, false),
        '!' => (KEY_1, true),
        '@' => (KEY_2, true),
        '#' => (KEY_3, true),
        '$' => (KEY_4, true),
        '%' => (KEY_5, true),
        '^' => (KEY_6, true),
        '&' => (KEY_7, true),
        '*' => (KEY_8, true),
        '(' => (KEY_9, true),
        ')' => (KEY_0, true),
        '_' => (KEY_MINUS, true),
        '+' => (KEY_EQUAL, true),
        '{' => (KEY_LEFTBRACE, true),
        '}' => (KEY_RIGHTBRACE, true),
        ':' => (KEY_SEMICOLON, true),
        '"' => (KEY_APOSTROPHE, true),
        '<' => (KEY_COMMA, true),
        '>' => (KEY_DOT, true),
        '?' => (KEY_SLASH, true),
        '|' => (KEY_BACKSLASH, true),
        '~' => (KEY_GRAVE, true),
        _ => return None,
    };
    Some((kc, shift))
}

// ---------------------------------------------------------------------------
// XKB modifier state tracking
// ---------------------------------------------------------------------------

/// Bitmask values matching the `modifier_map` in us-qwerty.xkb.
const MOD_SHIFT: u32 = 1 << 0;
const MOD_LOCK: u32 = 1 << 1;
const MOD_CONTROL: u32 = 1 << 2;
const MOD_MOD1: u32 = 1 << 3; // Alt
const MOD_MOD4: u32 = 1 << 6; // Super / Meta

/// Return the XKB modifier bit for an evdev keycode, or 0 if the key is
/// not a modifier.
fn keycode_to_mod(keycode: u32) -> u32 {
    match keycode {
        42 | 54 => MOD_SHIFT,   // ShiftLeft, ShiftRight
        58 => MOD_LOCK,         // CapsLock (toggled, handled separately)
        29 | 97 => MOD_CONTROL, // ControlLeft, ControlRight
        56 | 100 => MOD_MOD1,   // AltLeft, AltRight
        125 | 126 => MOD_MOD4,  // MetaLeft, MetaRight
        _ => 0,
    }
}

/// Per-object state for a `zwp_text_input_v3` resource.
struct TextInputState {
    resource: ZwpTextInputV3,
    /// Surface most recently named by `enter`. Requests after `leave` are
    /// ignored, as required by text-input-v3, and an enabled object may only
    /// receive browser text while this is the actual keyboard focus.
    entered_surface: Option<WlSurface>,
    /// Whether text input is active, i.e. the client has sent `enable` *and*
    /// the `commit` that applies it.
    enabled: bool,
    /// The `enable`/`disable` the client has asked for but not yet committed.
    /// Both requests are double-buffered, so acting on one before its commit
    /// would hand text to an input the client has not turned on.
    pending_enabled: bool,
    /// Whether the pending enable value was explicitly changed since the
    /// last commit. A repeated enable is a fresh request even when the
    /// resulting boolean is unchanged.
    pending_enabled_changed: bool,
    /// A committed enable is the app asking for an input panel. Kept apart
    /// from `pending_enabled_changed` because disable is state, not a show
    /// request.
    pending_show_requested: bool,
    content_hint: u32,
    content_purpose: u32,
    pending_content_hint: u32,
    pending_content_purpose: u32,
    pending_content_type_changed: bool,
    /// Where the app draws the text being edited, in surface-local logical
    /// coordinates: `(x, y, width, height)`.  This is what an input method
    /// anchors its candidate window to, and the browser needs it to put the
    /// host IME's popup over the same spot.  `None` until the app sends one.
    cursor_rect: Option<(i32, i32, i32, i32)>,
    /// The rectangle named since the last commit.  Double-buffered like the
    /// rest of the object's state, and reset by `enable`/`disable`.
    pending_cursor_rect: Option<(i32, i32, i32, i32)>,
    /// Whether the app is currently drawing a preedit we put there.  Every
    /// `done` resets the preedit, so this tracks who owes a clearing one:
    /// a composition committed as synthesised keys sends no `done` of its
    /// own, and its preedit would otherwise stay on screen forever.
    preedit_shown: bool,
    /// How many `commit` requests this object has issued.  The spec defines
    /// the `done` serial as exactly this count, per object -- a client that
    /// gets any other number applies the text without adopting the state.
    commits: u32,
}

/// Main compositor state.
struct Compositor {
    display_handle: DisplayHandle,
    /// Client disconnects are rare; resource liveness scans are not cheap.
    /// The matching `ClientState` callback raises this edge-triggered flag.
    cleanup_needed: Arc<AtomicBool>,
    surfaces: FxHashMap<ObjectId, Surface>,
    /// Rectangles accumulated per live `wl_region`, until a surface takes a
    /// copy via `set_input_region`. The resource is kept so entries from a
    /// client that disconnected without destroying its regions can be
    /// reclaimed in `cleanup_dead_surfaces`.
    regions: FxHashMap<ObjectId, (WlRegion, Vec<RegionOp>)>,
    toplevel_surface_ids: FxHashMap<u16, ObjectId>,
    /// xdg-foreign-v2 export handles shared read-only with the server. The
    /// D-Bus bridge never interprets these opaque random strings.
    foreign_exports: Arc<RwLock<HashMap<String, u16>>>,
    foreign_export_objects: FxHashMap<ObjectId, (String, u16)>,
    screencast_surfaces: FxHashSet<u16>,
    /// Per-toplevel timestamp (`elapsed_ms`) of the last server-driven
    /// `RequestFrame`. Lets `handle_surface_commit` tell whether the server
    /// is actively pacing this surface (a viewer is connected): while it is,
    /// the eager per-commit frame-callback fire is suppressed so it doesn't
    /// drive a nested compositor (e.g. weston) into an unthrottled repaint
    /// loop that overruns the server's display-rate pacing. See the fallback
    /// note in `handle_surface_commit`.
    last_request_frame_ms: FxHashMap<u16, u32>,
    /// Per-surface timestamp (`elapsed_ms`) of the last fallback frame
    /// callback fired for a surface with no live toplevel.  Nothing
    /// composites such a surface, so the server's RequestFrame pacing never
    /// reaches it and firing per commit would let a client that repaints
    /// per callback free-run at full speed on a window nobody can see.
    last_topless_frame_ms: FxHashMap<ObjectId, u32>,
    /// A fixed-clock deadline that found no frame callback ready yet.
    ///
    /// `wl_surface.frame` takes effect with the client's next commit. At high
    /// refresh rates that request and the server clock can cross by a few
    /// microseconds: dropping the empty clock tick then makes the freshly
    /// committed callback wait an entire refresh period. Keep only the newest
    /// such deadline per toplevel and consume it after that commit instead.
    pending_request_frames: FxHashMap<u16, std::time::Instant>,
    /// Toplevels known to contain at least one frame callback or presentation
    /// feedback. Fixed clocks consult this before walking a surface tree.
    frame_callback_toplevels: FxHashSet<u16>,
    next_surface_id: u16,
    shm_pools: FxHashMap<ObjectId, Arc<ShmPool>>,
    /// Per-surface metadata (dimensions, scale, flags) populated at commit time.
    /// Replaces the old pixel_cache — pixel data now lives as persistent GPU
    /// textures inside VulkanRenderer.
    surface_meta: FxHashMap<ObjectId, super::render::SurfaceMeta>,
    dmabuf_params: FxHashMap<ObjectId, DmaBufParamsPending>,
    vulkan_renderer: Option<super::vulkan_render::VulkanRenderer>,
    /// Size handed to a toplevel that has never been sized by a viewer.
    output_width: i32,
    output_height: i32,
    /// Advertised refresh rate in millihertz.  Derived from the highest
    /// `display_fps` among connected browser clients.
    output_refresh_mhz: u32,
    /// Per-surface scale in 1/120th units (wp_fractional_scale_v1
    /// convention).  120 = 1×, 180 = 1.5×, 240 = 2×.  Derived from the
    /// devicePixelRatio of the viewers watching *that* surface.
    ///
    /// Density is a property of who is looking at a window, and in blit two
    /// windows are routinely looked at by different people on different
    /// screens.  One scale shared by every surface meant a viewer opening a
    /// pane on a HiDPI laptop resized every unrelated app in the session,
    /// twice per focus change, and composited them all at a density nobody
    /// was going to display.
    surface_scales: FxHashMap<u16, u16>,
    /// Every bound output, with the screen it belongs to.
    outputs: Vec<SurfaceOutput>,
    /// Every published `wl_output` global, keyed by slot.  A slot is a
    /// screen offered to one client; it holds a toplevel once the client
    /// puts a window on it.
    output_slots: FxHashMap<u32, OutputSlot>,
    /// Output globals withdrawn while their owner is still connected.
    ///
    /// `wl_registry.global_remove` and a client's already-queued `bind` travel
    /// in opposite directions, so immediately freeing the global can turn an
    /// ordinary hot-unplug race into a fatal protocol error.  Disabled globals
    /// still accept those stale binds and are freed once their owner is gone.
    retired_output_globals: Vec<RetiredOutputGlobal>,
    /// Source of slot ids.  Never reused, so a stale `SurfaceOutput` can
    /// never be mistaken for a live screen.
    next_output_slot: u32,
    /// The `xwayland-satellite` the server started, when it did.
    ///
    /// Every X11 window in the session arrives on this one client's
    /// connection, so it is the one client that must *not* be given a screen
    /// per window: the bridge turns each `wl_output` into an X monitor, and
    /// a monitor per window is not a desktop any X client can reason about.
    xwayland_pid: Option<u32>,
    /// Peer pid of every connected client, so a bridge that connects before
    /// the server has told us its pid is still recognised afterwards.
    client_pids: FxHashMap<ClientId, u32>,
    /// Clients resolved to belong to the bridge's process tree.
    xwayland_clients: FxHashSet<ClientId>,
    /// Identity of clients that arrived on a stamped per-app socket. Absent for
    /// the shared socket, where nothing can be said about who connected.
    client_identity: FxHashMap<ClientId, Arc<AppIdentity>>,
    seats: Vec<WlSeat>,
    keyboards: Vec<WlKeyboard>,
    pointers: Vec<WlPointer>,
    touches: Vec<WlTouch>,
    touch_enabled: bool,
    /// Bounded wall-clock playout for direct touch. Chromium ignores the
    /// protocol timestamp, so actual delivery cadence carries velocity.
    touch_pacer: TouchPacer,
    /// `(our clock, the client's)` at the first event of the live sequence, so
    /// later events keep the browser's spacing in our millisecond domain.
    input_time_anchor: Option<(u32, u32)>,
    /// Direct touch has its own per-owner anchor. Several browser viewers share
    /// this seat but their DOM timestamps have unrelated page epochs; letting a
    /// desktop viewer's pointer event anchor an iPad sequence collapses every
    /// touch move to compositor drain time.
    touch_time_anchor: Option<(u64, u32, u32)>,
    /// Local arrival time of the last timestamped direct-touch event, used to
    /// re-anchor after a pause without consulting another viewer's input.
    touch_time_last_arrival: Option<u32>,
    /// Last input timestamp emitted, to keep the seat monotonic.
    last_input_time: Option<u32>,
    active_touches: HashMap<(u64, i32), ActiveTouch>,
    keyboard_keymap_data: Vec<u8>,
    /// Currently depressed (held down) XKB modifier mask.
    mods_depressed: u32,
    /// CapsLock locked modifier mask (toggled on/off by CapsLock key).
    mods_locked: u32,
    serial: u32,
    event_tx: mpsc::Sender<CompositorEvent>,
    event_notify: Arc<dyn Fn() + Send + Sync>,
    loop_signal: LoopSignal,
    /// Pending per-(surface, target) commit data, keyed by `(sid,
    /// width, height)`.  Each render of one surface can produce
    /// several frames — one per registered per-client encoder target
    /// size — and each lands here as its own entry so the server sees
    /// one `SurfaceCommit` per target.  Value is `(log_w, log_h, pixels,
    /// encoder_skip)` where the logicals are derived from the per-target
    /// physical size.
    #[allow(clippy::type_complexity)]
    pending_commits: HashMap<(u16, u32, u32), (u32, u32, PixelData, bool)>,
    /// Bitstreams from compositor-resident encoders awaiting the next
    /// flush.  Kept apart from `pending_commits` because these are owned
    /// by one client each and must never be coalesced by target size.
    pending_encoded: Vec<EncodedFrame>,
    /// Latest composited (native) size per surface, used to gate
    /// `SurfaceResized` events.  The renderer emits one frame per
    /// per-client encoder target (downscaled), but `SurfaceResized`
    /// must reflect the compositor's native output so pointer
    /// coordinate mapping stays consistent regardless of how many
    /// clients are subscribed at what sizes.
    pending_native_sizes: FxHashMap<u16, (u32, u32, u32, u32)>,
    /// Toplevels that need a re-composite the next time the GPU
    /// pipeline is idle.  Populated when a per-client encoder target
    /// is installed (`SetExternalOutputBuffers` /
    /// `RegisterDownscaleTarget`) — without a follow-up render the
    /// new target buffer never gets pixels and the per-client encoder
    /// skips forever.  An immediate `render_tree_sized` from the
    /// command handler would early-return whenever the previous
    /// commit's GPU submit hasn't completed yet, so the work is
    /// deferred here and drained in the main loop after
    /// `try_retire_pending` clears `pending_submit`.
    /// Toplevels awaiting a deferred recomposite, and whether the request
    /// was for the encoders only.  An encoder-only recomposite re-runs the
    /// GPU pipeline over unchanged content — the pixels are identical to
    /// what the server already has, so publishing them again would only
    /// burn a generation and make every other viewer re-encode the frame it
    /// is already showing.  A content request (`false`) wins over an
    /// encoder-only one for the same toplevel.
    pending_recomposite_toplevels: FxHashMap<u16, bool>,
    /// Surfaces whose `wl_buffer` is still held because the commit that
    /// applied it deferred compositing rather than running it.
    ///
    /// A DMA-BUF is imported rather than copied, so the hold is the only
    /// thing keeping the client off pixels the compositor has yet to read.
    /// Keyed by the toplevel whose queued recomposite will read them, so the
    /// drain releases exactly the buffers that composite consumed — releasing
    /// another toplevel's would just move the race.
    deferred_buffer_holds: FxHashMap<u16, FxHashSet<ObjectId>>,
    focused_surface_id: u16,
    /// The wl_surface ObjectId the pointer is currently over (None = none).
    pointer_entered_id: Option<ObjectId>,
    /// Where the cursor last was inside `pointer_entered_id`, in surface-local
    /// coordinates, so a pointer created later can be entered there.
    pointer_entered_local: (f64, f64),
    /// The surface the client last passed to `wl_pointer.set_cursor`.  A
    /// toolkit may keep a pool of cursor surfaces and retire one it is not
    /// showing, so `is_cursor` alone (which is set once and never cleared) does
    /// not identify the cursor whose content is actually on screen.
    current_cursor_surface: Option<ObjectId>,
    /// Last browser-frame position seen for each toplevel. Axis messages name
    /// their destination but carry no coordinates, so this lets a scroll
    /// re-establish the named target after another surface stole the shared
    /// pointer focus.
    pointer_frame_positions: FxHashMap<u16, (f64, f64)>,
    /// Set after output scale change; triggers keyboard leave/re-enter
    /// on the next surface commit so clients have time to process the
    /// reconfigure before receiving new input events.
    pending_kb_reenter: bool,

    gpu_device: String,
    verbose: bool,
    shutdown: Arc<AtomicBool>,
    /// Track last reported size per toplevel surface_id to detect changes.
    /// Per-toplevel: (composited_w, composited_h, logical_w, logical_h).
    /// Used for pointer coordinate mapping (browser→Wayland).
    last_reported_size: FxHashMap<u16, (u32, u32, u32, u32)>,
    /// Per-toplevel configured size.  Each surface can live in a
    /// differently-sized BSP pane, so we need to track sizes individually
    /// rather than relying on the single `output_width`/`output_height`.
    surface_sizes: FxHashMap<u16, (i32, i32)>,
    /// Pending positioner geometry, keyed by XdgPositioner protocol id.
    positioners: FxHashMap<ObjectId, PositionerState>,
    /// Active wp_fractional_scale_v1 objects, each with the surface it was
    /// created for.  The protocol is per-surface and always was; keeping the
    /// association is what lets two windows be told two different scales.
    fractional_scales: Vec<SurfaceFractionalScale>,

    // -- Clipboard --
    /// Active wl_data_device objects (one per seat binding).
    data_devices: Vec<WlDataDevice>,
    /// The wl_data_source that currently owns the clipboard selection (if any).
    /// Cleared when the source is destroyed or replaced.
    selection_source: Option<WlDataSource>,
    /// External clipboard data offered from the browser or CLI.
    external_clipboard: Option<ExternalClipboard>,
    /// Browser-initiated drag session in flight, if any.
    drag: Option<DragSessionState>,
    /// Client-initiated drag session in flight, if any.  While one is
    /// active and not yet dropped, pointer input is the drag grab.
    client_drag: Option<ClientDragState>,
    /// Live xdg-toplevel-drag objects. Chromium uses the global's presence to
    /// start a tab drag before the pointer crosses into another Blit pane.
    toplevel_drags: Vec<XdgToplevelDragV1>,

    // -- Primary selection --
    primary_devices: Vec<ZwpPrimarySelectionDeviceV1>,
    primary_source: Option<ZwpPrimarySelectionSourceV1>,
    external_primary: Option<ExternalClipboard>,

    // -- Relative pointer --
    relative_pointers: Vec<ZwpRelativePointerV1>,

    /// Per-client multiplier for the smooth `wl_pointer.axis` value, keyed
    /// by client so the `/proc` lookup behind it happens once rather than
    /// once per scroll frame.  See [`Compositor::smooth_axis_scale`].
    axis_scale: HashMap<wayland_server::backend::ClientId, f64>,

    // -- Text input --
    /// Active zwp_text_input_v3 objects.  When the compositor receives
    /// composed text from the browser it delivers it via `commit_string`
    /// + `done` to the text_input object belonging to the focused surface.
    text_inputs: Vec<TextInputState>,

    // -- Activation --
    next_activation_token: u32,

    // -- Popup grab --
    /// Stack of grabbed xdg_popup surfaces (outermost first).  When the
    /// pointer clicks outside the topmost grabbed popup we send
    /// `xdg_popup.popup_done` to dismiss the popup chain.
    popup_grab_stack: Vec<ObjectId>,
    /// A button whose press dismissed a popup grab, and whose release must
    /// therefore be swallowed too.  Delivering the release alone would hand
    /// a client a button it never saw pressed.
    popup_dismiss_button: Option<u32>,
    /// The popup holding keyboard focus, when one does.
    ///
    /// Keyboard focus is otherwise a `u16` toplevel id resolved through
    /// `toplevel_surface_ids`, and a popup is in neither — it has no surface
    /// id at all.  So a grabbing menu could never be told it had focus, and
    /// GTK gates menu keynav on exactly that: Escape and the arrow keys went
    /// to the page behind the menu instead.  This overrides the toplevel for
    /// as long as the grab lasts; `keyboard_focus_wl` is the single answer to
    /// "who holds it".
    kb_focus_popup: Option<ObjectId>,

    // -- DMA-BUF buffer hold --
    /// Buffers whose DMA-BUF content could not be eagerly snapshotted to
    /// CPU memory (e.g. tiled VRAM that cannot be mmap-read linearly, or
    /// fence not ready).  We hold the `WlBuffer` alive so the client
    /// cannot reuse it while the GPU texture still references the fd.
    /// Released when the surface commits a new buffer or is destroyed.
    held_buffers: FxHashMap<ObjectId, HeldBuffer>,
    /// Explicit-sync device, when the render node supports timeline
    /// syncobjs; gates the `wp_linux_drm_syncobj_v1` global.
    syncobj_device: Option<std::sync::Arc<crate::drm_syncobj::DrmSyncobjDevice>>,
    /// Imported client timelines by protocol object id.  Pending sync
    /// points hold `Arc` clones, so dropping an entry never invalidates
    /// a point already attached to a commit.
    syncobj_timelines: FxHashMap<ObjectId, std::sync::Arc<crate::drm_syncobj::SyncobjTimeline>>,
    /// Committed buffers whose acquire point has not signalled yet.  The
    /// previous buffer stays current until promotion; a newer commit
    /// discards the waiting one unread (signalling its release point).
    awaiting_acquire: FxHashMap<ObjectId, AwaitingBuffer>,

    // -- Cursor pixel cache --
    /// CPU-accessible RGBA pixels for cursor surfaces.  Cursors aren't
    /// GPU-composited — they're sent as cursor image events.  Updated
    /// at cursor surface commit time.
    cursor_rgba: FxHashMap<ObjectId, (u32, u32, Vec<u8>)>,
    /// The last cursor announced per target surface, so artwork a viewer is
    /// already drawing is not sent again on every cursor-surface commit.
    last_cursor: FxHashMap<u16, CursorImage>,
}

/// Scan for a free surface id starting at `from`, wrapping past `u16::MAX`
/// and skipping 0 (reserved as "no surface"). `None` when every non-zero id
/// is taken.
///
/// Split out of `Compositor::allocate_surface_id` so exhaustion is testable
/// without standing up a compositor — it is otherwise 65535 live toplevels
/// away and was silently wrong.
fn scan_free_surface_id(from: u16, taken: impl Fn(u16) -> bool) -> Option<u16> {
    // Normalising `from` also closes a latent hang: the old loop compared
    // against a `start` of 0 that the skip-zero step could never produce
    // again, so a 0 seed would have spun forever.
    let start = if from == 0 { 1 } else { from };
    let mut id = start;
    loop {
        if !taken(id) {
            return Some(id);
        }
        id = next_surface_id_after(id);
        if id == start {
            return None;
        }
    }
}

/// The id to try after `id`, wrapping past `u16::MAX` back to 1.
fn next_surface_id_after(id: u16) -> u16 {
    if id == u16::MAX { 1 } else { id + 1 }
}

/// Resolve the native size to publish after one render attempt.
///
/// A browser-requested composite size is authoritative even if this attempt
/// could not submit. Otherwise only the renderer's explicit submission
/// metadata is usable; published frames may all be per-client downscales.
fn native_size_after_render(
    toplevel_sid: u16,
    configured: Option<(u32, u32, u32, u32)>,
    submitted: Option<(u16, u32, u32)>,
    scale_120: u32,
) -> Option<(u16, u32, u32, u32, u32)> {
    if let Some((w, h, log_w, log_h)) = configured {
        return Some((toplevel_sid, w, h, log_w, log_h));
    }
    submitted.map(|(sid, w, h)| {
        (
            sid,
            w,
            h,
            (w * 120).div_ceil(scale_120),
            (h * 120).div_ceil(scale_120),
        )
    })
}

impl Compositor {
    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1);
        self.serial
    }

    /// Update internal modifier state from a key event and send
    /// `wl_keyboard.modifiers` to all keyboards belonging to the focused
    /// surface's client.  Many Wayland clients (GTK, Chromium) rely on this
    /// event rather than tracking modifiers from raw key events.
    fn update_and_send_modifiers(&mut self, keycode: u32, pressed: bool) {
        let m = keycode_to_mod(keycode);
        if m == 0 {
            return;
        }
        if keycode == 58 {
            // CapsLock toggles mods_locked on press.
            if pressed {
                self.mods_locked ^= MOD_LOCK;
            }
        } else if pressed {
            self.mods_depressed |= m;
        } else {
            self.mods_depressed &= !m;
        }
        let serial = self.next_serial();
        let focused_wl = self
            .toplevel_surface_ids
            .get(&self.focused_surface_id)
            .and_then(|root_id| self.surfaces.get(root_id))
            .map(|s| s.wl_surface.clone());
        for kb in &self.keyboards {
            if let Some(ref wl) = focused_wl
                && same_client(kb, wl)
            {
                kb.modifiers(serial, self.mods_depressed, 0, self.mods_locked, 0);
            }
        }
    }

    fn emit_surface_text_input(
        &self,
        surface_id: u16,
        enabled: bool,
        requested: bool,
        hint: u32,
        purpose: u32,
        cursor_rect: Option<(i32, i32, i32, i32)>,
    ) {
        let cursor_rect = cursor_rect.map(|rect| self.cursor_rect_to_composited(surface_id, rect));
        let _ = self.event_tx.send(CompositorEvent::SurfaceTextInput {
            surface_id,
            enabled,
            requested,
            hint,
            purpose,
            cursor_rect,
        });
        (self.event_notify)();
    }

    /// Map a surface-local logical cursor rectangle into the composited
    /// frame's physical pixels — the space the browser lays its canvas out
    /// in.  This is exactly the inverse of `dispatch_pointer_motion`'s
    /// conversion, xdg_geometry crop included.
    ///
    /// The rectangle is relative to the surface the text input entered,
    /// which this takes to be the toplevel root: the toolkits we drive put
    /// their text input on the toplevel, and being wrong about a subsurface
    /// only misplaces the popup by that subsurface's offset.
    fn cursor_rect_to_composited(
        &self,
        surface_id: u16,
        (x, y, w, h): (i32, i32, i32, i32),
    ) -> (i32, i32, i32, i32) {
        let (mut x, mut y) = (x as f64, y as f64);
        if let Some((gx, gy, _, _)) = self
            .toplevel_surface_ids
            .get(&surface_id)
            .and_then(|rid| self.surfaces.get(rid))
            .and_then(|s| s.xdg_geometry)
        {
            x -= gx as f64;
            y -= gy as f64;
        }
        let (sx, sy) = match self.last_reported_size.get(&surface_id) {
            Some(&(cw, ch, lw, lh)) if lw > 0 && lh > 0 => {
                (cw as f64 / lw as f64, ch as f64 / lh as f64)
            }
            _ => (1.0, 1.0),
        };
        (
            (x * sx).round() as i32,
            (y * sy).round() as i32,
            (w as f64 * sx).round() as i32,
            (h as f64 * sy).round() as i32,
        )
    }

    fn text_input_has_focus(ti: &TextInputState, focused_wl: &WlSurface) -> bool {
        ti.enabled
            && ti
                .entered_surface
                .as_ref()
                .is_some_and(|entered| entered.id() == focused_wl.id())
    }

    /// Hand `composed` to the focused client's input method and clear it.
    ///
    /// Only the characters the keymap cannot express come through here, so a
    /// client with no enabled input method is no worse off than before: they
    /// were dropped then and they are dropped now.  What changes is that a
    /// client which asked for text finally gets it.
    fn flush_composed(&mut self, focused_wl: &WlSurface, composed: &mut String) {
        if composed.is_empty() {
            return;
        }
        let text = std::mem::take(composed);
        for ti in &mut self.text_inputs {
            if !Self::text_input_has_focus(ti, focused_wl) {
                continue;
            }
            ti.resource.commit_string(Some(text.clone()));
            // Nothing is inserted until `done` — commit_string only sets
            // pending state.  That same `done` also resets the preedit, so
            // the composition this text came from clears itself.
            ti.resource.done(ti.commits);
            ti.preedit_shown = false;
        }
    }

    /// Show `text` as the composition in progress, or withdraw it when empty.
    ///
    /// A preedit is only meaningful inside the app's own text field, so a
    /// client with no input method enabled is not sent one — its composed
    /// text still arrives on commit, as keys or as `commit_string`.
    fn send_preedit(&mut self, focused_wl: &WlSurface, text: &str, cursor: u16) {
        let cursor = i32::from(cursor.min(text.len().min(i32::MAX as usize) as u16));
        for ti in &mut self.text_inputs {
            if !Self::text_input_has_focus(ti, focused_wl) {
                continue;
            }
            // Withdrawing a preedit nobody is showing is a `done` that only
            // costs the client a state reset it did not need.
            if text.is_empty() && !ti.preedit_shown {
                continue;
            }
            ti.resource.preedit_string(
                (!text.is_empty()).then(|| text.to_string()),
                cursor,
                cursor,
            );
            ti.resource.done(ti.commits);
            ti.preedit_shown = !text.is_empty();
        }
    }

    /// Take back a preedit that nothing else has cleared.
    ///
    /// Committing through the synthesised-key path sends no `done`, so
    /// without this the pending composition stays drawn under text the app
    /// has already inserted.
    fn clear_stale_preedit(&mut self, focused_wl: &WlSurface) {
        for ti in &mut self.text_inputs {
            if !ti.preedit_shown || !same_client(&ti.resource, focused_wl) {
                continue;
            }
            ti.resource.done(ti.commits);
            ti.preedit_shown = false;
        }
    }

    /// The surface that currently holds `wl_keyboard.enter`.
    ///
    /// A grabbing popup outranks the focused toplevel: while a menu is up it
    /// is the thing the keyboard is talking to.
    fn keyboard_focus_wl(&self) -> Option<WlSurface> {
        if let Some(ref popup_id) = self.kb_focus_popup
            && let Some(surf) = self.surfaces.get(popup_id)
        {
            return Some(surf.wl_surface.clone());
        }
        self.toplevel_surface_ids
            .get(&self.focused_surface_id)
            .and_then(|root| self.surfaces.get(root))
            .map(|s| s.wl_surface.clone())
    }

    /// `wl_keyboard.leave` (and the text-input equivalent) for `wl`.
    fn send_keyboard_leave(&mut self, wl: &WlSurface) {
        let serial = self.next_serial();
        let text_input_surface_id = self.find_toplevel_root(&wl.id()).1;
        let mut text_input_was_enabled = false;
        for kb in &self.keyboards {
            if same_client(kb, wl) {
                kb.leave(serial, wl);
            }
        }
        for ti in &mut self.text_inputs {
            if same_client(&ti.resource, wl) {
                ti.resource.leave(wl);
                text_input_was_enabled |= ti.enabled
                    && ti
                        .entered_surface
                        .as_ref()
                        .is_some_and(|entered| entered.id() == wl.id());
                ti.entered_surface = None;
                ti.enabled = false;
                ti.pending_enabled = false;
                ti.pending_enabled_changed = false;
                ti.pending_show_requested = false;
                ti.content_hint = 0;
                ti.content_purpose = 0;
                ti.pending_content_hint = 0;
                ti.pending_content_purpose = 0;
                ti.pending_content_type_changed = false;
                ti.cursor_rect = None;
                ti.pending_cursor_rect = None;
                // "The client should reset any preedit string previously
                // set" — so whatever we last drew is already gone.
                ti.preedit_shown = false;
            }
        }
        if text_input_was_enabled && let Some(surface_id) = text_input_surface_id {
            self.emit_surface_text_input(surface_id, false, false, 0, 0, None);
        }
    }

    /// `wl_keyboard.enter` (and the text-input equivalent) for `wl`.
    fn send_keyboard_enter(&mut self, wl: &WlSurface) {
        // Both selections are delivered "immediately before receiving
        // keyboard focus", which is the only moment the protocol names —
        // so they go out ahead of the `enter` below, not at bind time.
        self.offer_selections_to_client(wl);
        let serial = self.next_serial();
        // A client's modifier state starts empty and only ever moves on a
        // `modifiers` event, which `update_and_send_modifiers` sends solely as
        // a side effect of a modifier key transition — and it sends it to
        // whoever held focus at the time.  So focus landing here while Ctrl is
        // held leaves this client believing nothing is down, and it reads the
        // next keystroke unmodified.  State the seat-wide fact it has no other
        // way to learn, the way the pointer's bind-time `enter` replay does.
        let mods_serial = self.next_serial();
        for kb in &self.keyboards {
            if same_client(kb, wl) {
                kb.enter(serial, wl, vec![]);
                kb.modifiers(mods_serial, self.mods_depressed, 0, self.mods_locked, 0);
            }
        }
        for ti in &mut self.text_inputs {
            if same_client(&ti.resource, wl) {
                // An enable belongs to the surface named by the preceding
                // enter. Never carry an old surface's text field across a
                // focus transition, even when both surfaces share a client.
                ti.entered_surface = Some(wl.clone());
                ti.enabled = false;
                ti.pending_enabled = false;
                ti.pending_enabled_changed = false;
                ti.pending_show_requested = false;
                ti.content_hint = 0;
                ti.content_purpose = 0;
                ti.pending_content_hint = 0;
                ti.pending_content_purpose = 0;
                ti.pending_content_type_changed = false;
                ti.resource.enter(wl);
            }
        }
    }

    /// Hand keyboard focus to a popup that has taken a grab.
    ///
    /// The menu, not the page behind it, is what the arrow keys and Escape
    /// are meant for. Idempotent: a client that re-grabs an already-focused
    /// popup must not be sent a second `enter` with no `leave` between —
    /// the hazard `set_keyboard_focus` documents at length.
    fn focus_popup(&mut self, popup_id: &ObjectId) {
        if self.kb_focus_popup.as_ref() == Some(popup_id) {
            return;
        }
        let Some(popup_wl) = self.surfaces.get(popup_id).map(|s| s.wl_surface.clone()) else {
            return;
        };
        if let Some(previous) = self.keyboard_focus_wl() {
            self.send_keyboard_leave(&previous);
        }
        self.kb_focus_popup = Some(popup_id.clone());
        self.send_keyboard_enter(&popup_wl);
        let _ = self.display_handle.flush_clients();
    }

    /// Give keyboard focus back after `popup_id` goes away.
    ///
    /// Back to the popup still grabbing underneath it, if the chain is nested
    /// — closing a submenu returns to its parent menu, not past both — and to
    /// the focused toplevel otherwise. A no-op unless that popup actually
    /// held focus, so dismissing an unfocused chain disturbs nothing.
    fn unfocus_popup(&mut self, popup_id: &ObjectId) {
        // The caller has already taken this popup off the stack, so what
        // remains is what is still grabbing beneath it.
        let Some(next_holder) = keyboard_focus_after_popup_close(
            popup_id,
            self.kb_focus_popup.as_ref(),
            &self.popup_grab_stack,
        ) else {
            return;
        };
        if let Some(going) = self.surfaces.get(popup_id).map(|s| s.wl_surface.clone()) {
            self.send_keyboard_leave(&going);
        }
        self.kb_focus_popup = next_holder;
        if let Some(next) = self.keyboard_focus_wl() {
            self.send_keyboard_enter(&next);
        }
        let _ = self.display_handle.flush_clients();
    }

    /// Switch keyboard (and text_input) focus from the current surface to
    /// `new_surface_id`.  Sends `wl_keyboard.leave` to the old surface's
    /// client and `wl_keyboard.enter` to the new surface's client, which is
    /// required by the Wayland protocol when focus changes between clients.
    fn set_keyboard_focus(&mut self, new_surface_id: u16) {
        let old_id = self.focused_surface_id;
        if old_id == new_surface_id {
            // Focus unchanged: the surface already holds keyboard focus and
            // received its `wl_keyboard.enter` when focus first moved here
            // (via the change path below). Do NOT re-send it. A second
            // `enter` with no intervening `leave` violates the protocol and
            // crashes a nested compositor whose focus is already set — e.g.
            // weston's wayland backend takes its "this shouldn't happen"
            // path and calls `frame_status(output->frame)` unconditionally;
            // a fullscreen output has no decoration frame, so that is a NULL
            // deref (SIGSEGV). The browser resends `C2S_SURFACE_FOCUS` for
            // the already-focused surface on every click/select, so this is
            // hit constantly.
            //
            // Surface-id reuse after a focused surface is destroyed used to
            // rely on this branch to deliver the first `enter` to the new
            // owner; that is now handled by resetting `focused_surface_id` to
            // 0 on destroy, so the reused id arrives here as a real change.
            return;
        }

        // Leave whoever actually holds focus, which is a grabbing popup if
        // there is one — sending the leave to the toplevel instead would
        // leave the menu believing it still had the keyboard, and the
        // toplevel receiving a leave it was never given.
        if let Some(old_wl) = self.keyboard_focus_wl()
            && (old_id != 0 || self.kb_focus_popup.is_some())
        {
            self.send_keyboard_leave(&old_wl);
        }
        // Focus moving to a toplevel outranks any open menu's grab.
        self.kb_focus_popup = None;

        self.focused_surface_id = new_surface_id;

        // Enter the new surface.
        if let Some(root_id) = self.toplevel_surface_ids.get(&new_surface_id)
            && let Some(wl_surface) = self.surfaces.get(root_id).map(|s| s.wl_surface.clone())
        {
            self.send_keyboard_enter(&wl_surface);
        }
    }

    /// Allocate a wire-visible id for a new toplevel, or `None` when every
    /// non-zero `u16` is taken.
    ///
    /// The scan used to `break` on exhaustion the same way it breaks on
    /// success, so it returned the occupied id it started from. Two live
    /// toplevels then shared one: the `toplevel_surface_ids` insert dropped
    /// the older mapping, encoder and size state keyed by the id aliased,
    /// and destroying either surface unregistered both.
    fn allocate_surface_id(&mut self) -> Option<u16> {
        let taken = &self.toplevel_surface_ids;
        let id = scan_free_surface_id(self.next_surface_id, |c| taken.contains_key(&c))?;
        self.next_surface_id = next_surface_id_after(id);
        Some(id)
    }

    fn flush_pending_commits(&mut self) {
        // First emit at most one SurfaceResized per surface, derived
        // from the compositor's NATIVE composite size (not any
        // per-client downscaled target).  The server's pointer
        // coordinate mapping depends on the native size staying
        // consistent regardless of how many viewers are subscribed at
        // what sizes.
        for (surface_id, (width, height, log_w, log_h)) in self.pending_native_sizes.drain() {
            let prev = self.last_reported_size.get(&surface_id).copied();
            // Record unconditionally. The entry is not only the size the
            // client was told; its logical half is the denominator the
            // pointer path divides browser coordinates by
            // (`PointerMotion`). Logical size can change while the physical
            // size does not — an output scale change alone does exactly that
            // — so gating the *store* on the physical size left the ratio
            // stale and scaled every later coordinate by the wrong factor.
            // Far enough off and the hit test lands outside the window: no
            // hover, no cursor, clicks on nothing. Stateful, and it looks
            // like the mouse is simply dead.
            self.last_reported_size
                .insert(surface_id, (width, height, log_w, log_h));
            // The event, on the other hand, is genuinely about the size the
            // client sees — but that includes the logical half.  A scale
            // change alone (a high-DPI viewer joining or leaving) can leave
            // the physical size untouched while the window it represents
            // triples in size, and a viewer told only the physical number
            // would go on drawing the old window at the old zoom.
            if prev != Some((width, height, log_w, log_h)) {
                let _ = self.event_tx.send(CompositorEvent::SurfaceResized {
                    surface_id,
                    width: width as u16,
                    height: height as u16,
                    logical_width: log_w.min(u16::MAX as u32) as u16,
                    logical_height: log_h.min(u16::MAX as u32) as u16,
                });
            }
        }
        // Drain into a stable order so per-surface targets are emitted
        // in a deterministic sequence.
        let (now_ms, now_sub_us) = elapsed_timestamp();
        #[allow(clippy::type_complexity)]
        let mut entries: Vec<((u16, u32, u32), (u32, u32, PixelData, bool))> =
            self.pending_commits.drain().collect();
        entries.sort_by_key(|((sid, w, h), _)| (*sid, *w, *h));
        for ((surface_id, width, height), (_log_w, _log_h, pixels, encoder_skip)) in entries {
            let _ = self.event_tx.send(CompositorEvent::SurfaceCommit {
                surface_id,
                width,
                height,
                pixels,
                timestamp_ms: now_ms,
                timestamp_sub_us: now_sub_us,
                encoder_skip,
            });
        }
        // Emitted after the commits so a client that owns a compositor
        // encoder sees its bitstream no earlier than the raw frame it was
        // built from.
        for frame in self.pending_encoded.drain(..) {
            let _ = self.event_tx.send(CompositorEvent::SurfaceEncoded {
                frame,
                timestamp_ms: now_ms,
                timestamp_sub_us: now_sub_us,
            });
        }
        (self.event_notify)();
    }

    fn read_shm_buffer(&self, buffer: &WlBuffer) -> Option<(u32, u32, PixelData)> {
        let data = buffer.data::<ShmBufferData>()?;
        let r = data.pool.read_buffer(
            data.offset,
            data.width,
            data.height,
            data.stride,
            data.format,
        );
        if r.is_none() {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 10 || n.is_multiple_of(100) {
                eprintln!(
                    "[read_shm_buffer #{n}] pool.read_buffer=None off={} {}x{} stride={} fmt={:?}",
                    data.offset, data.width, data.height, data.stride, data.format,
                );
            }
        }
        r
    }

    fn read_dmabuf_buffer(&self, buffer: &WlBuffer) -> Option<(u32, u32, PixelData)> {
        let data = buffer.data::<DmaBufBufferData>()?;
        let width = data.width as u32;
        let height = data.height as u32;
        if width == 0 || height == 0 || data.planes.is_empty() {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 10 || n.is_multiple_of(100) {
                eprintln!(
                    "[read_dmabuf_buffer #{n}] empty: {}x{} planes={}",
                    width,
                    height,
                    data.planes.len()
                );
            }
            return None;
        }
        let plane = &data.planes[0];
        if matches!(
            data.fourcc,
            drm_fourcc::ARGB8888
                | drm_fourcc::XRGB8888
                | drm_fourcc::ABGR8888
                | drm_fourcc::XBGR8888
        ) {
            // Check if this is a DRM GEM fd (importable by VA-API) or an
            // anonymous /dmabuf heap fd (Vulkan WSI, needs CPU mmap).
            use std::os::fd::AsRawFd;
            let raw_fd = plane.fd.as_raw_fd();
            let _is_drm = {
                let mut link_buf = [0u8; 256];
                let path = format!("/proc/self/fd/{raw_fd}\0");
                let n = unsafe {
                    libc::readlink(
                        path.as_ptr() as *const _,
                        link_buf.as_mut_ptr() as *mut _,
                        255,
                    )
                };
                n > 0 && link_buf[..n as usize].starts_with(b"/dev/dri/")
            };

            // Always dup the fd — the encoder handles both DRM GEM and
            // anonymous /dmabuf fds.  For /dmabuf fds, the encoder falls
            // back to CPU mmap internally.
            let owned = plane.fd.try_clone().ok()?;
            return Some((
                width,
                height,
                PixelData::DmaBuf {
                    fd: Arc::new(owned),
                    fourcc: data.fourcc,
                    modifier: data.modifier,
                    stride: plane.stride,
                    offset: plane.offset,
                    y_invert: data.y_invert,
                },
            ));
        }
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 10 || n.is_multiple_of(100) {
            eprintln!(
                "[read_dmabuf_buffer #{n}] unsupported fourcc=0x{:08x} ({}x{}) modifier=0x{:x}",
                data.fourcc, width, height, data.modifier,
            );
        }
        None
    }

    fn read_buffer(&self, buffer: &WlBuffer) -> Option<(u32, u32, PixelData)> {
        // Try SHM first, then DMA-BUF. Both paths now log their own
        // failures, so here we only log when the buffer matches neither
        // type (exotic buffer roles we don't recognise at all).
        if buffer.data::<ShmBufferData>().is_some() {
            return self.read_shm_buffer(buffer);
        }
        if buffer.data::<DmaBufBufferData>().is_some() {
            return self.read_dmabuf_buffer(buffer);
        }
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 10 || n.is_multiple_of(100) {
            eprintln!(
                "[read_buffer #{n}] buffer has unknown role (neither Shm nor DmaBuf data attached)",
            );
        }
        None
    }

    fn handle_surface_commit(&mut self, surface_id: &ObjectId) {
        let (root_id, toplevel_sid) = self.find_toplevel_root(surface_id);

        // Always consume the pending buffer so the client gets a release
        // event.  Skipping this (e.g. when the surface has no toplevel
        // role yet) leaks a buffer from the client's pool on every attach,
        // eventually starving it and causing a hang.
        let had_buffer = self
            .surfaces
            .get(surface_id)
            .is_some_and(|s| s.pending_buffer.as_ref().is_some_and(Option::is_some));
        if super::render::gpu_layer_debug() {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 40 || n.is_multiple_of(200) {
                let children = self
                    .surfaces
                    .get(surface_id)
                    .map(|s| s.children.len())
                    .unwrap_or(0);
                eprintln!(
                    "[commit-in #{n}] sid={surface_id:?} toplevel={toplevel_sid:?} root={root_id:?} had_buffer={had_buffer} children={children}",
                );
            }
        }
        self.apply_pending_state(surface_id);

        let toplevel_sid = match toplevel_sid {
            Some(sid) => sid,
            None => {
                // No toplevel yet — release any held DMA-BUF buffer since
                // no compositing will run to consume it.  Still fence-gated:
                // an earlier composite (before the toplevel went away) may
                // be in flight over its import.
                if let Some(held) = self.held_buffers.remove(surface_id) {
                    self.release_held(held);
                }
                // Fire pending frame callbacks so the client doesn't stall —
                // but paced.  Nothing composites this surface, so the
                // server's RequestFrame throttle never reaches it and a
                // client that repaints per callback would otherwise free-run
                // at full speed on a window nobody can see.  Unfired
                // callbacks stay pending; they fire on a later commit here
                // or once the surface gets a toplevel.
                const TOPLESS_FRAME_INTERVAL_MS: u32 = 250;
                let now = elapsed_ms();
                let due = self
                    .last_topless_frame_ms
                    .get(surface_id)
                    .is_none_or(|&t| now.wrapping_sub(t) >= TOPLESS_FRAME_INTERVAL_MS);
                if due {
                    self.last_topless_frame_ms.insert(surface_id.clone(), now);
                    self.fire_surface_frame_callbacks(surface_id, None);
                    let _ = self.display_handle.flush_clients();
                }
                return;
            }
        };
        if self.surfaces.get(surface_id).is_some_and(|surface| {
            !surface.pending_frame_callbacks.is_empty()
                || !surface.pending_presentation_feedbacks.is_empty()
        }) {
            self.frame_callback_toplevels.insert(toplevel_sid);
        }

        // Compositing while a previous submit's fence is still unsignalled
        // does not queue behind it — `render_tree_sized` early-returns and
        // the tree is gone.  Mid-stream that is invisible, because the next
        // commit composites the same surface state anyway.  On an app's
        // *last* commit nothing follows it: the pixels the user is waiting
        // for are never composited, so no SurfaceCommit is emitted, no
        // pixel generation is bumped, and the server's `unchanged` gate
        // then holds the client on the frame before it, indefinitely.
        // Pressing Ctrl+L in a browser surface is the everyday shape of it
        // — a short burst of commits whose last one lands in the fence
        // window and the address-bar highlight never arrives.
        //
        // So defer instead of dropping, exactly as the external-buffer and
        // downscale-target handlers already do for this same hazard.  The
        // event loop retires the in-flight submit on its 1 ms poll and
        // drains this queue once the GPU is idle.
        let deferred = self
            .vulkan_renderer
            .as_ref()
            .is_some_and(|vk| vk.would_defer_submit());
        if deferred {
            // Always `false`, overwriting any queued encoder-only entry:
            // that variant skips publishing pixels on purpose, which is
            // the one thing this commit exists to do.
            self.pending_recomposite_toplevels
                .insert(toplevel_sid, false);
            // The composite that will read this surface has not run, so the
            // buffer stays held — see the release below.  Recorded against
            // the toplevel whose recomposite consumes it so the drain
            // releases exactly what it read.
            //
            // Not when this commit parked on its acquire point, though —
            // same gate as the inline release below: the held buffer is
            // then still the surface's *displayed* content, and the drain
            // releasing it would hand it (and its release point) back
            // mid-use.  The parked buffer's promotion supersedes and
            // releases it instead.
            if self.held_buffers.contains_key(surface_id)
                && !self.awaiting_acquire.contains_key(surface_id)
            {
                self.deferred_buffer_holds
                    .entry(toplevel_sid)
                    .or_default()
                    .insert(surface_id.clone());
            }
            // Log sparsely, and only under `BLIT_DEBUG_GPU_LAYERS`: this fires
            // whenever a commit lands in the fence window, which on a busy
            // surface is often.  Every one of these used to be a discarded tree.
            if super::render::gpu_layer_debug() {
                static DEFERRED: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let n = DEFERRED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 5 || n.is_multiple_of(500) {
                    eprintln!(
                        "[commit-defer #{n}] sid={toplevel_sid}: submit in flight, \
                         queued for recomposite instead of dropping the tree",
                    );
                }
            }
        } else {
            self.composite_toplevel_into_pending(&root_id, toplevel_sid, false);
        }

        // Compositing is done — the VulkanRenderer holds its own dup'd
        // fd reference to the DMA-BUF via the persistent texture cache.
        // Release the held buffer so the client can reuse it for the
        // next frame.
        //
        // Only once something has actually read it, though.  A DMA-BUF is
        // imported, not copied, so `held_buffers` is what stops the client
        // drawing over pixels the compositor still needs (see
        // `apply_pending_state`).  "Read" means the submit's fence has
        // signalled: this used to release as soon as the read was *queued*,
        // trusting implicit DMA-BUF fencing to hold the client's next write
        // behind it — which NVIDIA's Vulkan driver does not honor, so a
        // video overlay's recycled buffer got redrawn mid-read and the
        // composite showed a future frame.  A deferred commit has queued no
        // GPU work at all: keep holding, and let the drain release it once
        // the recomposite has read.
        // Not when the commit parked on its acquire point, though: nothing
        // superseded the held buffer — it is still the surface's displayed
        // content and future composites keep sampling it until the parked
        // buffer promotes.  Releasing it here handed it (and its release
        // point) back mid-use.
        if !deferred
            && !self.awaiting_acquire.contains_key(surface_id)
            && let Some(held) = self.held_buffers.remove(surface_id)
        {
            self.release_held(held);
        }

        // Fire frame callbacks after processing a commit so clients can
        // continue their render loop — but only as a *fallback*. When a
        // viewer is connected the server paces this surface via RequestFrame
        // at the display rate (see server tick loop); that path is then the
        // sole, throttled driver. Firing again here on every commit would
        // drive a nested compositor (e.g. weston) into an unthrottled
        // repaint loop — running much faster than the display and, for
        // weston, tripping an internal subsurface assertion. So skip the
        // eager fire while RequestFrame has paced this surface recently, and
        // fall back to it otherwise (during resize, or when no subscriber is
        // driving RequestFrame) so the client never stalls. The grace window
        // is comfortably larger than the server's slowest pacing interval
        // (250 ms idle blanket), so suppression stays stable while any client
        // is connected and self-heals within it once pacing stops.
        const PACING_GRACE_MS: u32 = 500;
        let paced = self
            .last_request_frame_ms
            .get(&toplevel_sid)
            .is_some_and(|&t| elapsed_ms().wrapping_sub(t) < PACING_GRACE_MS);
        let pending_presentation = paced
            .then(|| self.pending_request_frames.get(&toplevel_sid).copied())
            .flatten();
        if let Some(presentation_at) = pending_presentation {
            // A fixed-clock tick crossed this commit's frame request before it
            // became visible to the compositor. Consume that same tick now;
            // waiting for the next one creates a full-period hole at 240 Hz.
            self.fire_frame_callbacks_for_toplevel(toplevel_sid, Some(presentation_at));
        } else if !paced {
            self.pending_request_frames.remove(&toplevel_sid);
            self.fire_frame_callbacks_for_toplevel(toplevel_sid, None);
        }

        // After an output scale change, re-send keyboard leave/enter on
        // the first commit so clients (especially Firefox) resume input
        // processing.  Deferred to here so the client has processed the
        // reconfigure before we re-enter.
        if self.pending_kb_reenter {
            self.pending_kb_reenter = false;
            // Re-enter only the surface that actually holds keyboard focus.
            // Re-entering every toplevel would hand phantom focus to
            // unfocused clients and leave their focus set, so a later real
            // focus change delivers a second `enter` with no matching
            // `leave` — the redundant-enter that crashes nested compositors
            // (see `set_keyboard_focus`). The leave-then-enter pair below is
            // safe: the `leave` clears the client's focus before the `enter`
            // re-establishes it.
            // Whoever actually holds it, which is a grabbing popup when a
            // menu is open: re-entering the toplevel instead would move
            // focus out from under the menu on a mere scale change.
            if let Some(wl) = self.keyboard_focus_wl() {
                self.send_keyboard_leave(&wl);
                self.send_keyboard_enter(&wl);
            }
            let _ = self.display_handle.flush_clients();
        }

        if self.verbose {
            let cache_entries = self.surface_meta.len();
            let has_pending = self
                .pending_commits
                .keys()
                .any(|(sid, _, _)| *sid == toplevel_sid);
            static COMMIT_COUNT: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let n = COMMIT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 5 || n.is_multiple_of(1000) {
                eprintln!(
                    "[commit #{n}] sid={surface_id:?} root={root_id:?} cache={cache_entries} pending={has_pending} buf={had_buffer}",
                );
            }
        }
    }

    /// The size this toplevel's native composite will come out at, as
    /// `(phys_w, phys_h, log_w, log_h)`, or `None` while nobody has sized
    /// it (the composite then falls back to the layer bounding box, which
    /// is only known once the layers are collected).
    ///
    /// This follows the *requested* size, so it changes the instant a
    /// `SurfaceResize` is handled — before the Wayland client has acked
    /// the configure, let alone painted.  That is deliberate: the render
    /// target is `surface_sizes` either way, so this is what the very next
    /// composite will produce, and the server needs it to size encoders
    /// against the right aspect rather than the previous one.
    ///
    /// The client's own size range applies on the way out.  A client that
    /// declared a minimum draws itself at that minimum no matter what we
    /// asked for -- Chromium's is 500 logical pixels wide -- so compositing
    /// a narrower pane at the pane's width cuts the right-hand side off the
    /// window.  Compositing at the size it really drew hands the viewer an
    /// oversized frame instead, which the browser already scales down to fit
    /// its pane.  Whole window, smaller, rather than part of one.
    /// The density this surface is watched at, 1× until a viewer says
    /// otherwise.
    fn surface_scale_120(&self, surface_id: u16) -> u16 {
        self.surface_scales
            .get(&surface_id)
            .copied()
            .unwrap_or(120)
            .max(120)
    }

    /// This surface's own logical size, or the default for one no viewer has
    /// sized yet.
    fn surface_logical_size(&self, surface_id: u16) -> (i32, i32) {
        self.surface_sizes
            .get(&surface_id)
            .copied()
            .unwrap_or((self.output_width, self.output_height))
    }

    /// Record a client's peer pid, and whether it is the X11 bridge.
    fn note_client_pid(&mut self, client: ClientId, pid: u32) {
        self.client_pids.insert(client.clone(), pid);
        if self.descends_from_xwayland(pid) {
            self.xwayland_clients.insert(client);
        }
    }

    /// Whether `pid` is the bridge or one of its children.
    ///
    /// `xwayland-satellite` connects on its own behalf, but Xwayland is its
    /// child and connects too, so the pid the server started is not always
    /// the pid on the other end of the socket.
    fn descends_from_xwayland(&self, pid: u32) -> bool {
        let Some(target) = self.xwayland_pid else {
            return false;
        };
        let mut pid = pid;
        // The bridge sits one or two levels above its connections; the bound
        // walk is only here so a pid cycle cannot hang the event loop.
        for _ in 0..8 {
            if pid == target {
                return true;
            }
            match parent_pid(pid) {
                Some(parent) if parent > 1 => pid = parent,
                _ => return false,
            }
        }
        false
    }

    /// The client a toplevel belongs to, while it is still connected.
    fn surface_owner(&self, surface_id: u16) -> Option<ClientId> {
        let root = self.toplevel_surface_ids.get(&surface_id)?;
        let surf = self.surfaces.get(root)?;
        Some(surf.wl_surface.client()?.id())
    }

    /// The stamped identity of the application a toplevel belongs to.
    ///
    /// `None` for anything on the shared socket, which is most windows: nothing
    /// is known about who connected there, and guessing from a self-asserted
    /// `app_id` would be worse than admitting it.
    fn surface_identity(&self, surface_id: u16) -> Option<&Arc<AppIdentity>> {
        self.client_identity.get(&self.surface_owner(surface_id)?)
    }

    /// Whether this client is the X11 bridge, whose screens are shared by
    /// every X window rather than owned one apiece.
    fn is_xwayland(&self, owner: &ClientId) -> bool {
        self.xwayland_clients.contains(owner)
    }

    /// The single screen offered to the X11 bridge: large enough to hold
    /// every X window blit has sized, and never smaller than the default.
    ///
    /// X clients clamp themselves to the screen they are on, so a screen
    /// smaller than the pane would stop an X app filling it.  Growing the
    /// screen to the largest window is ordinary RandR resize from the X
    /// side, where publishing a second monitor per window is not.
    fn xwayland_screen(&self, owner: &ClientId) -> (i32, i32, i32) {
        let (mut w, mut h, mut s120) = (self.output_width, self.output_height, 120u16);
        for &sid in self.toplevel_surface_ids.keys() {
            if self.surface_owner(sid).as_ref() != Some(owner) {
                continue;
            }
            let (lw, lh) = self.surface_logical_size(sid);
            w = w.max(lw);
            h = h.max(lh);
            s120 = s120.max(self.surface_scale_120(sid));
        }
        (w, h, s120 as i32)
    }

    /// The screen a surface is on: the bridge's shared one when its client
    /// is the bridge, otherwise the one this very toplevel claimed.
    fn slot_for_surface(&self, surface_id: u16, owner: &ClientId) -> Option<u32> {
        if self.is_xwayland(owner) {
            return self
                .output_slots
                .iter()
                .find(|(_, s)| s.owner == *owner)
                .map(|(&slot, _)| slot);
        }
        self.output_slots
            .iter()
            .find(|(_, s)| s.surface_id == Some(surface_id))
            .map(|(&slot, _)| slot)
    }

    /// Send an output's mode and scale.  `wl_output.scale` is integer-only,
    /// so a client that does not bind `wp_fractional_scale_v1` sees the
    /// ceiling of the real scale and draws slightly large rather than
    /// slightly small — the same trade every compositor makes.
    fn send_output_properties(&self, output: &WlOutput, slot: u32) {
        let Some(s) = self.output_slots.get(&slot) else {
            return;
        };
        let (lw, lh, s120) = if self.is_xwayland(&s.owner) {
            self.xwayland_screen(&s.owner)
        } else {
            match s.surface_id {
                Some(sid) => {
                    let (lw, lh) = self.surface_logical_size(sid);
                    (lw, lh, self.surface_scale_120(sid) as i32)
                }
                None => (self.output_width, self.output_height, 120),
            }
        };
        output.mode(
            wl_output::Mode::Current | wl_output::Mode::Preferred,
            lw * s120 / 120,
            lh * s120 / 120,
            self.output_refresh_mhz as i32,
        );
        if output.version() >= 2 {
            output.scale((s120 + 119) / 120);
            output.done();
        }
    }

    /// The toplevel currently on a screen, if any.
    fn output_slot_surface(&self, slot: u32) -> Option<u16> {
        self.output_slots.get(&slot).and_then(|s| s.surface_id)
    }

    /// Publish a screen for `owner`, optionally with a toplevel already on
    /// it.
    fn create_output_slot(&mut self, owner: ClientId, surface_id: Option<u16>) -> u32 {
        let slot = self.next_output_slot;
        self.next_output_slot = self.next_output_slot.wrapping_add(1);
        let global = self
            .display_handle
            .create_global::<Compositor, WlOutput, OutputGlobal>(
                4,
                OutputGlobal {
                    slot,
                    owner: owner.clone(),
                },
            );
        self.output_slots.insert(
            slot,
            OutputSlot {
                owner,
                global,
                surface_id,
            },
        );
        slot
    }

    /// Guarantee a connected client can always see a screen, even before it
    /// has opened a window on one.
    ///
    /// This is not a nicety.  A toolkit decides whether it *can* open a
    /// window by looking at the outputs in the registry, so a compositor
    /// that only publishes a screen once a window exists never gets one:
    /// mpv exits with "No outputs found", and Chromium and anything else
    /// GPU-accelerated simply never maps.  Only the simplest clients
    /// (alacritty) are indifferent.  So every client is offered one empty
    /// screen up front, which its first toplevel then claims.
    fn ensure_client_output(&mut self, owner: ClientId) {
        if self.output_slots.values().any(|s| s.owner == owner) {
            return;
        }
        self.create_output_slot(owner, None);
    }

    /// Put a toplevel on a screen: the empty one the client is already
    /// looking at when there is one, a freshly published screen otherwise.
    ///
    /// Reusing the empty screen is what makes the first window work.  The
    /// client bound that output during startup and sized itself against it;
    /// handing it a *second* screen at map time would leave the first — the
    /// one it is still reasoning about — describing nothing.
    fn claim_output_for_surface(&mut self, surface_id: u16, owner: ClientId) {
        // The X11 bridge keeps the one screen it was offered on connect: its
        // windows share a desktop, so they share a screen.  Claiming one
        // apiece would publish an X monitor per window.
        if self.is_xwayland(&owner) {
            self.ensure_client_output(owner.clone());
            if let Some(slot) = self.slot_for_surface(surface_id, &owner) {
                self.announce_slot(slot);
            }
            return;
        }
        if self
            .output_slots
            .values()
            .any(|s| s.surface_id == Some(surface_id))
        {
            return;
        }
        let spare = self
            .output_slots
            .iter()
            .find(|(_, s)| s.owner == owner && s.surface_id.is_none())
            .map(|(&slot, _)| slot);
        let slot = match spare {
            Some(slot) => {
                if let Some(s) = self.output_slots.get_mut(&slot) {
                    s.surface_id = Some(surface_id);
                }
                slot
            }
            None => self.create_output_slot(owner, Some(surface_id)),
        };
        // The screen now describes a window, so its mode and density
        // changed: ordinary hotplug from the client's side.
        self.announce_slot(slot);
    }

    /// Take a toplevel off its screen, and withdraw the screen — unless it
    /// is the client's last one, which is emptied and kept.
    ///
    /// An app that closes its only window is still running and may open
    /// another; dropping to zero outputs would strand it exactly as a cold
    /// start with no screen does.
    fn release_output_for_surface(&mut self, surface_id: u16) {
        self.surface_scales.remove(&surface_id);
        let Some(slot) = self
            .output_slots
            .iter()
            .find(|(_, s)| s.surface_id == Some(surface_id))
            .map(|(&slot, _)| slot)
        else {
            // A bridge window holds no screen of its own, but the screen it
            // shared may have been sized around it.
            let shared: Vec<u32> = self
                .output_slots
                .iter()
                .filter(|(_, s)| self.is_xwayland(&s.owner))
                .map(|(&slot, _)| slot)
                .collect();
            for slot in shared {
                self.announce_slot(slot);
            }
            return;
        };
        let owner = self.output_slots[&slot].owner.clone();
        let others = self
            .output_slots
            .iter()
            .filter(|&(&k, s)| k != slot && s.owner == owner)
            .count();
        if others == 0 {
            if let Some(s) = self.output_slots.get_mut(&slot) {
                s.surface_id = None;
            }
            self.announce_slot(slot);
            return;
        }
        if let Some(s) = self.output_slots.remove(&slot) {
            self.display_handle
                .disable_global::<Compositor>(s.global.clone());
            self.retired_output_globals.push(RetiredOutputGlobal {
                owner: s.owner,
                global: s.global,
                slot,
            });
        }
        self.outputs.retain(|o| o.slot != slot);
    }

    /// Re-send a screen's properties to everyone who bound it.
    fn announce_slot(&self, slot: u32) {
        for out in self.outputs.iter().filter(|o| o.slot == slot) {
            self.send_output_properties(&out.resource, slot);
        }
    }

    /// Re-announce a surface's output after its scale or size changed.
    fn refresh_output_for_surface(&self, surface_id: u16) {
        let s120 = self.surface_scale_120(surface_id) as u32;
        // The bridge's screen is sized from all its windows at once, so a
        // single window growing moves it too.
        if let Some(slot) = self
            .surface_owner(surface_id)
            .and_then(|owner| self.slot_for_surface(surface_id, &owner))
        {
            self.announce_slot(slot);
        }
        for fs in self.fractional_scales.iter() {
            if self.find_toplevel_root(&fs.surface).1 == Some(surface_id) {
                fs.resource.preferred_scale(s120);
            }
        }
    }

    fn native_composite_size(&self, toplevel_sid: u16) -> Option<(u32, u32, u32, u32)> {
        let &(lw, lh) = self.surface_sizes.get(&toplevel_sid)?;
        let (lw, lh) = match self
            .toplevel_surface_ids
            .get(&toplevel_sid)
            .and_then(|root_id| self.surfaces.get(root_id))
        {
            Some(surf) => constrain_to_hints(surf, lw, lh),
            None => (lw, lh),
        };
        let s120 = self.surface_scale_120(toplevel_sid) as u32;
        let pw = super::render::to_physical(lw as u32, s120);
        let ph = super::render::to_physical(lh as u32, s120);
        Some((pw, ph, (pw * 120).div_ceil(s120), (ph * 120).div_ceil(s120)))
    }

    /// Run the GPU compositor for `toplevel_sid` and store the produced
    /// frames into `pending_commits` / `pending_native_sizes` so they're
    /// drained by the next `flush_pending_commits` tick.  Drives both the
    /// surface-commit path (after applying a fresh client buffer) and
    /// the per-target registration paths (so a freshly-installed
    /// downscale target / external buffer pool is populated immediately
    /// from the most-recent surface state — without it, an idle wayland
    /// client never produces pixels at the new target size and the
    /// per-client encoder skips forever, wedging the surface).
    fn composite_toplevel_into_pending(
        &mut self,
        root_id: &ObjectId,
        toplevel_sid: u16,
        encoder_only: bool,
    ) {
        // Composite at the output scale so HiDPI clients are rendered
        // at full resolution.  Use the browser's requested size as the
        // target so the frame fits the canvas without letterboxing.
        let s120 = self.surface_scale_120(toplevel_sid);
        let native = self.native_composite_size(toplevel_sid);
        let target_phys = native.map(|(pw, ph, _, _)| (pw, ph));
        let mut encode_giveups: Vec<(u32, u64)> = Vec::new();
        let screen_cast = self.screencast_surfaces.contains(&toplevel_sid);
        let (submitted_native, composited) = if let Some(ref mut vk) = self.vulkan_renderer {
            if screen_cast {
                vk.request_native_bgra();
            }
            let rendered = vk.render_tree_sized(
                root_id,
                &self.surfaces,
                &self.surface_meta,
                s120,
                target_phys,
                toplevel_sid,
            );
            self.pending_encoded.extend(vk.take_encoded_frames());
            encode_giveups = vk.take_encode_giveups();
            rendered
        } else {
            (None, Vec::new())
        };

        // A session that has stopped producing bitstreams reports the same
        // way a session that was never created does, so the server takes the
        // path it already has: latch the refusal and build a server-side
        // encoder.  Without this the server cannot tell a dead encoder from
        // one that is still warming up, and waits on it forever.
        for (sid, cid) in encode_giveups {
            if let Some(vk) = self.vulkan_renderer.as_mut() {
                vk.destroy_vulkan_encoder(sid, Some(cid));
            }
            let _ = self
                .event_tx
                .send(CompositorEvent::VulkanEncoderUnavailable {
                    surface_id: sid as u16,
                    client_id: cid,
                    after_encode_failures: true,
                });
            (self.event_notify)();
        }

        // Record exactly what this call submitted as the native composite.
        // Published frames are not a fallback: with only a sidebar viewer
        // they can consist solely of its 512-ish target, and results retired
        // at the start of this call can even belong to the prior submission.
        let s120_u32 = (s120 as u32).max(120);
        if let Some((sid, nw, nh, nlog_w, nlog_h)) =
            native_size_after_render(toplevel_sid, native, submitted_native, s120_u32)
        {
            self.pending_native_sizes
                .insert(sid, (nw, nh, nlog_w, nlog_h));
        }

        for (result_sid, w, h, pixels, encoder_skip) in composited {
            if pixels.is_empty() {
                continue;
            }
            // The bitstreams this render produced are already collected.
            // The pixels are identical to what the server last saw, so
            // publishing them would only burn a generation and make every
            // other viewer re-encode the frame it is already showing.
            if encoder_only {
                continue;
            }
            let kind = match &pixels {
                PixelData::Bgra(_) => "bgra",
                PixelData::Rgba(_) => "rgba",
                PixelData::Nv12 { .. } => "nv12",
                PixelData::VaSurface { .. } => "va-surface",
                PixelData::Nv12DmaBuf { .. } => "nv12-dmabuf",
                PixelData::Nv12OpaqueFd { .. } => "nv12-opaque-fd",
                PixelData::GpuOnly => "gpu-only",
                PixelData::DmaBuf { fd, .. } => {
                    use std::os::fd::AsRawFd;
                    let raw = fd.as_raw_fd();
                    let mut lb = [0u8; 128];
                    let p = format!("/proc/self/fd/{raw}\0");
                    let n = unsafe {
                        libc::readlink(p.as_ptr() as *const _, lb.as_mut_ptr() as *mut _, 127)
                    };
                    if n > 0 && lb[..n as usize].starts_with(b"/dev/dri/") {
                        "dmabuf-drm"
                    } else {
                        "dmabuf-anon"
                    }
                }
            };
            if self.verbose {
                static LC: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let lc = LC.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if lc < 3 || lc.is_multiple_of(1000) {
                    eprintln!("[pending #{lc}] {w}x{h} kind={kind}");
                }
            }
            // Logical size derived from this target's physical size at
            // the same scale.  Each entry in `pending_commits` produces
            // one `SurfaceCommit` so the server can route the per-target
            // frame to the correct per-client encoder.
            let log_w = (w * 120).div_ceil(s120_u32);
            let log_h = (h * 120).div_ceil(s120_u32);
            self.pending_commits
                .insert((result_sid, w, h), (log_w, log_h, pixels, encoder_skip));
        }
    }

    /// Compute the absolute position of a surface within its toplevel by
    /// walking up the parent chain and summing `subsurface_position` offsets.
    /// The toplevel root itself has position (0, 0).
    fn surface_absolute_position(&self, surface_id: &ObjectId) -> (i32, i32) {
        let mut x = 0i32;
        let mut y = 0i32;
        let mut current = surface_id.clone();
        while let Some(surf) = self.surfaces.get(&current) {
            x += surf.subsurface_position.0;
            y += surf.subsurface_position.1;
            match surf.parent_surface_id {
                Some(ref parent) => current = parent.clone(),
                None => break,
            }
        }
        (x, y)
    }

    fn find_toplevel_root(&self, surface_id: &ObjectId) -> (ObjectId, Option<u16>) {
        let mut current = surface_id.clone();
        loop {
            match self.surfaces.get(&current) {
                Some(surf) => {
                    if let Some(ref parent) = surf.parent_surface_id {
                        current = parent.clone();
                    } else {
                        return (
                            current,
                            if surf.surface_id > 0 {
                                Some(surf.surface_id)
                            } else {
                                None
                            },
                        );
                    }
                }
                None => return (current, None),
            }
        }
    }

    fn collect_surface_tree(&self, root_id: &ObjectId) -> Vec<ObjectId> {
        let mut result = Vec::new();
        self.collect_tree_recursive(root_id, &mut result);
        result
    }

    fn collect_tree_recursive(&self, surface_id: &ObjectId, result: &mut Vec<ObjectId>) {
        result.push(surface_id.clone());
        if let Some(surf) = self.surfaces.get(surface_id) {
            for child_id in &surf.children {
                self.collect_tree_recursive(child_id, result);
            }
        }
    }

    /// Walk the surface tree rooted at `root_id` and return the topmost
    /// mapped surface whose pixel bounds contain (`x`, `y`).  Returns
    /// `(wl_surface, local_x, local_y)` with coordinates relative to the hit
    /// surface.  An unmapped child suppresses its whole subtree, as the
    /// Wayland mapping rules require; if no child accepts the point, input
    /// retains the compositor's historical toplevel fallback — but only while
    /// the toplevel is itself mapped.  Entering an unmapped surface is a
    /// protocol violation, and it is reachable: `unmap_surface_content` sends
    /// that surface a `leave`, and without this guard the next motion event
    /// hands it a fresh `enter` with nothing on screen.
    fn hit_test_surface_at(
        &self,
        root_id: &ObjectId,
        x: f64,
        y: f64,
    ) -> Option<(WlSurface, f64, f64)> {
        self.hit_test_recursive(root_id, x, y, 0, 0).or_else(|| {
            self.surfaces
                .get(root_id)
                .filter(|surface| surface.map_state != MapState::Unmapped)
                .map(|surface| (surface.wl_surface.clone(), x, y))
        })
    }

    fn hit_test_recursive(
        &self,
        surface_id: &ObjectId,
        x: f64,
        y: f64,
        offset_x: i32,
        offset_y: i32,
    ) -> Option<(WlSurface, f64, f64)> {
        let surf = self.surfaces.get(surface_id)?;
        // Content presence is the surface's mapped state.  Children retain
        // their own buffers across a parent unmap, but are not mapped again
        // until every ancestor has current content.
        if surf.map_state != MapState::Mapped {
            return None;
        }
        let sx = offset_x + surf.subsurface_position.0;
        let sy = offset_y + surf.subsurface_position.1;

        // Children are ordered back-to-front; iterate in reverse for topmost.
        for child_id in surf.children.iter().rev() {
            if let Some(hit) = self.hit_test_recursive(child_id, x, y, sx, sy) {
                return Some(hit);
            }
        }

        // Check this surface's bounds (logical coordinates).  A mapped surface
        // whose buffer we could not read has no meta and therefore no bounds
        // of its own, but its descendants above were still considered.
        let sm = self.surface_meta.get(surface_id)?;
        let (lw, lh) = super::render::surface_logical_size(surf, sm);
        let lx = x - sx as f64;
        let ly = y - sy as f64;
        if lx >= 0.0 && ly >= 0.0 && lx < lw && ly < lh {
            // A surface can decline input over part or all of itself, and
            // then the pointer belongs to whatever is behind it. Firefox
            // relies on this: it puts its rendering in a subsurface
            // covering the whole window and sets that subsurface's input
            // region empty, so input falls through to the toplevel where
            // its widget code is listening.
            match surf.input_region {
                Some(ref ops) if !input_region::contains(ops, lx, ly) => {}
                _ => return Some((surf.wl_surface.clone(), lx, ly)),
            }
        }
        None
    }

    /// Apply double-buffered pending state and consume the pending buffer.
    ///
    /// SHM buffers are uploaded as persistent GPU textures and released
    /// immediately.  DMA-BUF buffers are imported into VulkanRenderer's
    /// persistent texture cache and the wl_buffer is held in
    /// `held_buffers` so the client cannot reuse the underlying GPU
    /// memory while compositing reads from it.
    /// The held buffer is released after compositing completes in
    /// `handle_surface_commit`, or immediately if there is no toplevel
    /// to composite.  The Vulkan renderer imports DMA-BUFs on the GPU
    /// and handles vendor-specific tiled layouts (NVIDIA, AMD) natively
    /// — CPU mmap of such buffers would produce garbage or block.
    /// Install any parked commit whose explicit-sync acquire point has
    /// signalled, and recomposite the toplevel it belongs to.  Runs every
    /// loop pass; the loop shortens its poll timeout while anything waits
    /// so a point signalled between Wayland events lands within ~1 ms.
    fn promote_ready_acquires(&mut self) {
        if self.awaiting_acquire.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        let ready: Vec<(ObjectId, bool)> = self
            .awaiting_acquire
            .iter()
            .filter_map(|(id, a)| {
                if a.acquire.signaled() {
                    Some((id.clone(), false))
                } else if now.duration_since(a.parked_at) >= ACQUIRE_PARK_TIMEOUT {
                    Some((id.clone(), true))
                } else {
                    None
                }
            })
            .collect();
        for (surface_id, timed_out) in ready {
            let Some(a) = self.awaiting_acquire.remove(&surface_id) else {
                continue;
            };
            if timed_out {
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!(
                        "[compositor] acquire point still unsignalled after {}ms; installing anyway rather than freezing the surface",
                        ACQUIRE_PARK_TIMEOUT.as_millis(),
                    );
                }
            }
            // The commit's state was held back with its buffer; both land
            // together now.
            let damage = self.apply_committed_state(&surface_id);
            self.commit_buffer(&surface_id, a.buf, a.scale, a.is_cursor, a.release, &damage);
            let (root_id, toplevel_sid) = self.find_toplevel_root(&surface_id);
            let Some(tl) = toplevel_sid else {
                // No toplevel resolves (destroyed while this commit was
                // parked, or a role-less surface): no composite will ever
                // read the buffer `commit_buffer` just held.  Release it —
                // and signal its release point — exactly like
                // `handle_surface_commit`'s no-toplevel branch; holding on
                // starves the client's buffer pool, and an explicit-sync
                // client waiting on that release point wedges outright.
                if let Some(held) = self.held_buffers.remove(&surface_id) {
                    self.release_held(held);
                }
                continue;
            };
            // Mirror handle_surface_commit's tail exactly.  Routing every
            // promoted frame through the cache-refill recomposite queue
            // instead would serialize to one toplevel per loop pass AND
            // force the native BGRA readback per frame — the copy the
            // zero-copy path exists to suppress — on a path that runs for
            // nearly every frame of an explicit-sync client, since those
            // commit before their GPU work signals.
            let deferred = self
                .vulkan_renderer
                .as_ref()
                .is_some_and(|vk| vk.would_defer_submit());
            if deferred {
                self.pending_recomposite_toplevels.insert(tl, false);
                if self.held_buffers.contains_key(&surface_id) {
                    self.deferred_buffer_holds
                        .entry(tl)
                        .or_default()
                        .insert(surface_id.clone());
                }
            } else {
                self.composite_toplevel_into_pending(&root_id, tl, false);
                if let Some(held) = self.held_buffers.remove(&surface_id) {
                    self.release_held(held);
                }
            }
        }
    }

    /// Release a held client buffer, fence-gated on in-flight GPU work,
    /// signalling its explicit-sync release point at the same moment.
    fn release_held(&mut self, held: HeldBuffer) {
        let HeldBuffer { buf, release } = held;
        let gated = self
            .vulkan_renderer
            .as_mut()
            .is_some_and(|vk| vk.defer_buffer_release(buf.clone(), release.clone()));
        if !gated {
            buf.release();
            if let Some(r) = release {
                r.signal();
            }
        }
    }

    fn apply_pending_state(&mut self, surface_id: &ObjectId) {
        // The size hints are checked against each other here rather than as
        // they arrive: both halves are double-buffered, so a client may raise
        // its minimum past the old maximum as long as the new maximum lands in
        // the same commit.  Only once they are applied together is a minimum
        // above a maximum actually contradictory.
        if let Some(surf) = self.surfaces.get(surface_id) {
            let ((min_w, min_h), (max_w, max_h)) = (surf.pending_min_size, surf.pending_max_size);
            // Zero is "no opinion", so it never conflicts.
            if (max_w > 0 && min_w > max_w) || (max_h > 0 && min_h > max_h) {
                if let Some(tl) = surf.xdg_toplevel.clone() {
                    tl.post_error(
                        xdg_toplevel::Error::InvalidSize,
                        format!("minimum size {min_w}x{min_h} exceeds maximum {max_w}x{max_h}"),
                    );
                }
                return;
            }
        }
        // Only the buffer and its sync points are taken here: the rest of
        // the double-buffered state stays pending until the commit is known
        // to install.  Applying it up front is what made a parked commit
        // visible — scale, viewport and subsurface position would describe
        // the new frame while `surface_meta` still described the old buffer,
        // so the composite below stretched the previous frame into the new
        // destination rect and cropped it against stale dimensions, for the
        // frame or two until the promotion sweep ran.
        let (buffer, scale, is_cursor, acquire, release, syncobj_surface) = {
            let Some(surf) = self.surfaces.get_mut(surface_id) else {
                return;
            };
            (
                surf.pending_buffer.take(),
                surf.pending_buffer_scale,
                surf.is_cursor,
                surf.pending_acquire_point.take(),
                surf.pending_release_point.take(),
                surf.syncobj_surface.clone(),
            )
        };
        let Some(buffer) = buffer else {
            // No new content, so there is nothing to stay in step with and
            // the state applies now, as the protocol says it must.  The
            // points came without a buffer, which the spec calls an error
            // and blit treats leniently everywhere else: signal the release
            // rather than dropping it, or the client waits on a point that
            // can never be reached.
            self.apply_committed_state(surface_id);
            if let Some(r) = release {
                r.signal();
            }
            drop(acquire);
            return;
        };
        let Some(buf) = buffer else {
            // `attach(NULL)` is a real buffer-state change: it unmaps the
            // surface.  Apply the rest of the commit atomically, retire both
            // the displayed and any acquire-waiting buffer, and leave its
            // role/tree position intact so a subsurface can map again later.
            self.apply_committed_state(surface_id);
            if let Some(r) = release {
                r.signal();
            }
            drop(acquire);
            self.unmap_surface_content(surface_id);
            return;
        };
        // Mapped from here on: the client attached content.  Whether the
        // upload below succeeds decides only whether we can *draw* it.
        if let Some(surf) = self.surfaces.get_mut(surface_id) {
            surf.map_state = MapState::Mapped;
        }

        // The spec makes an unfenced buffer commit on an explicit-sync
        // surface a fatal protocol error — but Chromium-family browsers
        // associate the syncobj surface early and then commit software
        // raster frames without points during startup, and killing the
        // connection there is a browser that hangs before its first
        // window.  Be lenient: treat such commits as unfenced (which SHM
        // is anyway, and an unfenced dma-buf is no worse than a client
        // that never bound the protocol), and say so once.
        // A missing *release* is no reason to throw away a usable acquire:
        // dropping it composites the dma-buf with no wait at all, which is
        // exactly the early sample this path exists to prevent — the
        // client's previous frame, on the frame it navigates.  Only an
        // absent acquire, or SHM (copied on commit, so a GPU fence means
        // nothing), makes the commit genuinely unfenced.
        let unfenced = syncobj_surface.is_some()
            && (acquire.is_none() || buf.data::<ShmBufferData>().is_some());
        let (acquire, release) = if unfenced {
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let cause = if acquire.is_none() {
                    "no acquire point"
                } else {
                    "SHM buffer"
                };
                eprintln!(
                    "[compositor] explicit-sync surface committed with {cause}; treating as unfenced (spec says error, lenient for browser startup)",
                );
            }
            // A release point without a usable acquire is still honored so
            // the client's buffer bookkeeping never wedges.
            (None, release)
        } else {
            (acquire, release)
        };

        // Explicit sync, acquire side: the buffer's content is not ready
        // until the acquire point signals, and NVIDIA's Vulkan driver will
        // not learn that from implicit fencing — sampling early shows the
        // buffer's *previous* frame.  Prefer a GPU-side wait: export the
        // point's fence as a sync_file, import it as a semaphore, and let
        // the next composite submit wait on it — the commit installs
        // immediately and costs no latency, exactly like a client on
        // implicit sync with a driver that honors it.  Only when the point
        // has no fence yet (the client committed before submitting its GPU
        // work) fall back to parking the commit for the promotion sweep.
        if let Some(acq) = acquire
            && !acq.signaled()
        {
            let gpu_waited = acq
                .export_sync_file()
                .ok()
                .and_then(|fd| {
                    self.vulkan_renderer
                        .as_mut()
                        .map(|vk| vk.add_acquire_wait_fd(fd))
                })
                .unwrap_or(false);
            if !gpu_waited {
                let waiting = AwaitingBuffer {
                    buf,
                    scale,
                    is_cursor,
                    acquire: acq,
                    release,
                    parked_at: std::time::Instant::now(),
                };
                if let Some(prev) = self.awaiting_acquire.insert(surface_id.clone(), waiting) {
                    // Superseded before its content was ever ready, so it
                    // was never read — but its release point still goes
                    // through the fence gate, in order with the displayed
                    // buffer's.
                    let stale = prev.into_release();
                    self.release_held(stale);
                }
                // The commit's state stays pending with it: applying it now
                // would describe this buffer while the surface still shows
                // the last one.
                return;
            }
        }

        // This commit installs now — signalled, unfenced, or ordered on
        // the queue by the GPU-side wait — so it supersedes any older
        // commit still parked on its acquire point; discard that one the
        // same way a newer *parked* commit would.  Leaving it in place let
        // the promotion sweep later install the stale frame over this one
        // (a fence-less first commit parks, the next fenced one installs
        // past it, and a first-start burst then regressed to the client's
        // initial blank frame), and its mere presence made the
        // post-composite release gate treat this surface as parked,
        // stranding the buffer installed here.
        if let Some(stale) = self.awaiting_acquire.remove(surface_id) {
            let stale = stale.into_release();
            self.release_held(stale);
        }

        // This commit is installing, so its state lands with its pixels.
        let damage = self.apply_committed_state(surface_id);
        self.commit_buffer(surface_id, buf, scale, is_cursor, release, &damage);
    }

    /// Apply a commit's double-buffered surface state.  Split out of
    /// `apply_pending_state` so it runs at the moment the commit's buffer
    /// becomes the surface's content — immediately for an ordinary commit,
    /// at the promotion sweep for one parked on its acquire point — and
    /// never describes a frame that is not on screen yet.
    fn apply_committed_state(&mut self, surface_id: &ObjectId) -> Vec<PendingDamage> {
        let Some(surf) = self.surfaces.get_mut(surface_id) else {
            return Vec::new();
        };
        surf.min_size = surf.pending_min_size;
        surf.max_size = surf.pending_max_size;
        surf.buffer_scale = surf.pending_buffer_scale;
        surf.viewport_destination = surf.pending_viewport_destination;
        surf.viewport_source = surf.pending_viewport_source;
        surf.is_opaque = surf.pending_opaque;
        if let Some(region) = surf.pending_input_region.take() {
            surf.input_region = region;
        }
        let damage = std::mem::take(&mut surf.pending_damage);
        if let Some(pos) = surf.pending_subsurface_position.take() {
            surf.subsurface_position = pos;
        }
        damage
    }

    /// Install a committed buffer as the surface's current content.  The
    /// second half of `apply_pending_state`, split out so a commit parked
    /// on its explicit-sync acquire point can be installed later.
    fn commit_buffer(
        &mut self,
        surface_id: &ObjectId,
        buf: WlBuffer,
        scale: i32,
        is_cursor: bool,
        release: Option<crate::drm_syncobj::SyncPoint>,
        damage: &[PendingDamage],
    ) {
        // Release any previously held buffer for this surface — the new
        // commit supersedes it.  Not straight away, though: in-flight GPU
        // work may still sample its import, implicit dma-buf fencing does
        // not hold NVIDIA's Vulkan driver off the memory, and a client
        // with a fast-recycling pool (a video overlay subsurface) redraws
        // a released buffer within the composite window — the in-flight
        // composite then shows a *future* frame and the video visibly
        // jumps back and forth.  Gate the release on the submit's fence.
        if let Some(old) = self.held_buffers.remove(surface_id) {
            self.release_held(old);
        }

        // A surface with no live toplevel — never mapped, or its toplevel
        // destroyed while the wl_surface lives on — is never composited, so
        // an upload has no consumer.  It still cost one full-size
        // host-visible texture per commit, and with no composite running,
        // nothing drained the eviction backlog either: an animating client
        // in this state grew the server by w*h*4 per commit until the
        // kernel OOM-killed it (observed: >100 GB anonymous RSS in minutes
        // from a 60 fps client).  Consume and release the buffer so the
        // client's pool doesn't starve, but keep the pixels on the floor.
        // Cursor surfaces are exempt: their pixels feed `cursor_rgba`.
        if !is_cursor && self.find_toplevel_root(surface_id).1.is_none() {
            buf.release();
            if let Some(r) = release {
                r.signal();
            }
            return;
        }

        // Fast path for non-cursor SHM buffers: the client's mmap'd pool
        // has the pixels already; we copy+convert straight into Vulkan
        // memory and skip the `read_buffer → Vec<u8>` intermediate. Cursor
        // surfaces still go through the slow path because they need an
        // owned RGBA copy for the cursor protocol.
        if !is_cursor && let Some(shm) = buf.data::<ShmBufferData>() {
            let w = shm.width as u32;
            let h = shm.height as u32;
            let stride = shm.stride as usize;
            let offset = shm.offset as usize;
            let format = shm.format;
            // Reject negative/degenerate client geometry before it is cast to
            // usize (a negative i32 becomes a huge value that can wrap the
            // bounds check in the closure below).
            if shm.width > 0
                && shm.height > 0
                && shm.stride >= 0
                && shm.offset >= 0
                && let Some(ref mut vk) = self.vulkan_renderer
            {
                let source_bgra =
                    !matches!(format, wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888);
                let force_opaque =
                    matches!(format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Xbgr8888);
                let (viewport_source, viewport_destination) = self
                    .surfaces
                    .get(surface_id)
                    .map(|surface| (surface.viewport_source, surface.viewport_destination))
                    .unwrap_or((None, None));
                let damage =
                    shm_damage_rects(damage, w, h, scale, viewport_source, viewport_destination);
                let row_bytes = w as usize * 4;
                let buffer_id = buf.id();
                let upload_result = shm
                    .pool
                    .with_mmap(|slice| {
                        // Checked arithmetic: a crafted stride/height/offset
                        // could otherwise wrap this sum and pass the bounds
                        // check, causing an out-of-bounds read of the mmap.
                        let needed = stride
                            .checked_mul(h as usize - 1)
                            .and_then(|body| body.checked_add(offset))
                            .and_then(|n| n.checked_add(row_bytes));
                        match needed {
                            Some(n) if n <= slice.len() => {}
                            _ => return None,
                        }
                        vk.upload_surface_shm_mmap(
                            surface_id,
                            &buffer_id,
                            shm.pool.fd.as_raw_fd(),
                            slice,
                            offset,
                            stride,
                            w,
                            h,
                            source_bgra,
                            force_opaque,
                            &damage,
                        )
                    })
                    .flatten();
                if let Some(upload_result) = upload_result {
                    self.surface_meta.insert(
                        surface_id.clone(),
                        super::render::SurfaceMeta {
                            width: w,
                            height: h,
                            scale,
                            y_invert: false,
                        },
                    );
                    if upload_result == super::vulkan_render::ShmUploadResult::Imported {
                        self.held_buffers
                            .insert(surface_id.clone(), HeldBuffer { buf, release });
                    } else {
                        buf.release();
                        if let Some(r) = release {
                            r.signal();
                        }
                    }
                    return;
                }
            }
        }

        if let Some((w, h, pixels)) = self.read_buffer(&buf) {
            let y_invert = matches!(pixels, PixelData::DmaBuf { y_invert: true, .. });

            // Upload the surface's pixel data as a persistent GPU texture.
            if let Some(ref mut vk) = self.vulkan_renderer {
                vk.upload_surface(surface_id, Some(&buf.id()), &pixels, w, h);
            }

            // Store per-surface metadata for layout, hit-testing, etc.
            self.surface_meta.insert(
                surface_id.clone(),
                super::render::SurfaceMeta {
                    width: w,
                    height: h,
                    scale,
                    y_invert,
                },
            );

            // Cursor surfaces need CPU-accessible RGBA pixels for cursor
            // image events (they aren't GPU-composited).
            if is_cursor {
                let rgba = pixels.to_rgba(w, h);
                if !rgba.is_empty() {
                    self.cursor_rgba.insert(surface_id.clone(), (w, h, rgba));
                }
            }

            if pixels.is_dmabuf() {
                // Hold the wl_buffer alive so the client cannot reuse it
                // while the GPU texture still references the DMA-BUF fd.
                // The explicit-sync release point travels with the hold and
                // is signalled at the same fence-gated moment.
                self.held_buffers
                    .insert(surface_id.clone(), HeldBuffer { buf, release });
            } else {
                // SHM buffers are snapshotted into the GPU texture.
                // Release immediately so the client can reuse the buffer.
                buf.release();
                if let Some(r) = release {
                    r.signal();
                }
            }
        } else {
            buf.release();
            if let Some(r) = release {
                r.signal();
            }
        }
    }

    fn fire_surface_frame_callbacks(
        &mut self,
        surface_id: &ObjectId,
        presentation_at: Option<std::time::Instant>,
    ) -> bool {
        let (callbacks, feedbacks) = {
            let Some(surf) = self.surfaces.get_mut(surface_id) else {
                return false;
            };
            (
                std::mem::take(&mut surf.pending_frame_callbacks),
                std::mem::take(&mut surf.pending_presentation_feedbacks),
            )
        };
        // Keep wl_callback.done on the fixed frame-clock phase, but report
        // wp_presentation at the instant we actually emit the feedback. A
        // scheduled deadline is only the compositor's target; cross-thread
        // dispatch and a late client commit can make it older than the real
        // presentation, while converting an Instant back into the advertised
        // CLOCK_MONOTONIC domain has an unavoidable sampling error. Chromium
        // exposes that error as negative frame latency. The protocol asks for
        // when the content became visible, so "now" is the accurate value.
        let (callback_sec, callback_nsec) =
            presentation_at.map_or_else(monotonic_timespec, monotonic_timespec_at);
        let fired = !callbacks.is_empty() || !feedbacks.is_empty();
        let time = (callback_sec as u32)
            .wrapping_mul(1000)
            .wrapping_add(callback_nsec as u32 / 1_000_000);
        for cb in callbacks {
            cb.done(time);
        }
        if !feedbacks.is_empty() {
            let (presented_sec, presented_nsec) = monotonic_timespec();
            // Send sync_output for each feedback, then presented().
            for fb in feedbacks {
                for out in &self.outputs {
                    if same_client(&fb, &out.resource) {
                        fb.sync_output(&out.resource);
                    }
                }
                // refresh in nanoseconds (millihertz → ns: 1e12 / mhz)
                let refresh_ns = if self.output_refresh_mhz > 0 {
                    (1_000_000_000_000u64 / self.output_refresh_mhz as u64) as u32
                } else {
                    0
                };
                fb.presented(
                    (presented_sec >> 32) as u32,
                    presented_sec as u32,
                    presented_nsec as u32,
                    refresh_ns,
                    0, // sequence unknown for the synthetic output
                    0,
                    // The headless output is software-timed, but it is still
                    // presented on the fixed refresh timeline above.
                    WpPresentationFeedbackKind::Vsync,
                );
            }
        }
        fired
    }

    /// Forget pointer focus if it named `gone`.
    ///
    /// The pointer cannot be inside a surface that no longer exists, and
    /// nothing else clears this. Buttons survive a dangling id because the
    /// server always sends a `PointerMotion` immediately before a
    /// `PointerButton`, and that motion re-enters; scroll gets no such
    /// escort — `PointerAxis` arrives alone and ignores its own `surface_id`,
    /// resolving the stale id to no surface and dropping the event. So a
    /// dismissed context menu leaves the wheel dead until the cursor happens
    /// to cross into another surface.
    ///
    /// Keyboard focus is already cleared on both destroy paths for the
    /// equivalent reason (`focused_surface_id`); this is its pointer twin.
    fn forget_pointer_focus(&mut self, gone: &ObjectId) {
        if self.pointer_entered_id.as_ref() == Some(gone) {
            self.pointer_entered_id = None;
        }
    }

    /// Remove a still-live surface from pointer focus before unmapping it.
    ///
    /// Unlike destruction, unmapping leaves the `wl_surface` resource alive,
    /// so the client is owed a `leave`.  Clearing only our local id makes a
    /// later remap deliver a second `enter` with no matching leave, while not
    /// clearing it leaves axis events routed to invisible popup content.
    fn leave_pointer_focus(&mut self, gone: &ObjectId) {
        if self.pointer_entered_id.as_ref() != Some(gone) {
            return;
        }
        let wl = self
            .surfaces
            .get(gone)
            .map(|surface| surface.wl_surface.clone());
        self.pointer_entered_id = None;
        let Some(wl) = wl else {
            return;
        };
        let serial = self.next_serial();
        for ptr in &self.pointers {
            if same_client(ptr, &wl) {
                ptr.leave(serial, &wl);
                ptr.frame();
            }
        }
    }

    /// Drop the current buffer and every compositor-side cache derived from
    /// it while retaining the surface's role and tree position.  This is the
    /// content half of a Wayland unmap; a subsurface can attach a new buffer
    /// later and map again in the same position.
    ///
    /// Deliberately does *not* queue a recomposite. Every caller but
    /// `unmap_popup_surface` is a commit, and `handle_surface_commit` composites
    /// and publishes the (already updated) tree inline further down — queueing
    /// here would encode the same frame a second time on every unmap commit.
    fn unmap_surface_content(&mut self, surface_id: &ObjectId) {
        // Resolve the toplevel before anything else: the caller may already
        // Unmapping a parent also unmaps every descendant.  Pointer focus is
        // normally on the deepest hit surface, so clear that descendant too
        // instead of testing only the surface that received attach(NULL).
        let focused = self
            .pointer_entered_id
            .clone()
            .filter(|focused| self.is_in_subtree(focused, surface_id));
        if let Some(focused) = focused {
            self.leave_pointer_focus(&focused);
        }
        let loses_touch_target = self
            .active_touches
            .values()
            .any(|active| self.is_in_subtree(&active.target.id(), surface_id))
            // A touch drag has no `active_touches` entry — `start_drag`
            // cancelled the sequence — so its origin is checked separately, or
            // an unmap during a touch drag would leave the drag running against
            // a surface that is gone.  A dropped drag is excluded: it is waiting
            // on the target's `finish`, and cancelling that tail fails a
            // transfer that already succeeded.
            || (self.client_touch_drag_contact().is_some()
                && self
                    .client_drag
                    .as_ref()
                    .is_some_and(|drag| self.is_in_subtree(&drag.origin.id(), surface_id)));
        if loses_touch_target {
            // wl_touch has one cancel event for the whole sequence. If any
            // target disappears, retire the sequence rather than leaving a
            // contact bound to an unmapped wl_surface.
            self.cancel_touch_owner(None);
        }
        // A popup can be unmapped without its role being destroyed — GTK hides
        // a menu with `attach(NULL); commit` and reuses it later.  Retiring the
        // pixels but not the grab leaves keyboard focus on an invisible surface
        // and makes the dismiss loop swallow the user's next click.
        let grabbed: Vec<ObjectId> = self
            .popup_grab_stack
            .iter()
            .filter(|id| self.is_in_subtree(id, surface_id))
            .cloned()
            .collect();
        for grab_id in grabbed {
            self.popup_grab_stack.retain(|id| *id != grab_id);
            self.unfocus_popup(&grab_id);
        }
        if let Some(surf) = self.surfaces.get_mut(surface_id) {
            surf.map_state = MapState::Unmapped;
        }
        // xdg-toplevel-drag detaches an attached toplevel automatically when
        // it is unmapped, allowing the same drag object to attach a replacement
        // window later in the session.
        self.detach_toplevel_drag_surface(surface_id);
        self.surface_meta.remove(surface_id);
        self.cursor_rgba.remove(surface_id);
        if let Some(ref mut vk) = self.vulkan_renderer {
            vk.remove_surface(surface_id);
        }
        if let Some(held) = self.held_buffers.remove(surface_id) {
            self.release_held(held);
        }
        if let Some(awaiting) = self.awaiting_acquire.remove(surface_id) {
            self.release_held(awaiting.into_release());
        }
    }

    /// Tear down the whole popup grab stack, topmost first.
    ///
    /// Shared by the pointer-press and touch-down dismissal paths so a menu
    /// closed by tap and one closed by click leave identical focus state.
    fn dismiss_popup_grabs(&mut self) {
        while let Some(grab_id) = self.popup_grab_stack.pop() {
            if let Some(surface) = self.surfaces.get(&grab_id)
                && let Some(ref popup) = surface.xdg_popup
            {
                popup.popup_done();
            }
            // `popup_done` itself unmaps the surface.  Do not wait for the
            // client to destroy the role, or a static page keeps streaming the
            // last composite with the menu still in it.
            self.unmap_popup_surface(&grab_id);
            // Pop first, then hand focus back: `unfocus_popup` reads the stack
            // to find what is still grabbing underneath, and on the last
            // iteration that is nothing, so focus lands on the toplevel.
            self.unfocus_popup(&grab_id);
        }
    }

    /// Whether `id` is `ancestor` or one of its descendants, walking the
    /// `parent_surface_id` chain.
    fn is_in_subtree(&self, id: &ObjectId, ancestor: &ObjectId) -> bool {
        let mut at = Some(id.clone());
        while let Some(current) = at {
            if &current == ancestor {
                return true;
            }
            at = self
                .surfaces
                .get(&current)
                .and_then(|surface| surface.parent_surface_id.clone());
        }
        false
    }

    /// Stop drawing a dismissed popup and queue a fresh frame for its
    /// toplevel.
    ///
    /// `xdg_popup.popup_done` unmaps the popup at the compositor's end of
    /// the event; waiting for the client to destroy its role is not the same
    /// operation.  Chromium does destroy it, but an idle page may not commit
    /// another toplevel buffer afterwards.  Merely unlinking the popup then
    /// leaves the last composite (menu included) on screen until the next
    /// paced repaint, which is hundreds of milliseconds later in Brave.
    fn unmap_popup_surface(&mut self, popup_id: &ObjectId) {
        // Resolve the toplevel before unlinking the parent chain.  The queued
        // recomposite reads the tree after the removal, so it reveals the parent
        // pixels that were behind the menu.
        let (_, toplevel_sid) = self.find_toplevel_root(popup_id);
        let parent_id = self
            .surfaces
            .get(popup_id)
            .and_then(|surface| surface.parent_surface_id.clone());
        let mut was_mapped = false;
        if let Some(parent_id) = parent_id
            && let Some(parent) = self.surfaces.get_mut(&parent_id)
        {
            let old_len = parent.children.len();
            parent.children.retain(|child| child != popup_id);
            was_mapped = parent.children.len() != old_len;
        }
        if !was_mapped {
            return;
        }

        // An unmapped surface has no current content.  Drop the render cache
        // now as well as the tree edge, otherwise reusing this wl_surface for
        // another popup could briefly resurrect the old menu before its first
        // new buffer commit.
        self.unmap_surface_content(popup_id);

        // Unlike every other unmap, this one is not driven by a commit, so no
        // inline composite follows it and an idle page would keep streaming the
        // menu.  `false` is important: this must publish pixels, not run only
        // compositor-resident encoders.
        if let Some(toplevel_sid) = toplevel_sid {
            self.pending_recomposite_toplevels
                .insert(toplevel_sid, false);
        }
    }

    /// What to multiply a smooth `wl_pointer.axis` distance by before
    /// sending it to this pointer's client.
    ///
    /// The protocol calls the axis value a distance "in a coordinate space
    /// identical to those of motion events" — surface-local pixels — and
    /// that is what blit sends, as does Mutter. Chromium reads it as
    /// detents anyway: `wayland_pointer.cc` divides by a hardcoded
    /// `kAxisValueScale = 10` and multiplies by `kWheelDelta = 120`, for
    /// every source including `finger`, then hands the result to Blink as
    /// precise pixels. A pixel-valued axis therefore scrolls Chromium and
    /// Electron windows exactly twelve times too far.
    ///
    /// So spell the same distance for them in the detent units Weston
    /// established and Mutter still emits (`DEFAULT_AXIS_STEP_DISTANCE`,
    /// ten per detent). Everyone else — GTK, which treats the value as
    /// `GDK_SCROLL_UNIT_SURFACE` pixels, and winit, which hands it to
    /// Alacritty as a `PixelDelta` — keeps pixels. `axis_value120` is
    /// unaffected: its unit is unambiguous and every toolkit agrees on it.
    ///
    /// Xwayland belongs with Chromium, for the same reason and by the same
    /// factor: `dispatch_scroll_motion` divides the smooth value by ten into
    /// a scroll valuator declared with an increment of one click, so a whole
    /// X session scrolled twelve clicks per detent of trackpad travel. Every
    /// X11 application inherits that, which is why it reads as X11 scrolling
    /// being broken rather than as one toolkit misbehaving.
    fn smooth_axis_scale(&mut self, ptr: &WlPointer) -> f64 {
        let Some(client) = ptr.client() else {
            return 1.0;
        };
        let id = client.id();
        // The X11 bridge reads the axis the same way Chromium does, and is
        // asked before the cache because a client only becomes known as the
        // bridge once the server names its pid — which can land after its
        // first scroll, and a cached 1.0 would outlive the correction.
        //
        // `xwayland-input.c` splits on exactly the same line blit does: with
        // `axis_value120` it takes the detents outright, and without one it
        // divides the smooth value by ten into a scroll valuator whose
        // increment is one click. So a wheel arrives intact and a trackpad --
        // which sends no value120 at all -- moved twelve clicks per detent.
        if self.is_xwayland(&id) {
            return AXIS_UNITS_PER_DETENT / PX_PER_DETENT;
        }
        if let Some(&scale) = self.axis_scale.get(&id) {
            return scale;
        }
        let scale = match client.get_credentials(&self.display_handle) {
            Ok(creds) if pid_is_chromium(creds.pid) => AXIS_UNITS_PER_DETENT / PX_PER_DETENT,
            _ => 1.0,
        };
        self.axis_scale.insert(id, scale);
        scale
    }

    /// Remove surfaces whose underlying `WlSurface` is no longer alive.
    /// This handles the case where a Wayland client process exits or crashes
    /// without explicitly destroying its surfaces — `dispatch_clients()`
    /// marks the resources as dead, and we clean up here.
    fn cleanup_disconnected_clients(&mut self) {
        if self.cleanup_needed.swap(false, Ordering::AcqRel) {
            self.cleanup_dead_surfaces();
        }
    }

    fn remove_foreign_export(&mut self, object_id: &ObjectId) {
        let Some((handle, _)) = self.foreign_export_objects.remove(object_id) else {
            return;
        };
        if let Ok(mut exports) = self.foreign_exports.write() {
            exports.remove(&handle);
        }
    }

    fn remove_foreign_exports_for_surface(&mut self, surface_id: u16) {
        let handles = self
            .foreign_export_objects
            .iter()
            .filter(|(_, (_, exported_surface))| *exported_surface == surface_id)
            .map(|(object_id, (handle, _))| (object_id.clone(), handle.clone()))
            .collect::<Vec<_>>();
        if let Ok(mut exports) = self.foreign_exports.write() {
            for (_, handle) in &handles {
                exports.remove(handle);
            }
        }
        for (object_id, _) in handles {
            self.foreign_export_objects.remove(&object_id);
        }
    }

    fn cleanup_dead_surfaces(&mut self) {
        let dead: Vec<ObjectId> = self
            .surfaces
            .iter()
            .filter(|(_, surf)| !surf.wl_surface.is_alive())
            .map(|(id, _)| id.clone())
            .collect();
        if self
            .active_touches
            .values()
            .any(|active| dead.contains(&active.target.id()))
        {
            self.cancel_touch_owner(None);
        }

        // Purge stale protocol objects from disconnected clients.
        self.fractional_scales.retain(|fs| fs.resource.is_alive());
        self.outputs.retain(|o| o.resource.is_alive());
        // A screen outlives the window on it, but not the client it was
        // offered to — an empty slot is kept for the next window, so
        // nothing else would ever reclaim one belonging to a client that
        // has gone.
        let backend = self.display_handle.backend_handle();
        let dead_slots: Vec<u32> = self
            .output_slots
            .iter()
            .filter(|(_, s)| backend.get_client_data(s.owner.clone()).is_err())
            .map(|(&slot, _)| slot)
            .collect();
        for slot in dead_slots {
            if let Some(s) = self.output_slots.remove(&slot) {
                self.display_handle.remove_global::<Compositor>(s.global);
            }
            self.outputs.retain(|o| o.slot != slot);
        }
        self.client_pids
            .retain(|client, _| backend.get_client_data(client.clone()).is_ok());
        self.xwayland_clients
            .retain(|client| backend.get_client_data(client.clone()).is_ok());
        self.client_identity
            .retain(|client, _| backend.get_client_data(client.clone()).is_ok());
        // Disabled globals remain bindable specifically so a client cannot
        // lose a race with `global_remove`.  Once that client is dead there
        // can be no in-flight bind, and no other client was ever allowed to
        // see the owner-filtered global, so it is finally safe to free it.
        let mut live_retired = Vec::with_capacity(self.retired_output_globals.len());
        for retired in self.retired_output_globals.drain(..) {
            if backend.get_client_data(retired.owner.clone()).is_err() {
                self.display_handle
                    .remove_global::<Compositor>(retired.global);
                self.outputs.retain(|o| o.slot != retired.slot);
            } else {
                live_retired.push(retired);
            }
        }
        self.retired_output_globals = live_retired;
        self.seats.retain(|s| s.is_alive());
        self.keyboards.retain(|k| k.is_alive());
        self.pointers.retain(|p| p.is_alive());
        self.touches.retain(|t| t.is_alive());
        self.data_devices.retain(|d| d.is_alive());
        self.primary_devices.retain(|d| d.is_alive());
        self.relative_pointers.retain(|p| p.is_alive());
        self.text_inputs.retain(|ti| ti.resource.is_alive());
        self.shm_pools.retain(|_, p| p.resource.is_alive());
        self.dmabuf_params.retain(|_, p| p.resource.is_alive());
        self.positioners.retain(|_, p| p.resource.is_alive());
        // A client that disconnects without destroying its regions never
        // sends `wl_region.destroy`, so reclaim its builder entries here.
        self.regions.retain(|_, (r, _)| r.is_alive());
        // The axis-scale cache is keyed by client rather than by resource,
        // so nothing above reclaims it, and a long session launches a lot
        // of apps.
        let live: std::collections::HashSet<_> = self
            .pointers
            .iter()
            .filter_map(|p| p.client().map(|c| c.id()))
            .collect();
        self.axis_scale.retain(|id, _| live.contains(id));

        for proto_id in &dead {
            self.surface_meta.remove(proto_id);
            // A client that crashes with a live cursor surface never sends
            // `wl_surface.destroy`, which is the only other place this is
            // reclaimed, so the RGBA would be leaked for the session's life.
            self.cursor_rgba.remove(proto_id);
            if self.current_cursor_surface.as_ref() == Some(proto_id) {
                self.current_cursor_surface = None;
            }
            if let Some(ref mut vk) = self.vulkan_renderer {
                vk.remove_surface(proto_id);
            }
            if let Some(held) = self.held_buffers.remove(proto_id) {
                self.release_held(held);
            }
            if let Some(a) = self.awaiting_acquire.remove(proto_id) {
                let a = a.into_release();
                self.release_held(a);
            }
            self.forget_pointer_focus(proto_id);
            // Unconditionally: a dead popup that was *not* the focus holder was
            // left on the grab stack forever, so the dismiss loop kept swallowing
            // clicks for a surface that no longer exists.
            let was_grabbing = self.popup_grab_stack.contains(proto_id);
            self.popup_grab_stack.retain(|id| id != proto_id);
            // A crashed client's popup takes the keyboard with it otherwise:
            // the override would name a surface that no longer exists, and
            // `keyboard_focus_wl` would answer None for every later event.
            if self.kb_focus_popup.as_ref() == Some(proto_id) {
                self.kb_focus_popup = self.popup_grab_stack.last().cloned();
                if let Some(next) = self.keyboard_focus_wl() {
                    self.send_keyboard_enter(&next);
                }
            } else if was_grabbing {
                // The stack shrank under the current holder; keep it pointing at
                // whatever still grabs.
                self.kb_focus_popup = self
                    .kb_focus_popup
                    .take()
                    .filter(|id| self.popup_grab_stack.contains(id));
            }
            if let Some(surf) = self.surfaces.remove(proto_id) {
                // Discard any pending presentation feedbacks — the surface
                // died before the frame was ever presented.
                for fb in surf.pending_presentation_feedbacks {
                    fb.discarded();
                }
                self.last_topless_frame_ms.remove(proto_id);
                if let Some(ref parent_id) = surf.parent_surface_id
                    && let Some(parent) = self.surfaces.get_mut(parent_id)
                {
                    parent.children.retain(|c| c != proto_id);
                }
                if surf.surface_id > 0 {
                    self.remove_foreign_exports_for_surface(surf.surface_id);
                    self.screencast_surfaces.remove(&surf.surface_id);
                    self.release_output_for_surface(surf.surface_id);
                    self.toplevel_surface_ids.remove(&surf.surface_id);
                    // Clear keyboard focus if it pointed at the dead surface,
                    // so a reused surface id is treated as a fresh focus
                    // change (and gets its first `wl_keyboard.enter`) rather
                    // than a redundant re-enter. See `set_keyboard_focus`.
                    if self.focused_surface_id == surf.surface_id {
                        self.focused_surface_id = 0;
                    }
                    self.last_request_frame_ms.remove(&surf.surface_id);
                    self.pending_request_frames.remove(&surf.surface_id);
                    self.frame_callback_toplevels.remove(&surf.surface_id);
                    self.last_reported_size.remove(&surf.surface_id);
                    self.pointer_frame_positions.remove(&surf.surface_id);
                    self.surface_sizes.remove(&surf.surface_id);
                    if let Some(ref mut vk) = self.vulkan_renderer {
                        vk.destroy_external_outputs_for_surface(surf.surface_id as u32);
                    }
                    let _ = self.event_tx.send(CompositorEvent::SurfaceDestroyed {
                        surface_id: surf.surface_id,
                    });
                    (self.event_notify)();
                }
            }
        }
    }

    /// Re-send a toplevel's current configure, restating the size and states
    /// it already has.  Used to answer requests we decline, so a client that
    /// optimistically applied the new state locally snaps back to what we
    /// actually show.
    fn reassert_toplevel_configure(&mut self, wl_surface_id: &ObjectId) {
        let Some(surf) = self.surfaces.get(wl_surface_id) else {
            return;
        };
        let (w, h) = self
            .surface_sizes
            .get(&surf.surface_id)
            .copied()
            .unwrap_or((self.output_width, self.output_height));
        let (w, h) = constrain_to_hints(surf, w, h);
        if let Some(ref tl) = surf.xdg_toplevel {
            tl.configure(w, h, pane_states(surf.xdg_fullscreen));
        }
        if let Some(ref xs) = surf.xdg_surface {
            let serial = self.serial.wrapping_add(1);
            self.serial = serial;
            xs.configure(serial);
        }
        let _ = self.display_handle.flush_clients();
    }

    fn fire_frame_callbacks_for_toplevel(
        &mut self,
        toplevel_sid: u16,
        presentation_at: Option<std::time::Instant>,
    ) -> bool {
        if !self.frame_callback_toplevels.contains(&toplevel_sid) {
            return false;
        }
        let Some(root_id) = self.toplevel_surface_ids.get(&toplevel_sid).cloned() else {
            self.frame_callback_toplevels.remove(&toplevel_sid);
            return false;
        };
        let tree = self.collect_surface_tree(&root_id);
        let mut fired = false;
        for sid in &tree {
            fired |= self.fire_surface_frame_callbacks(sid, presentation_at);
        }
        if fired {
            self.pending_request_frames.remove(&toplevel_sid);
            let _ = self.display_handle.flush_clients();
        }
        // The set is a cheap readiness hint. If topology changed underneath
        // a pending request, clear the stale hint and let the next protocol
        // request or commit re-arm it.
        self.frame_callback_toplevels.remove(&toplevel_sid);
        fired
    }

    /// Which surface a cursor change belongs to.
    ///
    /// The cursor follows the pointer, not the keyboard, so the surface being
    /// hovered is the one whose viewers should see the shape change.  Keying
    /// this off `focused_surface_id` files an I-beam set by an unfocused pane
    /// under the focused one, which the shared-pointer overlay then draws on
    /// the wrong surface.  Keyboard focus is the fallback for the case where
    /// nothing is hovered.
    fn cursor_target_sid(&self) -> u16 {
        self.pointer_entered_id
            .as_ref()
            .and_then(|id| self.find_toplevel_root(id).1)
            .unwrap_or(self.focused_surface_id)
    }

    fn handle_cursor_commit(&mut self, surface_id: &ObjectId) {
        // Only the surface currently set on the pointer speaks for the cursor.
        // `is_cursor` is latched by the first `set_cursor` and never cleared, so
        // a toolkit retiring a pooled cursor frame it is not showing would
        // otherwise blank the live cursor.
        let is_current = self.current_cursor_surface.as_ref() == Some(surface_id);
        let unmaps = is_current
            && self
                .surfaces
                .get(surface_id)
                .is_some_and(|surface| matches!(surface.pending_buffer.as_ref(), Some(None)));
        self.apply_pending_state(surface_id);
        let hotspot = self
            .surfaces
            .get(surface_id)
            .map(|s| s.cursor_hotspot)
            .unwrap_or((0, 0));
        let target_sid = self.cursor_target_sid();
        if unmaps {
            self.last_cursor.insert(target_sid, CursorImage::Hidden);
            let _ = self.event_tx.send(CompositorEvent::SurfaceCursor {
                surface_id: target_sid,
                cursor: CursorImage::Hidden,
            });
        } else if is_current
            && let Some((w, h, rgba)) = self.cursor_rgba.get(surface_id)
            && !rgba.is_empty()
        {
            // `hotspot` is surface-local (logical) per `wl_pointer.set_cursor`,
            // while `w`/`h` are raw buffer pixels.  Carry the cursor's own
            // buffer scale so a consumer can put them in one space instead of
            // guessing from the pane's scale.
            let scale = self
                .surfaces
                .get(surface_id)
                .map_or(1, |s| s.buffer_scale.clamp(1, i32::from(u16::MAX)) as u16);
            let cursor = CursorImage::Custom {
                hotspot_x: hotspot.0 as u16,
                hotspot_y: hotspot.1 as u16,
                width: *w as u16,
                height: *h as u16,
                scale,
                rgba: rgba.clone(),
            };
            // A cursor surface commits far more often than its artwork
            // changes -- Xwayland re-attaches on enter and on every update it
            // was throttling -- and re-announcing artwork a viewer is already
            // drawing costs a full image on the wire for nothing. Worse, a
            // viewer that rebuilds its object URL per announcement blinks the
            // cursor each time, so this is not merely wasted bandwidth.
            if self.last_cursor.get(&target_sid) == Some(&cursor) {
                self.fire_surface_frame_callbacks(surface_id, None);
                let _ = self.display_handle.flush_clients();
                return;
            }
            self.last_cursor.insert(target_sid, cursor.clone());
            let _ = self.event_tx.send(CompositorEvent::SurfaceCursor {
                surface_id: target_sid,
                cursor,
            });
        }
        self.fire_surface_frame_callbacks(surface_id, None);
        let _ = self.display_handle.flush_clients();
    }

    fn pointer_focus_matches_surface(&self, surface_id: u16) -> bool {
        let Some(root_id) = self.toplevel_surface_ids.get(&surface_id) else {
            return false;
        };
        self.surface_meta.contains_key(root_id)
            && self.pointer_entered_id.as_ref().is_some_and(|entered| {
                self.surface_meta.contains_key(entered)
                    && self.find_toplevel_root(entered).0 == *root_id
            })
    }

    /// Every lookup `pointer_focus_matches_surface` consults, for a
    /// `--verbose` log.
    ///
    /// A dropped scroll is invisible from both ends: the browser sent it, the
    /// client never saw it, and the five `return`s that can swallow it are
    /// distinguishable only from in here. Scroll dying in one pane while its
    /// neighbours work is the shape this state produces, so print the state
    /// rather than the conclusion.
    fn axis_target_state(&self, surface_id: u16) -> String {
        let root = self.toplevel_surface_ids.get(&surface_id);
        let entered = self.pointer_entered_id.as_ref();
        format!(
            "sid={surface_id} root={root:?} root_meta={} entered={entered:?} \
             entered_meta={} entered_root={:?} lrs={:?} last_point={:?}",
            root.is_some_and(|r| self.surface_meta.contains_key(r)),
            entered.is_some_and(|e| self.surface_meta.contains_key(e)),
            entered.map(|e| self.find_toplevel_root(e).0),
            self.last_reported_size.get(&surface_id),
            self.pointer_frame_positions.get(&surface_id),
        )
    }

    /// Hit-test and dispatch one pointer motion in composited-frame
    /// coordinates. The browser motion path and scroll retargeting share this
    /// so they cannot disagree about scale, crop, popup, or subsurface rules.
    fn dispatch_pointer_motion(&mut self, surface_id: u16, x: f64, y: f64, time_ms: u32) {
        let time = self.input_event_time(time_ms);
        // The browser sends coordinates in the composited frame's physical
        // pixel space. Convert to logical (surface-local) coordinates using
        // the actual composited-to-logical ratio for this surface.
        let (mut x, mut y) =
            if let Some(&(cw, ch, lw, lh)) = self.last_reported_size.get(&surface_id) {
                let sx = if cw > 0 { lw as f64 / cw as f64 } else { 1.0 };
                let sy = if ch > 0 { lh as f64 / ch as f64 } else { 1.0 };
                (x * sx, y * sy)
            } else {
                (x, y)
            };
        // The composited frame is cropped to xdg_geometry (if set), so the
        // browser's (0,0) corresponds to (geo_x, geo_y) in the surface tree.
        if let Some((gx, gy, _, _)) = self
            .toplevel_surface_ids
            .get(&surface_id)
            .and_then(|rid| self.surfaces.get(rid))
            .and_then(|s| s.xdg_geometry)
        {
            x += gx as f64;
            y += gy as f64;
        }
        // Hit-test the surface tree to find the actual target (which may be a
        // subsurface or popup rather than the root).
        let target_wl = self
            .toplevel_surface_ids
            .get(&surface_id)
            .and_then(|root_id| self.hit_test_surface_at(root_id, x, y))
            .map(|(wl_surface, lx, ly)| (wl_surface.id(), wl_surface, lx, ly));

        static PTR_DBG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let pn = PTR_DBG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if pn < 5 || pn.is_multiple_of(500) {
            let root = self.toplevel_surface_ids.get(&surface_id).cloned();
            let lrs = self.last_reported_size.get(&surface_id).copied();
            eprintln!(
                "[pointer #{pn}] sid={surface_id} logical=({x:.1},{y:.1}) lrs={lrs:?} root={root:?} hit={:?}",
                target_wl
                    .as_ref()
                    .map(|(pid, _, lx, ly)| format!("proto={pid:?} local=({lx:.1},{ly:.1})"))
            );
        }
        if let Some((proto_id, wl_surface, lx, ly)) = target_wl {
            // Remember where we are inside the surface, so a pointer created
            // after this can be entered at the right spot.
            self.pointer_entered_local = (lx, ly);
            let matching_ptrs = self
                .pointers
                .iter()
                .filter(|p| same_client(*p, &wl_surface))
                .count();
            if let Some(change) = focus_transition(
                self.pointer_entered_id.as_ref(),
                &proto_id,
                matching_ptrs > 0,
            ) {
                let serial = self.next_serial();
                if matching_ptrs == 0 {
                    eprintln!(
                        "[pointer-enter] proto={proto_id:?} has no pointer yet (total_ptrs={}); deferring",
                        self.pointers.len()
                    );
                }
                if let Some(ref leaving) = change.leave {
                    let old_wl = self
                        .surfaces
                        .values()
                        .find(|s| s.wl_surface.id() == *leaving)
                        .map(|s| s.wl_surface.clone());
                    if let Some(old_wl) = old_wl {
                        for ptr in &self.pointers {
                            if same_client(ptr, &old_wl) {
                                ptr.leave(serial, &old_wl);
                                ptr.frame();
                            }
                        }
                    }
                }
                for ptr in &self.pointers {
                    if same_client(ptr, &wl_surface) {
                        ptr.enter(serial, &wl_surface, lx, ly);
                    }
                }
                // An enter nobody received is not focus. The next motion
                // retries instead of silently dispatching into a void.
                self.pointer_entered_id = change.entered;
            }
            for ptr in &self.pointers {
                if same_client(ptr, &wl_surface) {
                    ptr.motion(time, lx, ly);
                    ptr.frame();
                }
            }
        }
    }

    fn handle_command(&mut self, cmd: CompositorCommand) {
        match cmd {
            // Registering a socket needs the event loop's handle, which this
            // method does not have, so the command loop intercepts it before
            // dispatching here — same reason `Shutdown` is handled there.
            // Reaching this arm would mean that interception was lost, and
            // silently dropping the fd would strand an app with a socket
            // nothing ever accepts on.
            CompositorCommand::AddAppSocket { identity, .. } => {
                eprintln!(
                    "[compositor] BUG: AddAppSocket for {} reached handle_command; \
                     the command loop must intercept it",
                    identity.app_id
                );
            }
            CompositorCommand::KeyInput {
                surface_id: _,
                keycode,
                pressed,
                time_ms,
            } => {
                let serial = self.next_serial();
                let time = self.input_event_time(time_ms);
                let state = if pressed {
                    wl_keyboard::KeyState::Pressed
                } else {
                    wl_keyboard::KeyState::Released
                };
                // Popup-aware: same client either way, so the keys already
                // arrived — but asking the same question everywhere keeps
                // "who has the keyboard" from having two answers.
                let focused_wl = self.keyboard_focus_wl();
                for kb in &self.keyboards {
                    if let Some(ref wl) = focused_wl
                        && same_client(kb, wl)
                    {
                        kb.key(serial, time, keycode, state);
                    }
                }
                // Send wl_keyboard.modifiers if this key changed modifier
                // state.  Many Wayland clients (GTK, Chromium, Qt) rely on
                // this event rather than computing modifiers from raw key
                // events.
                self.update_and_send_modifiers(keycode, pressed);
                let _ = self.display_handle.flush_clients();
            }
            // Synthesised keys for an IME commit: no browser key event stands
            // behind them, so `0` takes the compositor's own clock.
            CompositorCommand::TextInput { text } => {
                // Whoever holds `wl_keyboard.enter` — a grabbing popup, else
                // the focused toplevel — is also who was sent the text-input
                // `enter`, so both halves below address the same surface.
                let Some(focused_wl) = self.keyboard_focus_wl() else {
                    return;
                };

                // Synthesise evdev key sequences for ASCII
                // characters that exist on the US-QWERTY layout.
                //
                // The browser sends text (rather than raw keycodes) for
                // printable characters when Ctrl/Alt/Meta are NOT held,
                // so that keyboard layout differences are handled by the
                // browser.  However, the physical Shift key may still be
                // held -- its keydown was already forwarded as a raw evdev
                // event, so `mods_depressed` already has MOD_SHIFT.
                //
                // The synthetic Shift press/release we inject around
                // shifted characters must not corrupt the real modifier
                // state.  Save and restore `mods_depressed` so that a
                // subsequent key combo (e.g. Ctrl+Shift+Q) still sees
                // the Shift modifier from the physically-held key.
                const KEY_LEFTSHIFT: u32 = 42;
                // Withdraw any composition before typing into it.  A
                // commit_string that lands while a preedit is still up is
                // applied at the composition's anchor rather than at the
                // caret, so mixing it with synthesised keys puts the two
                // halves in the wrong order — real Chromium turns "hi日本語"
                // into "日本語hi".  Clearing first also means the key path,
                // which sends no `done` of its own, cannot leave an
                // abandoned composition on screen.
                self.clear_stale_preedit(&focused_wl);
                let saved_mods_depressed = self.mods_depressed;
                // Characters US-QWERTY has no key for go to the input method
                // instead.  They accumulate into runs so that a mixed string
                // ("café") still reaches the app in source order: each run is
                // flushed before the next key that follows it.
                let mut composed = String::new();
                for ch in text.chars() {
                    if let Some((kc, need_shift)) = char_to_keycode(ch) {
                        self.flush_composed(&focused_wl, &mut composed);
                        let time = self.input_event_time(0);
                        if need_shift {
                            let serial = self.next_serial();
                            for kb in &self.keyboards {
                                if same_client(kb, &focused_wl) {
                                    kb.key(
                                        serial,
                                        time,
                                        KEY_LEFTSHIFT,
                                        wl_keyboard::KeyState::Pressed,
                                    );
                                }
                            }
                            self.update_and_send_modifiers(KEY_LEFTSHIFT, true);
                        }
                        let serial = self.next_serial();
                        for kb in &self.keyboards {
                            if same_client(kb, &focused_wl) {
                                kb.key(serial, time, kc, wl_keyboard::KeyState::Pressed);
                            }
                        }
                        let serial = self.next_serial();
                        for kb in &self.keyboards {
                            if same_client(kb, &focused_wl) {
                                kb.key(serial, time, kc, wl_keyboard::KeyState::Released);
                            }
                        }
                        if need_shift {
                            let serial = self.next_serial();
                            for kb in &self.keyboards {
                                if same_client(kb, &focused_wl) {
                                    kb.key(
                                        serial,
                                        time,
                                        KEY_LEFTSHIFT,
                                        wl_keyboard::KeyState::Released,
                                    );
                                }
                            }
                            self.update_and_send_modifiers(KEY_LEFTSHIFT, false);
                        }
                    } else {
                        composed.push(ch);
                    }
                }
                self.flush_composed(&focused_wl, &mut composed);
                // Restore the real modifier state that was active before
                // text synthesis.  If the user is still holding Shift,
                // this puts MOD_SHIFT back into mods_depressed.
                if self.mods_depressed != saved_mods_depressed {
                    self.mods_depressed = saved_mods_depressed;
                    let serial = self.next_serial();
                    for kb in &self.keyboards {
                        if same_client(kb, &focused_wl) {
                            kb.modifiers(serial, self.mods_depressed, 0, self.mods_locked, 0);
                        }
                    }
                }
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::Preedit { text, cursor } => {
                let Some(focused_wl) = self.keyboard_focus_wl() else {
                    return;
                };
                self.send_preedit(&focused_wl, &text, cursor);
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::PointerMotion {
                surface_id,
                x,
                y,
                time_ms,
            } => {
                // A client-initiated drag owns the pointer: motion drives
                // the drag session instead of wl_pointer.
                if self.client_pointer_drag_grabbed() {
                    self.client_drag_motion(surface_id, x, y);
                    let _ = self.display_handle.flush_clients();
                    return;
                }
                self.pointer_frame_positions.insert(surface_id, (x, y));
                self.dispatch_pointer_motion(surface_id, x, y, time_ms);
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::PointerButton {
                surface_id: _,
                button,
                pressed,
                time_ms,
            } => {
                // A client-initiated drag swallows button input: presses go
                // nowhere, and the release ends the grab — drop on the
                // current target, or dnd_cancelled when there is none.
                if self.client_pointer_drag_grabbed() {
                    if !pressed {
                        self.client_drag_release();
                    }
                    let _ = self.display_handle.flush_clients();
                    return;
                }
                let serial = self.next_serial();
                let time = self.input_event_time(time_ms);
                let state = if pressed {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };

                // If a popup is grabbed and the pointer clicked outside
                // the popup chain, dismiss the topmost grabbed popup.
                let mut dismissed = false;
                if pressed && !self.popup_grab_stack.is_empty() {
                    let click_on_grabbed = self.pointer_entered_id.as_ref().is_some_and(|eid| {
                        self.popup_grab_stack.iter().any(|gid| {
                            self.surfaces
                                .get(gid)
                                .is_some_and(|s| s.wl_surface.id() == *eid)
                        })
                    });
                    if !click_on_grabbed {
                        self.dismiss_popup_grabs();
                        let _ = self.display_handle.flush_clients();
                        dismissed = true;
                    }
                }

                // The click that closed a menu is spent on closing it; see
                // `button_routing`, whose tests enumerate the cases.
                let (routing, swallow) =
                    button_routing(pressed, button, dismissed, self.popup_dismiss_button);
                self.popup_dismiss_button = swallow;

                if routing == ButtonRouting::Deliver {
                    let focused_wl = self
                        .surfaces
                        .values()
                        .find(|s| Some(s.wl_surface.id()) == self.pointer_entered_id)
                        .map(|s| s.wl_surface.clone());
                    for ptr in &self.pointers {
                        if let Some(ref wl) = focused_wl
                            && same_client(ptr, wl)
                        {
                            ptr.button(serial, time, button, state);
                            ptr.frame();
                        }
                    }
                }
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::PointerAxis {
                surface_id,
                dx,
                dy,
                time_ms,
                v120_x,
                v120_y,
                source,
                stop,
            } => {
                if !self.pointer_focus_matches_surface(surface_id) {
                    // One Wayland seat is shared by every browser viewer. A
                    // motion from another pane can therefore steal focus
                    // between this viewer's wheel motion and axis message.
                    // Re-hit-test the last point on the surface the axis
                    // explicitly named before dispatching the delta.
                    let point = self
                        .pointer_frame_positions
                        .get(&surface_id)
                        .copied()
                        .or_else(|| {
                            self.last_reported_size.get(&surface_id).map(
                                |&(width, height, _, _)| {
                                    (f64::from(width) / 2.0, f64::from(height) / 2.0)
                                },
                            )
                        })
                        .or_else(|| {
                            let root_id = self.toplevel_surface_ids.get(&surface_id)?;
                            let surface = self.surfaces.get(root_id)?;
                            let meta = self.surface_meta.get(root_id)?;
                            let (width, height) = surface.xdg_geometry.map_or_else(
                                || super::render::surface_logical_size(surface, meta),
                                |(_, _, width, height)| (f64::from(width), f64::from(height)),
                            );
                            Some((width / 2.0, height / 2.0))
                        });
                    if let Some((x, y)) = point {
                        self.dispatch_pointer_motion(surface_id, x, y, time_ms);
                    } else if self.verbose {
                        eprintln!(
                            "[axis-drop] no point to re-seed the hit test: {}",
                            self.axis_target_state(surface_id)
                        );
                    }
                    if !self.pointer_focus_matches_surface(surface_id) {
                        // An invalid, unmapped, or pointerless destination is
                        // not permission to scroll whichever surface happened
                        // to hold the shared seat before this message.
                        if self.verbose {
                            eprintln!(
                                "[axis-drop] target still not entered after re-hit-test: {}",
                                self.axis_target_state(surface_id)
                            );
                        }
                        return;
                    }
                }
                let time = self.input_event_time(time_ms);
                // Scroll distance arrives in the composited frame's pixel
                // space, like pointer motion; wl_pointer.axis wants
                // surface-logical pixels. Same conversion PointerMotion
                // does, so a wheel and a drag move content by equal amounts
                // on a scaled surface.
                let (sx, sy) = self.last_reported_size.get(&surface_id).map_or(
                    (1.0, 1.0),
                    |&(cw, ch, lw, lh)| {
                        (
                            if cw > 0 {
                                f64::from(lw) / f64::from(cw)
                            } else {
                                1.0
                            },
                            if ch > 0 {
                                f64::from(lh) / f64::from(ch)
                            } else {
                                1.0
                            },
                        )
                    },
                );
                let (dx, dy) = (dx * sx, dy * sy);
                if !stop && dx == 0.0 && dy == 0.0 && v120_x == 0 && v120_y == 0 {
                    return;
                }
                let focused_wl = self
                    .surfaces
                    .values()
                    .find(|s| Some(s.wl_surface.id()) == self.pointer_entered_id)
                    .map(|s| s.wl_surface.clone());
                let Some(wl) = focused_wl else {
                    if self.verbose {
                        eprintln!(
                            "[axis-drop] entered surface is no longer live: {}",
                            self.axis_target_state(surface_id)
                        );
                    }
                    return;
                };
                use wl_pointer::Axis;
                // Cloned out of `self.pointers` so the per-client axis
                // scale below can take `&mut self` for its cache.
                let targets: Vec<WlPointer> = self
                    .pointers
                    .iter()
                    .filter(|p| same_client(*p, &wl))
                    .cloned()
                    .collect();
                for ptr in &targets {
                    let v = ptr.version();
                    let scale = self.smooth_axis_scale(ptr);
                    // axis_source first: it applies to every axis event in
                    // this frame, and tells the client whether to expect
                    // detents or a smooth stream. Without it a trackpad's
                    // pixel deltas read as wheel clicks and toolkits scale
                    // them up by a lines-per-click factor.
                    if v >= 5
                        && let Some(src) = source.and_then(|s| axis_source_from_wire(s, v))
                    {
                        ptr.axis_source(src);
                    }
                    for (axis, delta, v120) in [
                        (Axis::VerticalScroll, dy, v120_y),
                        (Axis::HorizontalScroll, dx, v120_x),
                    ] {
                        if stop {
                            if v >= 5 {
                                ptr.axis_stop(time, axis);
                            }
                            continue;
                        }
                        if delta == 0.0 && v120 == 0 {
                            continue;
                        }
                        // value120 replaces axis_discrete at v8; neither may
                        // carry zero, and each must be followed by exactly
                        // one axis event on the same axis in this frame.
                        // Sub-detent travel has no axis_discrete spelling,
                        // so pre-v8 clients get it as smooth motion only.
                        if v120 != 0 {
                            if v >= 8 {
                                ptr.axis_value120(axis, i32::from(v120));
                            } else if v >= 5 && v120.abs() >= 120 {
                                ptr.axis_discrete(axis, i32::from(v120 / 120));
                            }
                        }
                        ptr.axis(time, axis, delta * scale);
                    }
                    if v >= 5 {
                        ptr.frame();
                    }
                }
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::SetTouchEnabled { enabled } => {
                self.set_touch_enabled(enabled);
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::Touch {
                owner_id,
                surface_id,
                phase,
                time_ms,
                contacts,
            } => {
                self.schedule_touch(owner_id, surface_id, phase, time_ms, contacts);
            }
            CompositorCommand::SurfaceResize {
                surface_id,
                width,
                height,
                scale_120,
            } => {
                // The browser sends physical pixels (cssW × DPR).  Convert
                // to logical (CSS) pixels for use in Wayland configures.
                let s_in = (scale_120 as i32).max(120);
                let w = (width as i32) * 120 / s_in;
                let h = (height as i32) * 120 / s_in;
                self.surface_sizes.insert(surface_id, (w, h));

                // Density belongs to this surface alone.  It used to be one
                // number for the whole session, so a viewer changing its DPR
                // — or merely switching which pane it was looking at —
                // reconfigured every other app in the session and made them
                // all repaint.
                let scale_changed =
                    scale_120 > 0 && scale_120 != self.surface_scale_120(surface_id);
                if scale_changed {
                    self.surface_scales.insert(surface_id, scale_120.max(120));
                }
                // The output this surface is on is its own display: its mode
                // follows the pane, its scale follows the viewer.  Re-announce
                // scale + mode before the configure, so the client sees the
                // display change first and picks a buffer scale to match.
                self.refresh_output_for_surface(surface_id);

                if let Some(root_id) = self.toplevel_surface_ids.get(&surface_id)
                    && let Some(surf) = self.surfaces.get(root_id)
                {
                    let (cw, ch) = constrain_to_hints(surf, w, h);
                    if let Some(ref tl) = surf.xdg_toplevel {
                        tl.configure(cw, ch, pane_states(surf.xdg_fullscreen));
                    }
                    if let Some(ref xs) = surf.xdg_surface {
                        let serial = self.serial.wrapping_add(1);
                        self.serial = serial;
                        xs.configure(serial);
                    }
                }
                self.fire_frame_callbacks_for_toplevel(surface_id, None);

                if scale_changed {
                    // Pointer coordinates are expressed in this surface's
                    // scale, so the enter has to be reissued — but only for
                    // the surface whose scale moved.
                    if self.pointer_entered_id.as_ref()
                        == self.toplevel_surface_ids.get(&surface_id)
                    {
                        self.pointer_entered_id = None;
                    }
                    self.pending_kb_reenter = true;
                }

                // The composite target moved the moment `surface_sizes`
                // (and possibly the output scale) changed above, so report
                // the new native size now instead of waiting for the client
                // to paint.  The server sizes each viewer's encoder by
                // inscribing this aspect into that viewer's box: reporting
                // it a round trip late meant the first encoder built after
                // every resize was inscribed into the *previous* aspect,
                // and the composite got squeezed into it (a 1200x1000 pane
                // encoded at 1200x674).  A second encoder then replaced it
                // — one wasted rebuild, one wasted keyframe, and a visibly
                // squashed picture in between.
                //
                // Only this surface's composite moved: scale is per-surface,
                // so a density change no longer rescales the session.
                if let Some(dims) = self.native_composite_size(surface_id) {
                    self.pending_native_sizes.insert(surface_id, dims);
                }
                self.flush_pending_commits();

                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::SurfaceFocus { surface_id } => {
                self.set_keyboard_focus(surface_id);
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::SurfaceClose { surface_id } => {
                if let Some(root_id) = self.toplevel_surface_ids.get(&surface_id)
                    && let Some(surf) = self.surfaces.get(root_id)
                    && let Some(ref tl) = surf.xdg_toplevel
                {
                    tl.close();
                }
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::ClipboardOffer { mime_type, data } => {
                self.external_clipboard = Some(ExternalClipboard { mime_type, data });
                // Tell the previous selection owner it's no longer selected.
                // Without this, apps that set their own selection keep
                // thinking they're the owner and paste from their internal
                // buffer on Ctrl+V instead of requesting data from the new
                // offer we're about to advertise.
                if let Some(src) = self.selection_source.take() {
                    src.cancelled();
                }
                self.offer_clipboard_selection();
                let _ = self
                    .event_tx
                    .send(CompositorEvent::ClipboardOwner { wayland: false });
                (self.event_notify)();
            }
            CompositorCommand::DragEnter {
                surface_id,
                x,
                y,
                mimes,
                planned_uri_list,
            } => {
                self.drag_enter(surface_id, x, y, &mimes, planned_uri_list);
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::DragMotion { surface_id, x, y } => {
                if self.drag.is_some()
                    && let Some((_, lx, ly)) = self.drag_target(surface_id, x, y)
                    && let Some(ref drag) = self.drag
                {
                    drag.device.motion(elapsed_ms(), lx, ly);
                }
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::DragLeave => {
                self.drag_end(true);
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::DragDrop {
                surface_id,
                x,
                y,
                offers,
            } => {
                self.drag_drop(surface_id, x, y, offers);
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::DragCancel => {
                self.drag_end(true);
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::PrimaryOffer { mime_type, data } => {
                self.external_primary = Some(ExternalClipboard { mime_type, data });
                // Same reasoning as ClipboardOffer: a client that still
                // believes it owns PRIMARY answers `receive` from its own
                // buffer, so the displaced owner has to be told.
                if let Some(src) = self.primary_source.take() {
                    src.cancelled();
                }
                self.offer_primary_selection();
            }
            CompositorCommand::Capture {
                surface_id,
                scale_120,
                reply,
            } => {
                // Use the capture-specific scale if provided, otherwise
                // fall back to the current output scale.
                let cap_s120 = if scale_120 > 0 {
                    scale_120
                } else {
                    self.surface_scale_120(surface_id)
                };
                let result = if let Some(root_id) = self.toplevel_surface_ids.get(&surface_id) {
                    if let Some(ref mut vk) = self.vulkan_renderer {
                        // Capture asks for the compositor's native
                        // composite at `cap_s120`.  No external targets
                        // are registered for capture, so the renderer
                        // returns 0..1 results — pick the first (or
                        // None if the render failed to produce
                        // anything).
                        // Capture is the on-demand CPU-pixel consumer: ask
                        // for the native BGRA, which the NVENC zero-copy
                        // path otherwise leaves unpublished.
                        vk.request_native_bgra();
                        // Capture failing for every surface at once is the
                        // clearest symptom of a dead renderer, and "not found
                        // or has no buffer" sends whoever sees it looking at
                        // the app instead.  Name the real reason here.
                        if vk.gpu_unrecoverable() {
                            eprintln!(
                                "[capture] surface {surface_id}: the GPU renderer stopped after \
                                 repeated submit failures, so there is nothing to composite — \
                                 restart the server.",
                            );
                        }
                        let readable = |v: Vec<(u16, u32, u32, PixelData, bool)>| {
                            // Take the first result we can actually read. A
                            // zero-copy NV12 handle may be in here too, and
                            // `to_rgba` is empty for it by construction —
                            // it is GPU-only memory.  encoder_skip is
                            // irrelevant here: capture IS the CPU-pixel
                            // consumer the flag protects.
                            v.into_iter().find_map(|(_sid, w, h, pixels, _skip)| {
                                let rgba = pixels.to_rgba(w, h);
                                (!rgba.is_empty()).then_some((w, h, rgba))
                            })
                        };
                        let (_, rendered) = vk.render_tree_sized(
                            root_id,
                            &self.surfaces,
                            &self.surface_meta,
                            cap_s120,
                            None,
                            surface_id,
                        );
                        let mut captured = readable(rendered);
                        // That render publishes the *previous* submit's
                        // readback, which normally satisfies capture. When
                        // there was no submit in flight — or the zero-copy
                        // path left the last one unpublished — wait for the
                        // one we just queued instead of reporting no buffer.
                        if captured.is_none() {
                            let deadline =
                                std::time::Instant::now() + std::time::Duration::from_millis(300);
                            while captured.is_none() && std::time::Instant::now() < deadline {
                                std::thread::sleep(std::time::Duration::from_millis(2));
                                let (_native, results) = vk.try_retire_pending();
                                captured = readable(results);
                            }
                        }
                        // Capture registers no external targets, so this is
                        // normally empty; drain anyway so a stray bitstream
                        // can't leak into the next flush.
                        let stray = vk.take_encoded_frames();
                        debug_assert!(stray.is_empty());
                        captured
                    } else {
                        None
                    }
                } else {
                    None
                };
                let _ = reply.send(result);
            }
            CompositorCommand::RequestFrame {
                surface_id,
                presentation_at,
            } => {
                // Remember that the server paced this surface, so the eager
                // per-commit fire in `handle_surface_commit` can stand down
                // while this (display-rate-throttled) path is driving frames.
                self.last_request_frame_ms.insert(surface_id, elapsed_ms());
                if self.toplevel_surface_ids.contains_key(&surface_id)
                    && !self.fire_frame_callbacks_for_toplevel(surface_id, Some(presentation_at))
                {
                    // The client's next frame request has not reached us
                    // yet. Latch this deadline; its associated commit will
                    // consume it instead of losing a complete refresh.
                    self.pending_request_frames
                        .insert(surface_id, presentation_at);
                }
            }
            CompositorCommand::SetScreenCastActive { surface_id, active } => {
                if active {
                    self.screencast_surfaces.insert(surface_id);
                    if let Some(root_id) = self.toplevel_surface_ids.get(&surface_id).cloned() {
                        self.composite_toplevel_into_pending(&root_id, surface_id, false);
                    }
                } else {
                    self.screencast_surfaces.remove(&surface_id);
                }
            }
            // Server-side cleanup on disconnect, not a browser event: `0` takes
            // the compositor's own clock.
            CompositorCommand::ReleaseKeys { keycodes } => {
                let time = self.input_event_time(0);
                let focused_wl = self
                    .toplevel_surface_ids
                    .get(&self.focused_surface_id)
                    .and_then(|root_id| self.surfaces.get(root_id))
                    .map(|s| s.wl_surface.clone());
                for keycode in &keycodes {
                    let serial = self.next_serial();
                    for kb in &self.keyboards {
                        if let Some(ref wl) = focused_wl
                            && same_client(kb, wl)
                        {
                            kb.key(serial, time, *keycode, wl_keyboard::KeyState::Released);
                        }
                    }
                }
                // Update modifier state for any released modifier keys.
                for keycode in &keycodes {
                    self.update_and_send_modifiers(*keycode, false);
                }
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::ClipboardListMimes { reply } => {
                let mimes = self.collect_clipboard_mime_types();
                let _ = reply.send(mimes);
            }
            CompositorCommand::ClipboardGet { mime_type, reply } => {
                let data = self.get_clipboard_content(&mime_type);
                let _ = reply.send(data);
            }
            CompositorCommand::SetExternalOutputBuffers {
                surface_id,
                target_w,
                target_h,
                native_w,
                native_h,
                buffers,
            } => {
                let installed = !buffers.is_empty();
                if let Some(ref mut vk) = self.vulkan_renderer {
                    vk.set_external_output_buffers(
                        surface_id,
                        target_w,
                        target_h,
                        (native_w, native_h),
                        buffers,
                    );
                }
                // Populate the freshly-installed external buffer pool
                // from the most-recent committed surface state.  An
                // idle wayland client (steady-state UI after a resize)
                // doesn't volunteer another commit, so without this
                // re-blit the per-client encoder skips forever waiting
                // for `last_pixels[(sid, target)]` to appear.  The
                // re-composite is deferred until the GPU is idle —
                // calling `render_tree_sized` here while a previous
                // submit's fence is still pending would early-return
                // and skip the new submit entirely.
                if installed && self.toplevel_surface_ids.contains_key(&(surface_id as u16)) {
                    self.pending_recomposite_toplevels
                        .insert(surface_id as u16, false);
                }
            }
            CompositorCommand::Recomposite { surface_id } => {
                // Full recomposite (not encoder-only): the point is to
                // republish pixels the server no longer has.  `insert`
                // rather than `or_insert` so it upgrades a queued
                // encoder-only pass.
                if self.toplevel_surface_ids.contains_key(&surface_id) {
                    self.pending_recomposite_toplevels.insert(surface_id, false);
                }
            }
            CompositorCommand::RegisterDownscaleTarget {
                surface_id,
                target_w,
                target_h,
                native_w,
                native_h,
                want_nv12_opaque,
                want_cpu_pixels,
                opaque_is_444,
            } => {
                if let Some(ref mut vk) = self.vulkan_renderer {
                    vk.register_downscale_target(
                        surface_id,
                        target_w,
                        target_h,
                        (native_w, native_h),
                        want_nv12_opaque,
                        want_cpu_pixels,
                        opaque_is_444,
                    );
                }
                // See the SetExternalOutputBuffers handler above for
                // why we re-composite here.
                if self.toplevel_surface_ids.contains_key(&(surface_id as u16)) {
                    self.pending_recomposite_toplevels
                        .insert(surface_id as u16, false);
                }
            }
            CompositorCommand::SetXwaylandPid { pid } => {
                self.xwayland_pid = Some(pid);
                // The bridge usually connects after this arrives, but it is
                // spawned first and the two race, so re-judge whoever is
                // already here.
                let known: Vec<(ClientId, u32)> = self
                    .client_pids
                    .iter()
                    .map(|(client, &pid)| (client.clone(), pid))
                    .collect();
                for (client, pid) in known {
                    if self.descends_from_xwayland(pid) {
                        self.xwayland_clients.insert(client);
                    }
                }
            }
            CompositorCommand::RestampTarget {
                surface_id,
                target_w,
                target_h,
                native_w,
                native_h,
            } => {
                if let Some(ref mut vk) = self.vulkan_renderer {
                    vk.restamp_target(surface_id, target_w, target_h, (native_w, native_h));
                }
            }
            CompositorCommand::ClearDownscaleTarget {
                surface_id,
                target_w,
                target_h,
            } => {
                if let Some(ref mut vk) = self.vulkan_renderer {
                    vk.clear_downscale_target(surface_id, target_w, target_h);
                }
            }
            CompositorCommand::SetRefreshRate { mhz } => {
                // Only update on meaningful changes (2 Hz or larger) to
                // avoid flooding clients with mode events from jittery
                // requestAnimationFrame measurements.  Two hertz itself is
                // meaningful at high refresh rates: a busy startup sample
                // can report 143 Hz for a 145 Hz display, then recover when
                // the short client probe runs again.
                let diff = (mhz as i64 - self.output_refresh_mhz as i64).unsigned_abs();
                if diff >= 2000 && mhz > 0 {
                    self.output_refresh_mhz = mhz;
                    // Refresh is the one output property that really is
                    // shared: it comes from the fastest connected display,
                    // and every surface's mode carries it.
                    for out in &self.outputs {
                        self.send_output_properties(&out.resource, out.slot);
                    }
                    let _ = self.display_handle.flush_clients();
                }
            }
            CompositorCommand::SetVulkanEncoder {
                surface_id,
                client_id,
                codec,
                qp,
                width,
                height,
                native_w,
                native_h,
                is_444,
            } => {
                let created = self.vulkan_renderer.as_mut().is_some_and(|vk| {
                    vk.create_vulkan_encoder(
                        surface_id, client_id, codec, qp, width, height, native_w, native_h, is_444,
                    )
                });
                if !created {
                    let _ = self
                        .event_tx
                        .send(CompositorEvent::VulkanEncoderUnavailable {
                            surface_id: surface_id as u16,
                            client_id,
                            after_encode_failures: false,
                        });
                    (self.event_notify)();
                }
            }
            CompositorCommand::SetVulkanEncoderQp {
                surface_id,
                client_id,
                qp,
            } => {
                if let Some(ref mut vk) = self.vulkan_renderer {
                    vk.set_vulkan_encoder_qp(surface_id, client_id, qp);
                }
            }
            CompositorCommand::RequestVulkanFrame {
                surface_id,
                client_id,
            } => {
                if let Some(vk) = self.vulkan_renderer.as_mut() {
                    vk.request_vulkan_frame(surface_id, client_id);
                }
            }
            CompositorCommand::RequestVulkanKeyframe {
                surface_id,
                client_id,
            } => {
                if let Some(ref mut vk) = self.vulkan_renderer {
                    vk.request_encoder_keyframe(surface_id, client_id);
                }
                // The latch is consumed by the next encode, and encodes only
                // happen when this toplevel is composited.  A surface whose
                // app has stopped painting would never composite again, so
                // the keyframe — and any quantizer change staged with it —
                // would wait forever.  Queue a recomposite; the drain runs it
                // once the GPU pipeline is idle.  Encoder-only: the content
                // is unchanged, so republishing the pixels would just make
                // every other viewer re-encode the frame it already has.
                if self.toplevel_surface_ids.contains_key(&(surface_id as u16)) {
                    self.pending_recomposite_toplevels
                        .entry(surface_id as u16)
                        .or_insert(true);
                }
            }
            CompositorCommand::DestroyVulkanEncoder {
                surface_id,
                client_id,
            } => {
                if let Some(ref mut vk) = self.vulkan_renderer {
                    vk.destroy_vulkan_encoder(surface_id, client_id);
                }
            }
            CompositorCommand::Shutdown => {
                self.shutdown.store(true, Ordering::Relaxed);
                self.loop_signal.stop();
            }
        }
    }

    /// Send dmabuf feedback events on a `ZwpLinuxDmabufFeedbackV1` object.
    /// Builds the format table from the Vulkan renderer's supported modifiers,
    /// then sends main_device, one tranche, and done.
    fn send_dmabuf_feedback(&self, fb: &ZwpLinuxDmabufFeedbackV1) {
        use std::os::unix::fs::MetadataExt;

        // Collect format+modifier pairs from the Vulkan renderer.
        let modifiers: &[(u32, u64)] = self
            .vulkan_renderer
            .as_ref()
            .map(|vk| vk.supported_dmabuf_modifiers.as_slice())
            .unwrap_or(&[]);

        // Build the format table: tightly packed (u32 format, u32 pad, u64 modifier).
        let entry_size = 16usize;
        let table_size = modifiers.len() * entry_size;
        let mut table_data = vec![0u8; table_size];
        for (i, &(fmt, modifier)) in modifiers.iter().enumerate() {
            let off = i * entry_size;
            table_data[off..off + 4].copy_from_slice(&fmt.to_ne_bytes());
            // 4 bytes padding (already zero)
            table_data[off + 8..off + 16].copy_from_slice(&modifier.to_ne_bytes());
        }

        // Create a memfd for the format table.
        let name = c"dmabuf-feedback-table";
        let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if raw_fd < 0 {
            eprintln!("[compositor] memfd_create for dmabuf feedback failed");
            fb.done();
            return;
        }
        let table_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        if !table_data.is_empty() {
            use std::io::Write;
            let mut file = std::fs::File::from(table_fd.try_clone().unwrap());
            if file.write_all(&table_data).is_err() {
                eprintln!("[compositor] failed to write dmabuf feedback table");
                fb.done();
                return;
            }
        }
        fb.format_table(table_fd.as_fd(), table_size as u32);

        // Get dev_t for the GPU device.
        let dev = std::fs::metadata(&self.gpu_device)
            .map(|m| m.rdev())
            .unwrap_or(0);
        let dev_bytes = dev.to_ne_bytes().to_vec();
        fb.main_device(dev_bytes.clone());

        // Single tranche with all format+modifier pairs.
        fb.tranche_target_device(dev_bytes);

        // Indices into the format table (array of u16 in native endianness).
        let indices: Vec<u8> = (0..modifiers.len() as u16)
            .flat_map(|i| i.to_ne_bytes())
            .collect();
        fb.tranche_formats(indices);

        fb.tranche_flags(
            wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags::empty(),
        );
        fb.tranche_done();
        fb.done();
    }
}

impl Compositor {
    /// Queue one transport touch frame at the wall-clock cadence of the
    /// browser events that produced it.
    ///
    /// `due = max(previous_due + browser_delta, arrival)` has two useful
    /// properties. A live stream is never delayed just to match a prediction,
    /// while a burst that arrived late is replayed from its first event at the
    /// original spacing instead of being flattened into one compositor pass.
    fn schedule_touch(
        &mut self,
        owner_id: u64,
        surface_id: u16,
        phase: TouchPhase,
        time_ms: u32,
        contacts: Vec<TouchPoint>,
    ) {
        self.touch_pacer.push(
            std::time::Instant::now(),
            owner_id,
            surface_id,
            phase,
            time_ms,
            contacts,
        );
    }

    fn dispatch_due_touches(&mut self) {
        let Some(touch) = self.touch_pacer.pop_due(std::time::Instant::now()) else {
            return;
        };
        self.handle_touch(
            touch.owner_id,
            touch.surface_id,
            touch.phase,
            touch.time_ms,
            &touch.contacts,
        );
        let _ = self.display_handle.flush_clients();
    }

    fn next_touch_deadline(&self) -> Option<std::time::Instant> {
        self.touch_pacer.next_deadline()
    }

    /// The seat's advertised device set.  One definition for both the initial
    /// `bind` and every later update: a client that binds `wl_seat` before the
    /// first direct-touch viewer connects must not see a different device set
    /// from one that binds after.
    fn seat_capabilities(&self) -> wl_seat::Capability {
        let mut capabilities = wl_seat::Capability::Keyboard | wl_seat::Capability::Pointer;
        if self.touch_enabled {
            capabilities |= wl_seat::Capability::Touch;
        }
        capabilities
    }

    fn set_touch_enabled(&mut self, enabled: bool) {
        if self.touch_enabled == enabled {
            return;
        }
        if !enabled {
            self.cancel_touch_owner(None);
        }
        self.touch_enabled = enabled;
        let capabilities = self.seat_capabilities();
        for seat in &self.seats {
            seat.capabilities(capabilities);
        }
    }

    fn allocate_touch_id(&self) -> i32 {
        // Wayland only requires ids to stay unique while their contacts are
        // live. Reuse the lowest free slot, as a physical touchscreen does.
        // Chromium's gesture stack accepts at most 32 pointer ids; handing it
        // a fresh seat-global id forever makes every app lose fling recognition
        // after the compositor's 32nd gesture even though dragging still works.
        (0..i32::MAX)
            .find(|id| {
                !self
                    .active_touches
                    .values()
                    .any(|active| active.wayland_id == *id)
            })
            .expect("all Wayland touch ids are live")
    }

    fn touch_frame_target(targets: &mut Vec<WlTouch>, touch: &WlTouch) {
        if !targets.iter().any(|target| target.id() == touch.id()) {
            targets.push(touch.clone());
        }
    }

    /// Retire an owner's touch sequence (`None` = every owner).
    ///
    /// Also ends a touch-started drag belonging to that owner, and tells the
    /// server so it stops treating the owner as holding a live sequence.
    fn cancel_touch_owner(&mut self, owner: Option<u64>) {
        // A down may still be waiting for its paced delivery. It has not
        // reached `active_touches` yet, but the server has already granted its
        // browser owner the sequence lock and must be told if we discard it.
        let had_pending = self.touch_pacer.has_contacts(owner);
        self.touch_pacer.clear(owner);
        // The drag is checked independently of `active_touches`: `start_drag`
        // cancels the sequence and empties that map, so during a touch drag the
        // grab is the only remaining trace of the contact.
        //
        // Only a *live* drag, though. After a valid drop the session stays alive
        // waiting for the target's `finish`, and cancelling there would send
        // `wl_data_source.cancelled` after `dnd_drop_performed` — the source app
        // reads its completed drop as failed and a Move never deletes the
        // original.
        let cancel_drag = self
            .client_touch_drag_contact()
            .is_some_and(|grab| owner.is_none_or(|owner| owner == grab.owner_id));
        let had_contacts = self
            .active_touches
            .keys()
            .any(|(active_owner, _)| owner.is_none_or(|owner| owner == *active_owner));
        if !cancel_drag && !had_contacts && !had_pending {
            self.reset_touch_clock(owner);
            return;
        }
        if cancel_drag {
            self.client_drag_cancel(true);
        }
        if had_contacts {
            self.retire_touch_contacts(owner);
        }
        self.reset_touch_clock(owner);
        // Tell the server, or it keeps this owner registered as holding a live
        // sequence and refuses every other viewer's contacts until that
        // browser's fingers happen to lift.
        let _ = self
            .event_tx
            .send(CompositorEvent::TouchCancelled { owner_id: owner });
        (self.event_notify)();
    }

    /// Send `wl_touch.cancel` for an owner's contacts and forget them.
    ///
    /// `cancel` is defined over the whole sequence, so this is all-or-nothing
    /// per client — which is exactly what a seat hand-off means. It does not
    /// touch `client_drag`: `start_drag` calls it to hand the seat to a drag it
    /// is in the middle of establishing.
    fn retire_touch_contacts(&mut self, owner: Option<u64>) {
        let targets: Vec<WlSurface> = self
            .active_touches
            .iter()
            .filter(|((active_owner, _), _)| owner.is_none_or(|owner| owner == *active_owner))
            .map(|(_, active)| active.target.clone())
            .collect();
        if targets.is_empty() {
            return;
        }
        self.active_touches
            .retain(|(active_owner, _), _| owner.is_some_and(|owner| owner != *active_owner));
        for touch in &self.touches {
            if targets.iter().any(|target| same_client(touch, target)) {
                touch.cancel();
            }
        }
    }

    fn touch_local_position(
        &self,
        surface_id: u16,
        target_id: &ObjectId,
        x: f64,
        y: f64,
    ) -> Option<(f64, f64)> {
        let (root_id, x, y) = self.frame_to_surface_tree(surface_id, x, y)?;
        let (target_root, _) = self.find_toplevel_root(target_id);
        if root_id != target_root {
            return None;
        }
        let (offset_x, offset_y) = self.surface_absolute_position(target_id);
        Some((x - f64::from(offset_x), y - f64::from(offset_y)))
    }

    /// Turn a browser event's `timeStamp` into a seat input timestamp.
    ///
    /// Apps differentiate position against `wl_pointer.motion` / `wl_touch.time`
    /// / `wl_pointer.axis` to get a velocity — a fling, a stroke width, a swipe.
    /// So the *spacing* between events has to be the browser's. It cannot be the
    /// browser's clock outright, because these timestamps share one millisecond
    /// domain across the whole seat, so the client's epoch is anchored to ours and
    /// its deltas ride on top.
    ///
    /// Reading our own clock per event instead is what broke inertial scrolling:
    /// the command queue is drained in one pass, so a burst of coalesced moves all
    /// landed on the same instant and every velocity came out as a division by
    /// zero.
    fn input_event_time(&mut self, client_time_ms: u32) -> u32 {
        let now = elapsed_ms();
        // No browser event behind this one — the legacy axis opcode, an IME commit's
        // synthesised keys, disconnect cleanup. Take our own clock, but leave the
        // anchor alone: these interleave with real gestures (a chord synthesised
        // around a real keypress, a modifier during a drag), and dropping the
        // anchor there would restart the pacing of the gesture around them.
        if client_time_ms == 0 {
            let time = now.max(self.last_input_time.unwrap_or(now));
            self.last_input_time = Some(time);
            return time;
        }
        // Re-anchor after an idle gap. Within a gesture the browser's spacing is
        // exact; across gestures a stale anchor would accumulate the drift
        // between the two clocks, and nothing needs continuity across a pause.
        const REANCHOR_IDLE_MS: u32 = 200;
        const MAX_DRIFT_MS: u32 = 60_000;
        if let Some((anchor_local, _)) = self.input_time_anchor
            && now > anchor_local
            && now - anchor_local > MAX_DRIFT_MS
        {
            self.input_time_anchor = None;
        }
        // `>` first: a batched event can legitimately sit slightly ahead of `now`,
        // and a wrapping subtraction there would look like a huge idle gap and
        // re-anchor on every event of the burst.
        if let Some(last) = self.last_input_time
            && now > last
            && now - last > REANCHOR_IDLE_MS
        {
            self.input_time_anchor = None;
        }
        let (anchor_local, anchor_client) =
            *self.input_time_anchor.get_or_insert((now, client_time_ms));
        // The client's epoch is its own and is not trusted: a wrap, a clock step,
        // or a bad actor must not send time backwards (which clients may assert
        // on) or into the future (which would starve a fling of samples).
        let elapsed = client_time_ms.wrapping_sub(anchor_client);
        let time = if elapsed <= MAX_DRIFT_MS {
            anchor_local.wrapping_add(elapsed)
        } else {
            now
        };
        // Not clamped to `now`: a batch of events that were generated before they
        // arrived is legitimately spread across the instant we drain it, and
        // clamping each to `now` would flatten the very spacing this exists to
        // keep. Unbounded futureness is the real hazard, so a client whose clock
        // runs fast simply re-anchors once it gets too far ahead.
        const MAX_AHEAD_MS: u32 = 1_000;
        let time = if time.wrapping_sub(now) > MAX_AHEAD_MS && time > now {
            self.input_time_anchor = Some((now, client_time_ms));
            now
        } else {
            time
        };
        // Monotonic: clients may assert on time going backwards.
        let time = time.max(self.last_input_time.unwrap_or(time));
        self.last_input_time = Some(time);
        time
    }

    /// Map one direct-touch owner's browser clock into the shared seat domain.
    ///
    /// Pointer, key, and axis commands may come from a different connected
    /// browser. Their DOM `timeStamp` epochs are unrelated, so direct touch
    /// cannot reuse the generic input anchor. The one-viewer touch lock ensures
    /// there is at most one live touch owner; the owner id still matters across
    /// back-to-back sequences from different viewers.
    fn touch_event_time(&mut self, owner_id: u64, client_time_ms: u32) -> u32 {
        if client_time_ms == 0 {
            return self.input_event_time(0);
        }

        let now = elapsed_ms();
        const REANCHOR_IDLE_MS: u32 = 200;
        const MAX_DRIFT_MS: u32 = 60_000;
        const MAX_AHEAD_MS: u32 = 1_000;

        if self
            .touch_time_anchor
            .is_some_and(|(owner, _, _)| owner != owner_id)
        {
            self.touch_time_anchor = None;
            self.touch_time_last_arrival = None;
        }
        if let Some((_, anchor_local, _)) = self.touch_time_anchor
            && now > anchor_local
            && now - anchor_local > MAX_DRIFT_MS
        {
            self.touch_time_anchor = None;
        }
        if let Some(last_arrival) = self.touch_time_last_arrival
            && now > last_arrival
            && now - last_arrival > REANCHOR_IDLE_MS
        {
            self.touch_time_anchor = None;
        }
        self.touch_time_last_arrival = Some(now);

        // Start at the seat's last emitted time when another input batch is
        // slightly ahead of our clock. That keeps the seat monotonic without
        // flattening this sequence's following browser-time deltas.
        let local_base = now.max(self.last_input_time.unwrap_or(now));
        let (_, anchor_local, anchor_client) =
            *self
                .touch_time_anchor
                .get_or_insert((owner_id, local_base, client_time_ms));
        let elapsed = client_time_ms.wrapping_sub(anchor_client);
        let mut time = if elapsed <= MAX_DRIFT_MS {
            anchor_local.wrapping_add(elapsed)
        } else {
            self.touch_time_anchor = Some((owner_id, local_base, client_time_ms));
            local_base
        };
        if time > now && time.wrapping_sub(now) > MAX_AHEAD_MS {
            self.touch_time_anchor = Some((owner_id, local_base, client_time_ms));
            time = local_base;
        }
        time = time.max(self.last_input_time.unwrap_or(time));
        self.last_input_time = Some(time);
        time
    }

    fn reset_touch_clock(&mut self, owner: Option<u64>) {
        if owner.is_none_or(|owner| {
            self.touch_time_anchor
                .is_some_and(|(active_owner, _, _)| active_owner == owner)
        }) {
            self.touch_time_anchor = None;
            self.touch_time_last_arrival = None;
        }
    }

    fn handle_touch(
        &mut self,
        owner_id: u64,
        surface_id: u16,
        phase: TouchPhase,
        client_time_ms: u32,
        contacts: &[TouchPoint],
    ) {
        if phase == TouchPhase::Cancel {
            self.cancel_touch_owner(Some(owner_id));
            return;
        }
        if !self.touch_enabled {
            return;
        }

        let time = self.touch_event_time(owner_id, client_time_ms);
        let mut framed = Vec::new();
        for point in contacts {
            let key = (owner_id, point.id);
            match phase {
                TouchPhase::Down => {
                    if self.active_touches.contains_key(&key) {
                        continue;
                    }
                    // A touch-started DnD installs a seat-wide touch grab.
                    // New contacts are swallowed until its initiating
                    // contact lifts, matching a physical compositor.
                    if self.client_touch_drag_active() {
                        continue;
                    }
                    let Some((target, lx, ly)) = self.drag_target(surface_id, point.x, point.y)
                    else {
                        continue;
                    };
                    let target_id = target.id();

                    // A touch outside a grabbed popup is spent dismissing the
                    // popup, just like the pointer press path.
                    if !self.popup_grab_stack.is_empty()
                        && !self.popup_grab_stack.contains(&target_id)
                    {
                        self.dismiss_popup_grabs();
                        continue;
                    }

                    let recipients: Vec<WlTouch> = self
                        .touches
                        .iter()
                        .filter(|touch| same_client(*touch, &target))
                        .cloned()
                        .collect();
                    if recipients.is_empty() {
                        continue;
                    }
                    let serial = self.next_serial();
                    let wayland_id = self.allocate_touch_id();
                    for touch in &recipients {
                        touch.down(serial, time, &target, wayland_id, lx, ly);
                        Self::touch_frame_target(&mut framed, touch);
                    }
                    self.active_touches.insert(
                        key,
                        ActiveTouch {
                            wayland_id,
                            target,
                            surface_id,
                            down_serial: serial,
                        },
                    );
                }
                TouchPhase::Motion => {
                    // Checked before `active_touches`, which `start_drag`
                    // emptied when it cancelled the sequence: the grab is the
                    // only remaining record of the contact driving the drag.
                    // Everything else was cancelled and is owed nothing.
                    if let Some(grab) = self.client_touch_drag_contact() {
                        if (grab.owner_id, grab.browser_id) == key {
                            self.client_drag_motion(grab.surface_id, point.x, point.y);
                        }
                        continue;
                    }
                    let Some(active) = self.active_touches.get(&key) else {
                        continue;
                    };
                    let wayland_id = active.wayland_id;
                    let target = active.target.clone();
                    let target_id = target.id();
                    let active_surface_id = active.surface_id;
                    let Some((lx, ly)) =
                        self.touch_local_position(active_surface_id, &target_id, point.x, point.y)
                    else {
                        continue;
                    };
                    for touch in &self.touches {
                        if same_client(touch, &target) {
                            touch.motion(time, wayland_id, lx, ly);
                            Self::touch_frame_target(&mut framed, touch);
                        }
                    }
                }
                TouchPhase::Up => {
                    // As in Motion: the grab outlives `active_touches`. Lifting
                    // the drag's contact completes the drop, and the client
                    // needs no `up` — it was told to forget the sequence when
                    // the drag took the seat.
                    if let Some(grab) = self.client_touch_drag_contact() {
                        if (grab.owner_id, grab.browser_id) == key {
                            self.client_drag_release();
                        }
                        continue;
                    }
                    let Some(active) = self.active_touches.remove(&key) else {
                        continue;
                    };
                    let serial = self.next_serial();
                    for touch in &self.touches {
                        if same_client(touch, &active.target) {
                            touch.up(serial, time, active.wayland_id);
                            Self::touch_frame_target(&mut framed, touch);
                        }
                    }
                }
                TouchPhase::Cancel => unreachable!(),
            }
        }
        for touch in framed {
            touch.frame();
        }
        if self.active_touches.is_empty() && self.client_touch_drag_contact().is_none() {
            self.reset_touch_clock(Some(owner_id));
        }
    }

    /// Collect all MIME types available on the current clipboard.
    fn collect_clipboard_mime_types(&self) -> Vec<String> {
        // If a Wayland app owns the selection, use its MIME types.
        if let Some(ref src) = self.selection_source {
            let data = src.data::<DataSourceData>().unwrap();
            return data.mime_types.lock().unwrap().clone();
        }
        // Otherwise use the external (browser/CLI) clipboard.
        if let Some(ref cb) = self.external_clipboard
            && !cb.mime_type.is_empty()
        {
            return cb.mime_types();
        }
        Vec::new()
    }

    /// Get clipboard content for a specific MIME type.
    fn get_clipboard_content(&mut self, mime_type: &str) -> Option<Vec<u8>> {
        // If external clipboard matches, return its data directly.
        if let Some(ref cb) = self.external_clipboard
            && self.selection_source.is_none()
        {
            // External clipboard is active.
            if cb.offers(mime_type) {
                return Some(cb.data.clone());
            }
            return None;
        }
        // If a Wayland app owns the selection, read from it via pipe.
        if let Some(src) = self.selection_source.clone() {
            return self.read_data_source_sync(&src, mime_type);
        }
        None
    }

    /// Synchronously read data from a Wayland data source via pipe.
    fn read_data_source_sync(&mut self, source: &WlDataSource, mime_type: &str) -> Option<Vec<u8>> {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return None;
        }
        let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        source.send(mime_type.to_string(), write_fd.as_fd());
        let _ = self.display_handle.flush_clients();
        drop(write_fd); // close write end so read gets EOF
        // Non-blocking read with a modest limit.
        unsafe {
            libc::fcntl(read_fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            let n = unsafe {
                libc::read(
                    read_fd.as_raw_fd(),
                    tmp.as_mut_ptr() as *mut libc::c_void,
                    tmp.len(),
                )
            };
            if n <= 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n as usize]);
            if buf.len() > 1024 * 1024 {
                break; // 1 MiB cap
            }
        }
        if buf.is_empty() { None } else { Some(buf) }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read `CLOCK_MONOTONIC` and return `(tv_sec, tv_nsec)`.
fn monotonic_timespec() -> (i64, i64) {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime with CLOCK_MONOTONIC is always valid.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec, ts.tv_nsec)
}

/// Convert an absolute `Instant` into the Wayland presentation clock domain.
/// On Linux `Instant` and `CLOCK_MONOTONIC` share a clock, but Rust keeps the
/// former opaque; sampling both once lets us preserve the server's scheduled
/// refresh phase instead of replacing it with compositor wake-up time.
fn monotonic_timespec_at(when: std::time::Instant) -> (i64, i64) {
    // Read CLOCK_MONOTONIC first and Instant second.  Reading them in the
    // opposite order maps `when` ahead by the time spent in clock_gettime:
    // the kernel clock has advanced but the Instant anchor has not.  Chromium
    // then sees a presentation a few dozen microseconds in its future and
    // reports negative frame latency.  This order makes the same unavoidable
    // sampling error conservative (slightly in the past).  A timer may still
    // wake a hair before its scheduled phase, so the result is capped at the
    // sampled clock below: presentation feedback must never describe an event
    // in the future.  Past deadlines retain their exact frame-clock phase.
    let (sec, nsec) = monotonic_timespec();
    let instant_now = std::time::Instant::now();
    let now_ns = (sec as u128)
        .saturating_mul(1_000_000_000)
        .saturating_add(nsec as u128);
    let scheduled_ns = if when <= instant_now {
        now_ns.saturating_sub(instant_now.duration_since(when).as_nanos())
    } else {
        now_ns.saturating_add(when.duration_since(instant_now).as_nanos())
    };
    let when_ns = scheduled_ns.min(now_ns);
    (
        (when_ns / 1_000_000_000) as i64,
        (when_ns % 1_000_000_000) as i64,
    )
}

fn elapsed_ms() -> u32 {
    // Use CLOCK_MONOTONIC directly so the timestamp matches what Wayland
    // clients (especially Chromium/Brave) expect for frame-latency
    // calculations.  The previous implementation measured from an arbitrary
    // epoch which caused Chromium to report negative frame latency.
    let (sec, nsec) = monotonic_timespec();
    (sec as u32)
        .wrapping_mul(1000)
        .wrapping_add(nsec as u32 / 1_000_000)
}

/// Read the same clock as [`elapsed_ms`] while retaining its sub-ms part.
fn elapsed_timestamp() -> (u32, u16) {
    let (sec, nsec) = monotonic_timespec();
    let ms = (sec as u32)
        .wrapping_mul(1000)
        .wrapping_add(nsec as u32 / 1_000_000);
    (ms, ((nsec as u32 % 1_000_000) / 1_000) as u16)
}

/// Map a scroll source onto `wl_pointer.axis_source`, for a pointer bound
/// at `version`.
///
/// The numbers are Wayland's own `axis_source` enum. Anything this build
/// does not recognise, or that the client's version predates, yields
/// `None`: an unclassified scroll is recoverable, one labelled wrong is
/// not, and an out-of-range enum value is a protocol error that would kill
/// the client.
fn axis_source_from_wire(source: u8, version: u32) -> Option<wl_pointer::AxisSource> {
    match source {
        0 => Some(wl_pointer::AxisSource::Wheel),
        1 => Some(wl_pointer::AxisSource::Finger),
        2 => Some(wl_pointer::AxisSource::Continuous),
        3 if version >= 6 => Some(wl_pointer::AxisSource::WheelTilt),
        _ => None,
    }
}

/// Pixels blit calls one wheel detent, matching `WHEEL_DETENT_PX` in the
/// browser client and the unit `blit surface scroll` takes.
const PX_PER_DETENT: f64 = 120.0;

/// Smooth `wl_pointer.axis` units per detent in the convention Weston
/// established and Mutter still emits.
const AXIS_UNITS_PER_DETENT: f64 = 10.0;

/// Files every Chromium build ships beside its executable. The process
/// name is no use here — Electron apps rename the binary — but the
/// runtime payload next to it is always there.
const CHROMIUM_SIBLINGS: [&str; 3] = [
    "chrome_crashpad_handler",
    "v8_context_snapshot.bin",
    "icudtl.dat",
];

/// Whether the process behind a Wayland connection is Chromium or an
/// Electron app, and so needs its scroll distance in detent units.
/// See [`Compositor::smooth_axis_scale`].
fn pid_is_chromium(pid: i32) -> bool {
    let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    let Some(dir) = exe.parent() else {
        return false;
    };
    if dir_is_chromium(dir) {
        return true;
    }
    // A sandboxed client — flatpak, snap — named its executable in its own
    // mount namespace, where that path means nothing to us. Re-root it.
    dir_is_chromium(
        &std::path::Path::new("/proc")
            .join(pid.to_string())
            .join("root")
            .join(dir.strip_prefix("/").unwrap_or(dir)),
    )
}

/// Split out from [`pid_is_chromium`] so the marker list is testable
/// without a live process.
fn dir_is_chromium(dir: &std::path::Path) -> bool {
    CHROMIUM_SIBLINGS.iter().any(|f| dir.join(f).exists())
}

/// Returns true when two Wayland resources belong to the same still-connected client.
/// What a `wl_output` global stands for: one screen, offered to one client,
/// holding at most one of that client's toplevels.
///
/// The parent of a live process, read from `/proc`.  `None` once it has
/// exited, and on anything that is not Linux-shaped.
fn parent_pid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))?
        .trim()
        .parse()
        .ok()
}

/// Wayland's own answer to "what density should I draw at" is the set of
/// outputs a surface has entered, so giving each toplevel its own output is
/// what lets two windows of the same application render at two different
/// densities — one watched on a laptop, the other on a phone. It also stops
/// `wl_output.mode` being a fold over every surface in the session, which
/// used to send a maximising app to the size of the *largest* other window.
///
/// `owner` keeps the global out of every other client's registry: an app
/// must not discover a monitor for a window it does not have.
///
/// The slot is indirect on purpose.  A global's data is immutable once
/// published, but a screen outlives the window on it: it is offered empty,
/// claimed at map, and emptied again at unmap.  The mutable half lives in
/// `Compositor::output_slots`, which the slot indexes.
#[derive(Clone)]
struct OutputGlobal {
    slot: u32,
    owner: ClientId,
}

/// A published screen: who may see it, the global to withdraw it with, and
/// the toplevel currently on it — `None` for one nobody has claimed.
struct OutputSlot {
    owner: ClientId,
    global: GlobalId,
    surface_id: Option<u16>,
}

/// A withdrawn output kept bindable until its owner disconnects.
struct RetiredOutputGlobal {
    owner: ClientId,
    global: GlobalId,
    slot: u32,
}

/// A bound `wl_output`, and the screen it speaks for.
struct SurfaceOutput {
    resource: WlOutput,
    slot: u32,
}

/// A `wp_fractional_scale_v1` and the `wl_surface` it was created for.  The
/// toplevel is resolved lazily: the object is usually created before the
/// surface has a role, and a subsurface answers with its root's scale.
struct SurfaceFractionalScale {
    resource: WpFractionalScaleV1,
    surface: ObjectId,
}

fn same_client<R1: Resource, R2: Resource>(a: &R1, b: &R2) -> bool {
    match (a.client(), b.client()) {
        (Some(ca), Some(cb)) => ca.id() == cb.id(),
        _ => false,
    }
}

/// Negotiate the drag-and-drop action after the destination's `set_actions`.
/// Its preferred action wins when it is in the intersection; otherwise Copy,
/// Move, then Ask provide a stable compositor preference.  An empty target
/// mask means it has not selected any action yet, not "accept anything".
fn negotiate_dnd_action(
    source: DndAction,
    offer: DndAction,
    preferred: DndAction,
) -> Option<DndAction> {
    if offer.is_empty() {
        return None;
    }
    let both = source & offer;
    if !preferred.is_empty() && both.contains(preferred) {
        Some(preferred)
    } else if both.contains(DndAction::Copy) {
        Some(DndAction::Copy)
    } else if both.contains(DndAction::Move) {
        Some(DndAction::Move)
    } else if both.contains(DndAction::Ask) {
        Some(DndAction::Ask)
    } else {
        None
    }
}

fn yuv420_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    // BT.601 limited-range inverse, matching the forward conversion
    // everywhere in blit (shaders and CPU paths).
    let y = (y as i32 - 16).max(0);
    let u = u as i32 - 128;
    let v = v as i32 - 128;
    let r = ((298 * y + 409 * v + 128) >> 8).clamp(0, 255) as u8;
    let g = ((298 * y - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u8;
    let b = ((298 * y + 516 * u + 128) >> 8).clamp(0, 255) as u8;
    [r, g, b]
}

/// The states a pane's configure carries.  It is always activated and always
/// maximized; `fullscreen` is added for a toplevel that asked to be, which
/// costs nothing because a pane already fills its output.
fn pane_states(fullscreen: bool) -> Vec<u8> {
    let mut states = vec![
        xdg_toplevel::State::Activated,
        xdg_toplevel::State::Maximized,
    ];
    if fullscreen {
        states.push(xdg_toplevel::State::Fullscreen);
    }
    xdg_toplevel_states(&states)
}

/// Fit a size we are about to quote in a configure into the range the client
/// said it can draw.  Zero in a dimension means it has no opinion there.
///
/// This changes the number we ask for, not the pane: the pane's size is the
/// viewer's layout and no client hint can move it.  What it buys is that we
/// stop asking for a size the client is only going to refuse -- xdg-shell lets
/// a compositor ignore these hints, but a configure the client will not honour
/// is a round trip that ends in a surface the wrong size either way.
fn constrain_to_hints(surf: &Surface, w: i32, h: i32) -> (i32, i32) {
    let fit = |v: i32, min: i32, max: i32| {
        let v = if min > 0 { v.max(min) } else { v };
        // Max is applied last, but they cannot disagree: a minimum above a
        // maximum is refused at commit.
        if max > 0 { v.min(max) } else { v }
    };
    (
        fit(w, surf.min_size.0, surf.max_size.0),
        fit(h, surf.min_size.1, surf.max_size.1),
    )
}

/// Encode xdg_toplevel states as the raw byte array expected by the protocol.
fn xdg_toplevel_states(states: &[xdg_toplevel::State]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(states.len() * 4);
    for state in states {
        bytes.extend_from_slice(&(*state as u32).to_ne_bytes());
    }
    bytes
}

/// Encode xdg_toplevel wm_capabilities the same way -- native-endian u32s.
fn xdg_wm_capabilities(caps: &[xdg_toplevel::WmCapabilities]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(caps.len() * 4);
    for cap in caps {
        bytes.extend_from_slice(&(*cap as u32).to_ne_bytes());
    }
    bytes
}

fn create_keymap_fd(keymap_data: &[u8]) -> Option<OwnedFd> {
    use std::io::Write;
    let name = c"blit-keymap";
    let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw_fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let mut file = std::fs::File::from(fd);
    file.write_all(keymap_data).ok()?;
    Some(file.into())
}

// ---------------------------------------------------------------------------
// Protocol dispatch implementations
// ---------------------------------------------------------------------------

// -- wl_compositor --

impl GlobalDispatch<WlCompositor, ()> for Compositor {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlCompositor>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WlCompositor, ()> for Compositor {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WlCompositor,
        request: <WlCompositor as Resource>::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wayland_server::protocol::wl_compositor::Request;
        match request {
            Request::CreateSurface { id } => {
                let surface = data_init.init(id, ());
                let proto_id = surface.id();
                state.surfaces.insert(
                    proto_id,
                    Surface {
                        surface_id: 0,
                        wl_surface: surface,
                        pending_buffer: None,
                        pending_buffer_scale: 1,
                        pending_damage: Vec::new(),
                        pending_frame_callbacks: Vec::new(),
                        pending_presentation_feedbacks: Vec::new(),
                        pending_opaque: false,
                        pending_input_region: None,
                        input_region: None,
                        map_state: MapState::Never,
                        buffer_scale: 1,
                        is_opaque: false,
                        pending_acquire_point: None,
                        pending_release_point: None,
                        syncobj_surface: None,
                        parent_surface_id: None,
                        pending_subsurface_position: None,
                        subsurface_position: (0, 0),
                        children: Vec::new(),
                        xdg_surface: None,
                        xdg_toplevel: None,
                        xdg_popup: None,
                        xdg_geometry: None,
                        xdg_fullscreen: false,
                        pending_min_size: (0, 0),
                        pending_max_size: (0, 0),
                        min_size: (0, 0),
                        max_size: (0, 0),
                        title: String::new(),
                        app_id: String::new(),
                        pending_viewport_destination: None,
                        viewport_destination: None,
                        pending_viewport_source: None,
                        viewport_source: None,
                        is_cursor: false,
                        cursor_hotspot: (0, 0),
                    },
                );
            }
            Request::CreateRegion { id } => {
                data_init.init(id, ());
            }
            _ => {}
        }
    }
}

// -- wl_surface --

impl Dispatch<WlSurface, ()> for Compositor {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WlSurface,
        request: <WlSurface as Resource>::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wayland_server::protocol::wl_surface::Request;
        let sid = resource.id();
        match request {
            Request::Attach { buffer, x, y } => {
                if let Some(surf) = state.surfaces.get_mut(&sid) {
                    surf.pending_buffer = Some(buffer);
                    if x != 0 || y != 0 {
                        surf.pending_damage.push(PendingDamage::Full);
                    }
                }
            }
            Request::Damage {
                x,
                y,
                width,
                height,
            } => {
                if let Some(surf) = state.surfaces.get_mut(&sid) {
                    surf.pending_damage.push(PendingDamage::Surface {
                        x,
                        y,
                        width,
                        height,
                    });
                }
            }
            Request::DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                if let Some(surf) = state.surfaces.get_mut(&sid) {
                    surf.pending_damage.push(PendingDamage::Buffer {
                        x,
                        y,
                        width,
                        height,
                    });
                }
            }
            Request::Frame { callback } => {
                let cb = data_init.init(callback, ());
                if let Some(surf) = state.surfaces.get_mut(&sid) {
                    surf.pending_frame_callbacks.push(cb);
                }
                if let Some(toplevel_sid) = state.find_toplevel_root(&sid).1 {
                    state.frame_callback_toplevels.insert(toplevel_sid);
                }
            }
            Request::SetBufferScale { scale } => {
                if let Some(surf) = state.surfaces.get_mut(&sid) {
                    surf.pending_buffer_scale = scale;
                }
            }
            Request::SetOpaqueRegion { region: _ } => {
                if let Some(surf) = state.surfaces.get_mut(&sid) {
                    surf.pending_opaque = true;
                }
            }
            Request::SetInputRegion { region } => {
                // Double-buffered: takes effect on the next commit. An empty
                // list is meaningfully different from nil — nil restores the
                // default of accepting input everywhere, an empty region
                // declines it everywhere.
                let ops = region.map(|r| {
                    state
                        .regions
                        .get(&r.id())
                        .map(|(_, ops)| ops.clone())
                        .unwrap_or_default()
                });
                if let Some(surf) = state.surfaces.get_mut(&sid) {
                    surf.pending_input_region = Some(ops);
                }
            }
            Request::Commit => {
                let is_cursor = state.surfaces.get(&sid).is_some_and(|s| s.is_cursor);
                if is_cursor {
                    state.handle_cursor_commit(&sid);
                } else {
                    state.handle_surface_commit(&sid);
                }
            }
            Request::SetBufferTransform { .. } => {}
            Request::Offset { .. } => {}
            Request::Destroy => {
                state.detach_toplevel_drag_surface(&sid);
                state.surface_meta.remove(&sid);
                state.cursor_rgba.remove(&sid);
                if let Some(ref mut vk) = state.vulkan_renderer {
                    vk.remove_surface(&sid);
                }
                if let Some(held) = state.held_buffers.remove(&sid) {
                    state.release_held(held);
                }
                if let Some(a) = state.awaiting_acquire.remove(&sid) {
                    let a = a.into_release();
                    state.release_held(a);
                }
                state.forget_pointer_focus(&sid);
                let client_drag_target_dropped = state
                    .client_drag
                    .as_ref()
                    .and_then(|drag| drag.target.as_ref())
                    .filter(|target| target.surface.id() == sid)
                    .map(|target| target.dropped);
                match client_drag_target_dropped {
                    Some(true) => state.client_drag_cancel(false),
                    Some(false) => {
                        state.client_drag_depart_target(true);
                    }
                    None => {}
                }
                if let Some(parent_id) = state
                    .surfaces
                    .get(&sid)
                    .and_then(|s| s.parent_surface_id.clone())
                    && let Some(parent) = state.surfaces.get_mut(&parent_id)
                {
                    parent.children.retain(|c| *c != sid);
                }
                if let Some(surf) = state.surfaces.remove(&sid) {
                    for fb in surf.pending_presentation_feedbacks {
                        fb.discarded();
                    }
                    state.last_topless_frame_ms.remove(&sid);
                    if surf.surface_id > 0 {
                        state.remove_foreign_exports_for_surface(surf.surface_id);
                        state.screencast_surfaces.remove(&surf.surface_id);
                        state.release_output_for_surface(surf.surface_id);
                        state.toplevel_surface_ids.remove(&surf.surface_id);
                        state.last_request_frame_ms.remove(&surf.surface_id);
                        state.pending_request_frames.remove(&surf.surface_id);
                        state.frame_callback_toplevels.remove(&surf.surface_id);
                        state.last_reported_size.remove(&surf.surface_id);
                        state.pointer_frame_positions.remove(&surf.surface_id);
                        state.surface_sizes.remove(&surf.surface_id);
                        if let Some(ref mut vk) = state.vulkan_renderer {
                            vk.destroy_external_outputs_for_surface(surf.surface_id as u32);
                        }
                        let _ = state.event_tx.send(CompositorEvent::SurfaceDestroyed {
                            surface_id: surf.surface_id,
                        });
                        (state.event_notify)();
                    }
                }
            }
            _ => {}
        }
    }
}

// -- xdg-foreign v2 exporter (portal parent handles) --

impl GlobalDispatch<ZxdgExporterV2, ()> for Compositor {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZxdgExporterV2>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZxdgExporterV2, ()> for Compositor {
    fn request(
        state: &mut Self,
        _client: &Client,
        exporter: &ZxdgExporterV2,
        request: <ZxdgExporterV2 as Resource>::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zxdg_exporter_v2::Request;
        match request {
            Request::ExportToplevel { id, surface } => {
                let surface_id = state.surfaces.get(&surface.id()).and_then(|known| {
                    (same_client(exporter, &surface)
                        && known.surface_id != 0
                        && known.xdg_toplevel.is_some())
                    .then_some(known.surface_id)
                });
                let Some(surface_id) = surface_id else {
                    exporter.post_error(
                        zxdg_exporter_v2::Error::InvalidSurface,
                        "surface is not this client's xdg_toplevel",
                    );
                    return;
                };
                if state
                    .foreign_exports
                    .read()
                    .map_or(true, |exports| exports.len() >= MAX_FOREIGN_EXPORTS)
                {
                    exporter.post_error(
                        zxdg_exporter_v2::Error::InvalidSurface,
                        "xdg-foreign export budget exhausted",
                    );
                    return;
                }
                let Some(handle) = new_foreign_handle(&state.foreign_exports) else {
                    exporter.post_error(
                        zxdg_exporter_v2::Error::InvalidSurface,
                        "could not allocate an export handle",
                    );
                    return;
                };
                let exported = data_init.init(id, handle.clone());
                state
                    .foreign_export_objects
                    .insert(exported.id(), (handle.clone(), surface_id));
                if let Ok(mut exports) = state.foreign_exports.write() {
                    exports.insert(handle.clone(), surface_id);
                }
                exported.handle(handle);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZxdgExportedV2, String> for Compositor {
    fn request(
        state: &mut Self,
        _client: &Client,
        exported: &ZxdgExportedV2,
        request: <ZxdgExportedV2 as Resource>::Request,
        _data: &String,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zxdg_exported_v2::Request;
        if let Request::Destroy = request {
            state.remove_foreign_export(&exported.id());
        }
    }

    fn destroyed(
        state: &mut Self,
        _client_id: ClientId,
        exported: &ZxdgExportedV2,
        _data: &String,
    ) {
        state.remove_foreign_export(&exported.id());
    }
}

fn new_foreign_handle(exports: &Arc<RwLock<HashMap<String, u16>>>) -> Option<String> {
    for _ in 0..8 {
        let mut random = [0u8; 16];
        std::fs::File::open("/dev/urandom")
            .ok()?
            .read_exact(&mut random)
            .ok()?;
        let mut handle = String::with_capacity(32);
        for byte in random {
            let _ = write!(handle, "{byte:02x}");
        }
        if exports
            .read()
            .ok()
            .is_some_and(|known| !known.contains_key(&handle))
        {
            return Some(handle);
        }
    }
    None
}

// -- wl_callback --
impl Dispatch<WlCallback, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlCallback,
        _: <WlCallback as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

// -- wp_presentation --
impl GlobalDispatch<WpPresentation, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WpPresentation>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let pres = data_init.init(resource, ());
        // Tell the client we use CLOCK_MONOTONIC for presentation timestamps.
        pres.clock_id(libc::CLOCK_MONOTONIC as u32);
    }
}

impl Dispatch<WpPresentation, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WpPresentation,
        request: <WpPresentation as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wp_presentation::Request;
        match request {
            Request::Feedback { surface, callback } => {
                let fb = data_init.init(callback, ());
                let sid = surface.id();
                if let Some(surf) = state.surfaces.get_mut(&sid) {
                    surf.pending_presentation_feedbacks.push(fb);
                }
                if let Some(toplevel_sid) = state.find_toplevel_root(&sid).1 {
                    state.frame_callback_toplevels.insert(toplevel_sid);
                }
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

// -- wp_presentation_feedback (no client requests) --
impl Dispatch<WpPresentationFeedback, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpPresentationFeedback,
        _: <WpPresentationFeedback as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

// -- wl_region --
impl Dispatch<WlRegion, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &WlRegion,
        request: <WlRegion as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wayland_server::protocol::wl_region::Request;
        let rid = resource.id();
        match request {
            Request::Add {
                x,
                y,
                width,
                height,
            } => {
                state
                    .regions
                    .entry(rid)
                    .or_insert_with(|| (resource.clone(), Vec::new()))
                    .1
                    .push(RegionOp {
                        add: true,
                        x,
                        y,
                        w: width,
                        h: height,
                    });
            }
            Request::Subtract {
                x,
                y,
                width,
                height,
            } => {
                state
                    .regions
                    .entry(rid)
                    .or_insert_with(|| (resource.clone(), Vec::new()))
                    .1
                    .push(RegionOp {
                        add: false,
                        x,
                        y,
                        w: width,
                        h: height,
                    });
            }
            Request::Destroy => {
                // Clients destroy the region as soon as they have handed it
                // to `set_input_region`, so the surface keeps its own copy
                // and this only drops the builder.
                state.regions.remove(&rid);
            }
            _ => {}
        }
    }
}

// -- wl_subcompositor --
impl GlobalDispatch<WlSubcompositor, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlSubcompositor>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WlSubcompositor, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlSubcompositor,
        request: <WlSubcompositor as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wayland_server::protocol::wl_subcompositor::Request;
        match request {
            Request::GetSubsurface {
                id,
                surface,
                parent,
            } => {
                let child_id = surface.id();
                let parent_id = parent.id();
                data_init.init(
                    id,
                    SubsurfaceData {
                        wl_surface_id: child_id.clone(),
                        parent_surface_id: parent_id.clone(),
                    },
                );
                if let Some(surf) = state.surfaces.get_mut(&child_id) {
                    surf.parent_surface_id = Some(parent_id.clone());
                }
                if let Some(parent_surf) = state.surfaces.get_mut(&parent_id)
                    && !parent_surf.children.contains(&child_id)
                {
                    parent_surf.children.push(child_id);
                }
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

// -- wl_subsurface --
/// User data for a `wp_linux_drm_syncobj_surface_v1`.
struct SyncobjSurfaceData {
    wl_surface_id: ObjectId,
}

impl GlobalDispatch<WpLinuxDrmSyncobjManagerV1, ()> for Compositor {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: wayland_server::New<WpLinuxDrmSyncobjManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpLinuxDrmSyncobjManagerV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        manager: &WpLinuxDrmSyncobjManagerV1,
        request: <WpLinuxDrmSyncobjManagerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wp_linux_drm_syncobj_manager_v1::Request;
        match request {
            Request::GetSurface { id, surface } => {
                let wl_surface_id = surface.id();
                let Some(existing) = state.surfaces.get(&wl_surface_id) else {
                    // Initialising first would hand back an object whose
                    // every request lands on nothing — a silent no-op the
                    // client cannot distinguish from working explicit sync.
                    manager.post_error(
                        wp_linux_drm_syncobj_manager_v1::Error::SurfaceExists,
                        "no such wl_surface",
                    );
                    return;
                };
                if existing.syncobj_surface.is_some() {
                    manager.post_error(
                        wp_linux_drm_syncobj_manager_v1::Error::SurfaceExists,
                        "surface already has a syncobj surface",
                    );
                    return;
                }
                let res = data_init.init(id, SyncobjSurfaceData { wl_surface_id });
                if let Some(surf) = state
                    .surfaces
                    .get_mut(&res.data::<SyncobjSurfaceData>().unwrap().wl_surface_id)
                {
                    surf.syncobj_surface = Some(res);
                }
            }
            Request::ImportTimeline { id, fd } => {
                let Some(dev) = state.syncobj_device.clone() else {
                    manager.post_error(
                        wp_linux_drm_syncobj_manager_v1::Error::InvalidTimeline,
                        "explicit sync unavailable",
                    );
                    return;
                };
                match dev.import_timeline(fd) {
                    Ok(timeline) => {
                        let res = data_init.init(id, ());
                        state.syncobj_timelines.insert(res.id(), timeline);
                    }
                    Err(e) => {
                        manager.post_error(
                            wp_linux_drm_syncobj_manager_v1::Error::InvalidTimeline,
                            format!("drm syncobj import failed: {e}"),
                        );
                    }
                }
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpLinuxDrmSyncobjTimelineV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        timeline: &WpLinuxDrmSyncobjTimelineV1,
        request: <WpLinuxDrmSyncobjTimelineV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wp_linux_drm_syncobj_timeline_v1::Request;
        if let Request::Destroy = request {
            // Pending points hold Arc clones, so dropping the map entry
            // never invalidates a commit already carrying this timeline.
            state.syncobj_timelines.remove(&timeline.id());
        }
    }

    fn destroyed(
        state: &mut Self,
        _: wayland_server::backend::ClientId,
        res: &WpLinuxDrmSyncobjTimelineV1,
        _: &(),
    ) {
        state.syncobj_timelines.remove(&res.id());
    }
}

/// Detach a syncobj surface object from its wl_surface, but only while the
/// wl_surface still points at *this* object: a client may destroy the old
/// one after having asked for a replacement, and clearing unconditionally
/// there would strip explicit sync from a surface that just enabled it.
fn clear_syncobj_surface(
    state: &mut Compositor,
    data: &SyncobjSurfaceData,
    res: &WpLinuxDrmSyncobjSurfaceV1,
) {
    if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id)
        && surf
            .syncobj_surface
            .as_ref()
            .is_some_and(|s| s.id() == res.id())
    {
        surf.syncobj_surface = None;
        surf.pending_acquire_point = None;
        surf.pending_release_point = None;
    }
}

impl Dispatch<WpLinuxDrmSyncobjSurfaceV1, SyncobjSurfaceData> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        res: &WpLinuxDrmSyncobjSurfaceV1,
        request: <WpLinuxDrmSyncobjSurfaceV1 as Resource>::Request,
        data: &SyncobjSurfaceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wp_linux_drm_syncobj_surface_v1::Request;
        let point_of = |state: &Self, timeline: &WpLinuxDrmSyncobjTimelineV1, hi: u32, lo: u32| {
            let found = state.syncobj_timelines.get(&timeline.id()).map(|tl| {
                crate::drm_syncobj::SyncPoint {
                    timeline: tl.clone(),
                    point: ((hi as u64) << 32) | lo as u64,
                }
            });
            if found.is_none() {
                // The import failed earlier, so the commit silently becomes
                // unfenced.  Say so once: without it the only symptom is a
                // surface that samples early, which looks like a driver bug.
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!(
                        "[compositor] sync point names a timeline that was never imported; commit treated as unfenced",
                    );
                }
            }
            found
        };
        match request {
            Request::SetAcquirePoint {
                timeline,
                point_hi,
                point_lo,
            } => {
                let point = point_of(state, &timeline, point_hi, point_lo);
                if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id) {
                    surf.pending_acquire_point = point;
                }
            }
            Request::SetReleasePoint {
                timeline,
                point_hi,
                point_lo,
            } => {
                let point = point_of(state, &timeline, point_hi, point_lo);
                if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id) {
                    surf.pending_release_point = point;
                }
            }
            Request::Destroy => {
                clear_syncobj_surface(state, data, res);
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _: wayland_server::backend::ClientId,
        res: &WpLinuxDrmSyncobjSurfaceV1,
        data: &SyncobjSurfaceData,
    ) {
        clear_syncobj_surface(state, data, res);
    }
}

impl Dispatch<WlSubsurface, SubsurfaceData> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlSubsurface,
        request: <WlSubsurface as Resource>::Request,
        data: &SubsurfaceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wayland_server::protocol::wl_subsurface::Request;
        match request {
            Request::SetPosition { x, y } => {
                if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id) {
                    surf.pending_subsurface_position = Some((x, y));
                }
            }
            Request::PlaceAbove { sibling } => {
                let sibling_id = sibling.id();
                if let Some(parent) = state.surfaces.get_mut(&data.parent_surface_id) {
                    let child_id = &data.wl_surface_id;
                    parent.children.retain(|c| c != child_id);
                    let pos = parent
                        .children
                        .iter()
                        .position(|c| *c == sibling_id)
                        .map(|p| p + 1)
                        .unwrap_or(parent.children.len());
                    parent.children.insert(pos, child_id.clone());
                }
            }
            Request::PlaceBelow { sibling } => {
                let sibling_id = sibling.id();
                if let Some(parent) = state.surfaces.get_mut(&data.parent_surface_id) {
                    let child_id = &data.wl_surface_id;
                    parent.children.retain(|c| c != child_id);
                    let pos = parent
                        .children
                        .iter()
                        .position(|c| *c == sibling_id)
                        .unwrap_or(0);
                    parent.children.insert(pos, child_id.clone());
                }
            }
            Request::SetSync | Request::SetDesync => {}
            Request::Destroy => {
                let child_id = &data.wl_surface_id;
                if let Some(parent) = state.surfaces.get_mut(&data.parent_surface_id) {
                    parent.children.retain(|c| c != child_id);
                }
                if let Some(surf) = state.surfaces.get_mut(child_id) {
                    surf.parent_surface_id = None;
                }
            }
            _ => {}
        }
    }
}

// -- xdg_wm_base --
impl GlobalDispatch<XdgWmBase, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<XdgWmBase>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<XdgWmBase, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &XdgWmBase,
        request: <XdgWmBase as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use xdg_wm_base::Request;
        match request {
            Request::GetXdgSurface { id, surface } => {
                let wl_surface_id = surface.id();
                let xdg_surface = data_init.init(
                    id,
                    XdgSurfaceData {
                        wl_surface_id: wl_surface_id.clone(),
                    },
                );
                if let Some(surf) = state.surfaces.get_mut(&wl_surface_id) {
                    surf.xdg_surface = Some(xdg_surface);
                }
            }
            Request::CreatePositioner { id } => {
                let positioner = data_init.init(id, ());
                let pos_id = positioner.id();
                state.positioners.insert(
                    pos_id,
                    PositionerState {
                        resource: positioner,
                        geometry: PositionerGeometry {
                            size: (0, 0),
                            anchor_rect: (0, 0, 0, 0),
                            anchor: 0,
                            gravity: 0,
                            constraint_adjustment: 0,
                            offset: (0, 0),
                        },
                    },
                );
            }
            Request::Pong { .. } => {}
            Request::Destroy => {}
            _ => {}
        }
    }
}

// -- xdg_surface --
impl Dispatch<XdgSurface, XdgSurfaceData> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &XdgSurface,
        request: <XdgSurface as Resource>::Request,
        data: &XdgSurfaceData,
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use xdg_surface::Request;
        match request {
            Request::GetToplevel { id } => {
                let toplevel = data_init.init(
                    id,
                    XdgToplevelData {
                        wl_surface_id: data.wl_surface_id.clone(),
                    },
                );
                let Some(surface_id) = state.allocate_surface_id() else {
                    // Refusing one toplevel is recoverable; handing out an
                    // id another live surface already holds is not.
                    eprintln!("blit-compositor: surface id space exhausted, refusing toplevel");
                    toplevel.close();
                    let _ = state.display_handle.flush_clients();
                    return;
                };
                if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id) {
                    surf.xdg_toplevel = Some(toplevel.clone());
                    surf.surface_id = surface_id;
                }
                state
                    .toplevel_surface_ids
                    .insert(surface_id, data.wl_surface_id.clone());
                if state.surfaces.get(&data.wl_surface_id).is_some_and(|surf| {
                    !surf.pending_frame_callbacks.is_empty()
                        || !surf.pending_presentation_feedbacks.is_empty()
                }) {
                    state.frame_callback_toplevels.insert(surface_id);
                }

                // Say up front which of the state requests are worth making,
                // so a client can leave the rest out of its titlebar instead
                // of drawing buttons that do nothing.  Fullscreen is the only
                // one we honour; a pane cannot be minimized or unmaximized.
                // This has to precede the first xdg_surface.configure.
                if toplevel.version() >= 5 {
                    toplevel.wm_capabilities(xdg_wm_capabilities(&[
                        xdg_toplevel::WmCapabilities::Fullscreen,
                    ]));
                }

                // Use a per-surface size if one was already configured
                // (e.g. the browser sent C2S_SURFACE_RESIZE before the
                // toplevel was created), otherwise fall back to the global
                // output dimensions.  surface_sizes stores logical pixels.
                let (cw, ch) = state
                    .surface_sizes
                    .get(&surface_id)
                    .copied()
                    .unwrap_or((state.output_width, state.output_height));
                toplevel.configure(cw, ch, pane_states(false));
                let serial = state.next_serial();
                resource.configure(serial);

                // Keyboard focus — sends leave to the previously focused
                // surface's client before entering the new one.
                state.set_keyboard_focus(surface_id);
                // Give this toplevel its own display, visible to nobody
                // else, and tell it which output it is on so it can pick a
                // scale and start rendering.  The client binds the global
                // asynchronously, so the bind handler sends the `enter` too:
                // whichever happens second is the one that lands.
                let owner = resource.client().map(|c| c.id());
                if let Some(owner) = owner.clone() {
                    state.claim_output_for_surface(surface_id, owner);
                }
                let slot = owner
                    .as_ref()
                    .and_then(|owner| state.slot_for_surface(surface_id, owner));
                if let Some(slot) = slot
                    && let Some(surf) = state.surfaces.get(&data.wl_surface_id)
                {
                    let wl = surf.wl_surface.clone();
                    for out in &state.outputs {
                        if out.slot == slot && same_client(&out.resource, &wl) {
                            wl.enter(&out.resource);
                        }
                    }
                }
                let _ = state.display_handle.flush_clients();

                let _ = state.event_tx.send(CompositorEvent::SurfaceCreated {
                    surface_id,
                    title: String::new(),
                    app_id: String::new(),
                    parent_id: 0,
                    width: 0,
                    height: 0,
                });
                // Sent right after creation so a subscriber never sees the
                // surface without knowing whose it is. Identity is fixed for
                // the life of the connection, so this is the only time it can
                // change.
                if let Some(identity) = state.surface_identity(surface_id) {
                    let _ = state.event_tx.send(CompositorEvent::SurfaceOrigin {
                        surface_id,
                        sandbox_engine: identity.sandbox_engine.clone(),
                        app_id: identity.app_id.clone(),
                        instance_id: identity.instance_id.clone(),
                    });
                }
                (state.event_notify)();
                if state.verbose {
                    eprintln!("[compositor] new_toplevel sid={surface_id}");
                }
            }
            Request::GetPopup {
                id,
                parent,
                positioner,
            } => {
                let popup = data_init.init(
                    id,
                    XdgPopupData {
                        wl_surface_id: data.wl_surface_id.clone(),
                    },
                );

                // Parent relationship: make the popup a child of the parent
                // surface so it is composited into the same toplevel frame.
                let parent_wl_id: Option<ObjectId> = parent
                    .as_ref()
                    .and_then(|p| p.data::<XdgSurfaceData>())
                    .map(|d| d.wl_surface_id.clone());

                // The xdg-shell protocol specifies popup positions relative
                // to the parent's *window geometry*, not its surface origin.
                // Fetch the parent's geometry offset so we can convert
                // between window-geometry space and surface-tree space.
                let parent_geom_offset = parent_wl_id
                    .as_ref()
                    .and_then(|pid| state.surfaces.get(pid))
                    .and_then(|s| s.xdg_geometry)
                    .map(|(gx, gy, _, _)| (gx, gy))
                    .unwrap_or((0, 0));

                // Compute the parent's absolute position within the toplevel
                // and the logical output bounds for constraint adjustment.
                // Add the geometry offset so parent_abs represents the
                // window-geometry origin in surface-tree coordinates.
                let parent_abs = parent_wl_id
                    .as_ref()
                    .map(|pid| {
                        let abs = state.surface_absolute_position(pid);
                        (abs.0 + parent_geom_offset.0, abs.1 + parent_geom_offset.1)
                    })
                    .unwrap_or((0, 0));
                // Use the client's actual surface size for popup bounds,
                // not the configured size (client may not have resized yet).
                let (_, toplevel_root) = parent_wl_id
                    .as_ref()
                    .map(|pid| state.find_toplevel_root(pid))
                    .unwrap_or_else(|| {
                        // Dummy root — no parent.
                        (data.wl_surface_id.clone(), None)
                    });
                let bounds = toplevel_root
                    .and_then(|_| {
                        let root_wl_id = parent_wl_id.as_ref().map(|pid| {
                            let (rid, _) = state.find_toplevel_root(pid);
                            rid
                        })?;
                        let surf = state.surfaces.get(&root_wl_id)?;
                        if let Some((gx, gy, gw, gh)) = surf.xdg_geometry
                            && gw > 0
                            && gh > 0
                        {
                            return Some((gx, gy, gw, gh));
                        }

                        // Fall back to the client's actual logical surface
                        // size when window geometry is unavailable.
                        let sm = state.surface_meta.get(&root_wl_id)?;
                        let (lw, lh) = super::render::surface_logical_size(surf, sm);
                        Some((0, 0, lw as i32, lh as i32))
                    })
                    .unwrap_or((0, 0, state.output_width, state.output_height));

                eprintln!(
                    "[popup] parent_abs={parent_abs:?} bounds={bounds:?} parent_wl={parent_wl_id:?} geom_off={parent_geom_offset:?}"
                );
                // Compute geometry from positioner with constraint adjustment.
                let pos_id = positioner.id();
                let (px, py, pw, ph) = state
                    .positioners
                    .get(&pos_id)
                    .map(|p| p.geometry.compute_position(parent_abs, bounds))
                    .unwrap_or((0, 0, 200, 200));
                eprintln!("[popup] result=({px},{py},{pw},{ph})");

                if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id) {
                    surf.xdg_popup = Some(popup.clone());
                    surf.parent_surface_id = parent_wl_id.clone();
                    // Convert from window-geometry-relative to surface-
                    // relative coords so the popup composites correctly.
                    // The rendering crops to xdg_geometry, so the popup
                    // must be offset by the parent's geometry origin.
                    surf.subsurface_position =
                        (parent_geom_offset.0 + px, parent_geom_offset.1 + py);
                }
                if let Some(ref parent_id) = parent_wl_id
                    && let Some(parent_surf) = state.surfaces.get_mut(parent_id)
                    && !parent_surf.children.contains(&data.wl_surface_id)
                {
                    parent_surf.children.push(data.wl_surface_id.clone());
                }

                popup.configure(px, py, pw, ph);
                let serial = state.next_serial();
                resource.configure(serial);
                let _ = state.display_handle.flush_clients();
            }
            Request::SetWindowGeometry {
                x,
                y,
                width,
                height,
            } => {
                if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id) {
                    // For popup surfaces, adjust subsurface_position to
                    // account for the popup's own geometry offset.  The
                    // xdg-shell protocol positions the popup's *geometry*
                    // (not its surface origin) relative to the parent's
                    // geometry.  Without this adjustment, CSD shadows or
                    // borders around the popup cause the visible content
                    // to shift by (gx, gy).
                    if surf.xdg_popup.is_some() {
                        let (old_gx, old_gy) = surf
                            .xdg_geometry
                            .map(|(gx, gy, _, _)| (gx, gy))
                            .unwrap_or((0, 0));
                        surf.subsurface_position.0 += old_gx - x;
                        surf.subsurface_position.1 += old_gy - y;
                    }
                    surf.xdg_geometry = Some((x, y, width, height));
                }
            }
            Request::AckConfigure { .. } => {}
            Request::Destroy => {}
            _ => {}
        }
    }
}

// -- xdg_toplevel --
impl Dispatch<XdgToplevel, XdgToplevelData> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        toplevel: &XdgToplevel,
        request: <XdgToplevel as Resource>::Request,
        data: &XdgToplevelData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use xdg_toplevel::Request;
        match request {
            Request::SetTitle { title } => {
                if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id)
                    && surf.title != title
                {
                    surf.title = title.clone();
                    if surf.surface_id > 0 {
                        let _ = state.event_tx.send(CompositorEvent::SurfaceTitle {
                            surface_id: surf.surface_id,
                            title,
                        });
                        (state.event_notify)();
                    }
                }
            }
            Request::SetAppId { app_id } => {
                if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id)
                    && surf.app_id != app_id
                {
                    surf.app_id = app_id.clone();
                    if surf.surface_id > 0 {
                        let _ = state.event_tx.send(CompositorEvent::SurfaceAppId {
                            surface_id: surf.surface_id,
                            app_id,
                        });
                        (state.event_notify)();
                    }
                }
            }
            Request::Destroy => {
                let wl_surface_id = &data.wl_surface_id;
                state.detach_toplevel_drag_surface(wl_surface_id);
                state.surface_meta.remove(wl_surface_id);
                state.cursor_rgba.remove(wl_surface_id);
                if let Some(ref mut vk) = state.vulkan_renderer {
                    vk.remove_surface(wl_surface_id);
                }
                if let Some(held) = state.held_buffers.remove(wl_surface_id) {
                    state.release_held(held);
                }
                if let Some(a) = state.awaiting_acquire.remove(wl_surface_id) {
                    let a = a.into_release();
                    state.release_held(a);
                }
                let mut unmapped_sid = None;
                if let Some(surf) = state.surfaces.get_mut(wl_surface_id) {
                    let sid = surf.surface_id;
                    surf.xdg_toplevel = None;
                    // The wl_surface outlives its toplevel and can be given a
                    // new one; that one starts out not fullscreen.
                    surf.xdg_fullscreen = false;
                    if sid > 0 {
                        unmapped_sid = Some(sid);
                        state.screencast_surfaces.remove(&sid);
                        state.toplevel_surface_ids.remove(&sid);
                        state.last_request_frame_ms.remove(&sid);
                        state.pending_request_frames.remove(&sid);
                        state.frame_callback_toplevels.remove(&sid);
                        state.last_reported_size.remove(&sid);
                        state.pointer_frame_positions.remove(&sid);
                        state.surface_sizes.remove(&sid);
                        if let Some(ref mut vk) = state.vulkan_renderer {
                            vk.destroy_external_outputs_for_surface(sid as u32);
                        }
                        let _ = state
                            .event_tx
                            .send(CompositorEvent::SurfaceDestroyed { surface_id: sid });
                        (state.event_notify)();
                        surf.surface_id = 0;
                    }
                }
                // Withdraw the window's display outside the surface borrow.
                if let Some(sid) = unmapped_sid {
                    state.remove_foreign_exports_for_surface(sid);
                    state.release_output_for_surface(sid);
                }
            }
            Request::SetMinimized => {
                // There is no minimized state here: a toplevel is a pane in
                // the workspace, always on screen.  xdg-shell promises no
                // configure in reply to this one, but staying silent strands
                // Chromium-based clients (every Electron app) -- they mark
                // themselves minimized the moment they send it and stop
                // drawing until a configure carrying `activated` says
                // otherwise.  With no reply the pane freezes for good, so
                // say no out loud.
                state.reassert_toplevel_configure(&data.wl_surface_id);
            }
            Request::SetMaximized | Request::UnsetMaximized => {
                // A pane is permanently maximized, so neither of these
                // changes anything.  Declining is our call to make; declining
                // silently is not -- xdg-shell says each of them "will respond
                // by emitting a configure event", and clients hold their own
                // state pending until one arrives.
                state.reassert_toplevel_configure(&data.wl_surface_id);
            }
            req @ (Request::SetFullscreen { .. } | Request::UnsetFullscreen) => {
                // Granted, both ways.  Nothing about the pane changes -- it
                // has no decorations and already fills its output, so it is
                // fullscreen in all but name -- but the client is told what it
                // asked for, because refusing costs a feature.  Chromium
                // hands a video to the compositor and waits: a configure
                // without `fullscreen` reads as a refusal, and it drops the
                // page straight back out of fullscreen.  There is nothing to
                // gain by saying no to a window that is already the whole
                // screen.  We ignore the output argument; there is one.
                let fullscreen = matches!(req, Request::SetFullscreen { .. });
                if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id) {
                    surf.xdg_fullscreen = fullscreen;
                }
                state.reassert_toplevel_configure(&data.wl_surface_id);
            }
            Request::SetMinSize { width, height } | Request::SetMaxSize { width, height } => {
                // How small and how large the client says it can draw itself.
                // Zero means "no opinion" for that dimension.
                if width < 0 || height < 0 {
                    toplevel.post_error(
                        xdg_toplevel::Error::InvalidSize,
                        format!("size hint must not be negative, got {width}x{height}"),
                    );
                    return;
                }
                let is_min = matches!(request, Request::SetMinSize { .. });
                if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id) {
                    if is_min {
                        surf.pending_min_size = (width, height);
                    } else {
                        surf.pending_max_size = (width, height);
                    }
                }
                // Double-buffered: nothing takes effect until commit, which is
                // also where the pair is checked against each other.  A client
                // is entitled to send a min above the old max as long as the
                // new max arrives before the same commit.
            }
            _ => {}
        }
    }
}

// -- xdg_popup --
impl Dispatch<XdgPopup, XdgPopupData> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &XdgPopup,
        request: <XdgPopup as Resource>::Request,
        data: &XdgPopupData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use xdg_popup::Request;
        match request {
            Request::Grab { seat: _, serial: _ } => {
                // Add this popup to the grab stack so we can send
                // popup_done when the user clicks outside.
                state
                    .popup_grab_stack
                    .retain(|id| *id != data.wl_surface_id);
                state.popup_grab_stack.push(data.wl_surface_id.clone());
                // A grab is the client saying this menu now owns the input
                // it named. Keyboard focus has to follow, or the menu is on
                // screen while the keyboard still talks to the page.
                state.focus_popup(&data.wl_surface_id);
            }
            Request::Reposition { positioner, token } => {
                // Recompute the popup position using the new positioner.
                let pos_id = positioner.id();
                if let Some(surf) = state.surfaces.get(&data.wl_surface_id)
                    && let Some(parent_id) = surf.parent_surface_id.clone()
                {
                    let parent_geom_offset = state
                        .surfaces
                        .get(&parent_id)
                        .and_then(|s| s.xdg_geometry)
                        .map(|(gx, gy, _, _)| (gx, gy))
                        .unwrap_or((0, 0));
                    let parent_abs = {
                        let abs = state.surface_absolute_position(&parent_id);
                        (abs.0 + parent_geom_offset.0, abs.1 + parent_geom_offset.1)
                    };
                    let (root_id, toplevel_root) = state.find_toplevel_root(&parent_id);
                    let bounds = toplevel_root
                        .and_then(|_| {
                            let surf = state.surfaces.get(&root_id)?;
                            if let Some((gx, gy, gw, gh)) = surf.xdg_geometry
                                && gw > 0
                                && gh > 0
                            {
                                return Some((gx, gy, gw, gh));
                            }
                            let sm = state.surface_meta.get(&root_id)?;
                            let (lw, lh) = super::render::surface_logical_size(surf, sm);
                            Some((0, 0, lw as i32, lh as i32))
                        })
                        .unwrap_or((0, 0, state.output_width, state.output_height));
                    let (px, py, pw, ph) = state
                        .positioners
                        .get(&pos_id)
                        .map(|p| p.geometry.compute_position(parent_abs, bounds))
                        .unwrap_or((0, 0, 200, 200));
                    if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id) {
                        // Undo the previous geometry adjustment before
                        // applying the new position.
                        let old_gx = surf.xdg_geometry.map(|(gx, _, _, _)| gx).unwrap_or(0);
                        let old_gy = surf.xdg_geometry.map(|(_, gy, _, _)| gy).unwrap_or(0);
                        surf.subsurface_position = (
                            parent_geom_offset.0 + px - old_gx,
                            parent_geom_offset.1 + py - old_gy,
                        );
                        if let Some(ref popup) = surf.xdg_popup {
                            popup.configure(px, py, pw, ph);
                            popup.repositioned(token);
                        }
                        if let Some(ref xs) = surf.xdg_surface {
                            let serial = state.serial.wrapping_add(1);
                            state.serial = serial;
                            xs.configure(serial);
                        }
                    }
                }
            }
            Request::Destroy => {
                // Remove from grab stack.
                state
                    .popup_grab_stack
                    .retain(|id| *id != data.wl_surface_id);
                // Hand the keyboard back — a menu closed by its own client
                // (picking an item, pressing Escape) never goes through the
                // click-outside path, so this is the ordinary way a popup
                // ends. Off the stack first, so what remains is what is
                // still grabbing beneath it.
                state.unfocus_popup(&data.wl_surface_id);
                // Destroying the role also unmaps it.  This is idempotent
                // when popup_done already performed the compositor-side
                // unmap above.
                state.unmap_popup_surface(&data.wl_surface_id);
                if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id) {
                    surf.xdg_popup = None;
                    surf.parent_surface_id = None;
                }
            }
            _ => {}
        }
    }
}

// -- xdg_positioner --
use wayland_protocols::xdg::shell::server::xdg_positioner;
impl Dispatch<XdgPositioner, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &XdgPositioner,
        request: <XdgPositioner as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use xdg_positioner::Request;
        let pos_id = resource.id();
        let Some(pos) = state.positioners.get_mut(&pos_id) else {
            return;
        };
        match request {
            Request::SetSize { width, height } => {
                pos.geometry.size = (width, height);
            }
            Request::SetAnchorRect {
                x,
                y,
                width,
                height,
            } => {
                pos.geometry.anchor_rect = (x, y, width, height);
            }
            Request::SetAnchor {
                anchor: wayland_server::WEnum::Value(v),
            } => {
                pos.geometry.anchor = v as u32;
            }
            Request::SetGravity {
                gravity: wayland_server::WEnum::Value(v),
            } => {
                pos.geometry.gravity = v as u32;
            }
            Request::SetOffset { x, y } => {
                pos.geometry.offset = (x, y);
            }
            Request::SetConstraintAdjustment {
                constraint_adjustment,
            } => {
                pos.geometry.constraint_adjustment = constraint_adjustment.into();
            }
            Request::Destroy => {
                state.positioners.remove(&pos_id);
            }
            _ => {}
        }
    }
}

// -- xdg_decoration --
impl GlobalDispatch<ZxdgDecorationManagerV1, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZxdgDecorationManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZxdgDecorationManagerV1, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZxdgDecorationManagerV1,
        request: <ZxdgDecorationManagerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zxdg_decoration_manager_v1::Request;
        match request {
            Request::GetToplevelDecoration { id, toplevel: _ } => {
                let decoration = data_init.init(id, ());
                // Always request server-side (i.e. no) decorations.
                decoration.configure(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZxdgToplevelDecorationV1, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        resource: &ZxdgToplevelDecorationV1,
        request: <ZxdgToplevelDecorationV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use zxdg_toplevel_decoration_v1::Request;
        match request {
            Request::SetMode { .. } | Request::UnsetMode => {
                resource.configure(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

// -- wl_shm --
impl GlobalDispatch<WlShm, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlShm>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let shm = data_init.init(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
        shm.format(wl_shm::Format::Abgr8888);
        shm.format(wl_shm::Format::Xbgr8888);
    }
}

impl Dispatch<WlShm, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlShm,
        request: <WlShm as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wayland_server::protocol::wl_shm::Request;
        if let Request::CreatePool { id, fd, size } = request {
            let pool = data_init.init(id, ());
            let pool_id = pool.id();
            state
                .shm_pools
                .insert(pool_id, Arc::new(ShmPool::new(pool, fd, size)));
        }
    }
}

// -- wl_shm_pool --
impl Dispatch<WlShmPool, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &WlShmPool,
        request: <WlShmPool as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wayland_server::protocol::wl_shm_pool::Request;
        let pool_id = resource.id();
        match request {
            Request::CreateBuffer {
                id,
                offset,
                width,
                height,
                stride,
                format,
            } => {
                // format comes as WEnum<Format>, extract the known value.
                let fmt = match format {
                    wayland_server::WEnum::Value(f) => f,
                    _ => wl_shm::Format::Argb8888, // fallback
                };
                let Some(pool) = state.shm_pools.get(&pool_id).cloned() else {
                    return;
                };
                data_init.init(
                    id,
                    ShmBufferData {
                        pool,
                        offset,
                        width,
                        height,
                        stride,
                        format: fmt,
                    },
                );
            }
            Request::Resize { size } => {
                if let Some(pool) = state.shm_pools.get(&pool_id) {
                    pool.resize(size);
                }
            }
            Request::Destroy => {
                // Drop the map entry — Arc keeps the ShmPool alive while
                // wl_buffers created from it still reference it.
                state.shm_pools.remove(&pool_id);
            }
            _ => {}
        }
    }
}

// -- wl_buffer (SHM) --
impl Dispatch<WlBuffer, ShmBufferData> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlBuffer,
        _: <WlBuffer as Resource>::Request,
        _: &ShmBufferData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }

    fn destroyed(
        state: &mut Self,
        _: wayland_server::backend::ClientId,
        buffer: &WlBuffer,
        _: &ShmBufferData,
    ) {
        if let Some(ref mut vk) = state.vulkan_renderer {
            vk.remove_buffer(&buffer.id());
        }
    }
}

// -- wl_buffer (DMA-BUF) --
impl Dispatch<WlBuffer, DmaBufBufferData> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlBuffer,
        _: <WlBuffer as Resource>::Request,
        _: &DmaBufBufferData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }

    fn destroyed(
        state: &mut Self,
        _: wayland_server::backend::ClientId,
        buffer: &WlBuffer,
        _: &DmaBufBufferData,
    ) {
        // Evict the buffer's cached Vulkan import so the per-buffer
        // texture cache stays bounded by the client's live buffer pool.
        if let Some(ref mut vk) = state.vulkan_renderer {
            vk.remove_buffer(&buffer.id());
        }
    }
}

// -- wl_output --
impl GlobalDispatch<WlOutput, OutputGlobal> for Compositor {
    /// Keep each toplevel's output out of every other client's registry.
    fn can_view(client: Client, global: &OutputGlobal) -> bool {
        client.id() == global.owner
    }

    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlOutput>,
        global: &OutputGlobal,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let slot = global.slot;
        let sid = state.output_slot_surface(slot);
        let output = data_init.init(resource, ());
        output.geometry(
            0,
            0,
            0,
            0,
            wl_output::Subpixel::Unknown,
            "Virtual".to_string(),
            "Headless".to_string(),
            wl_output::Transform::Normal,
        );
        // `name` and `description` are not optional at version 4: a client
        // that binds v4 is entitled to both before the first `done`, and one
        // that keeps a slot for them can fault when they never arrive —
        // xwayland-satellite unwraps its missing OutputName and takes the
        // whole X session down with it.
        if output.version() >= 4 {
            output.name(format!("blit-{slot}"));
            output.description("blit virtual screen".to_string());
        }
        state.send_output_properties(&output, slot);
        state.outputs.push(SurfaceOutput {
            resource: output,
            slot,
        });
        // A screen already holding a toplevel was claimed before the client
        // got round to binding it, so the enter has to be sent from here
        // too, or that surface never learns which output it is on and
        // renders at 1×.  An empty screen has nothing to enter yet; its
        // future toplevel sends its own enter at map.
        //
        // The bridge's screen holds no single toplevel, so its windows are
        // found the other way round: every one of them is on it.
        let owner = state.output_slots.get(&slot).map(|s| s.owner.clone());
        let waiting: Vec<u16> = match (&owner, sid) {
            (Some(owner), _) if state.is_xwayland(owner) => state
                .toplevel_surface_ids
                .keys()
                .copied()
                .filter(|&sid| state.surface_owner(sid).as_ref() == Some(owner))
                .collect(),
            (_, Some(sid)) => vec![sid],
            _ => Vec::new(),
        };
        for sid in waiting {
            if let Some(root_id) = state.toplevel_surface_ids.get(&sid).cloned()
                && let Some(surf) = state.surfaces.get(&root_id)
            {
                let wl = surf.wl_surface.clone();
                if let Some(out) = state.outputs.last() {
                    wl.enter(&out.resource);
                }
            }
        }
    }
}

impl Dispatch<WlOutput, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &WlOutput,
        request: <WlOutput as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wayland_server::protocol::wl_output::Request;
        if let Request::Release = request {
            state.outputs.retain(|o| o.resource.id() != resource.id());
        }
    }
}

// -- wl_seat --
impl GlobalDispatch<WlSeat, ()> for Compositor {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlSeat>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let seat = data_init.init(resource, ());
        seat.capabilities(state.seat_capabilities());
        if seat.version() >= 2 {
            seat.name("headless".to_string());
        }
        state.seats.push(seat);
    }
}

impl Dispatch<WlSeat, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &WlSeat,
        request: <WlSeat as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wayland_server::protocol::wl_seat::Request;
        match request {
            Request::GetKeyboard { id } => {
                let kb = data_init.init(id, ());
                if let Some(fd) = create_keymap_fd(&state.keyboard_keymap_data) {
                    kb.keymap(
                        wl_keyboard::KeymapFormat::XkbV1,
                        fd.as_fd(),
                        state.keyboard_keymap_data.len() as u32,
                    );
                }
                if kb.version() >= 4 {
                    kb.repeat_info(25, 200);
                }
                state.keyboards.push(kb);
            }
            Request::GetPointer { id } => {
                let ptr = data_init.init(id, ());
                // If the cursor is already inside one of this client's
                // surfaces, this pointer has missed its `enter` — the client
                // asked for it too late. Nothing else will deliver one until
                // the pointer crosses into a *different* surface, so a user
                // whose cursor is simply resting over the pane would click
                // into a void. Send it now, at the position we last saw.
                let entered = state.pointer_entered_id.as_ref().and_then(|eid| {
                    state
                        .surfaces
                        .values()
                        .find(|s| s.wl_surface.id() == *eid)
                        .map(|s| s.wl_surface.clone())
                });
                if let Some(wl) = entered
                    && same_client(&ptr, &wl)
                {
                    let serial = state.next_serial();
                    let (lx, ly) = state.pointer_entered_local;
                    ptr.enter(serial, &wl, lx, ly);
                    // Mandatory here: with no motion following, the frame is
                    // what tells a v5+ client the group is complete.
                    ptr.frame();
                    let _ = state.display_handle.flush_clients();
                }
                state.pointers.push(ptr);
            }
            Request::GetTouch { id } => {
                state.touches.push(data_init.init(id, ()));
            }
            Request::Release => {
                state.seats.retain(|s| s.id() != resource.id());
            }
            _ => {}
        }
    }
}

// -- wl_keyboard --
impl Dispatch<WlKeyboard, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &WlKeyboard,
        request: <WlKeyboard as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_keyboard::Request::Release = request {
            state.keyboards.retain(|k| k.id() != resource.id());
        }
    }
}

// -- wl_pointer --
impl Dispatch<WlPointer, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &WlPointer,
        request: <WlPointer as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wl_pointer::Request;
        match request {
            Request::SetCursor {
                serial: _,
                surface,
                hotspot_x,
                hotspot_y,
            } => {
                if let Some(surface) = surface {
                    let sid = surface.id();
                    if let Some(surf) = state.surfaces.get_mut(&sid) {
                        surf.is_cursor = true;
                        surf.cursor_hotspot = (hotspot_x, hotspot_y);
                    }
                    state.current_cursor_surface = Some(sid);
                } else {
                    state.current_cursor_surface = None;
                    let surface_id = state.cursor_target_sid();
                    let _ = state.event_tx.send(CompositorEvent::SurfaceCursor {
                        surface_id,
                        cursor: CursorImage::Hidden,
                    });
                }
            }
            Request::Release => {
                state.pointers.retain(|p| p.id() != resource.id());
            }
            _ => {}
        }
    }
}

// -- wl_touch --
impl Dispatch<WlTouch, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &WlTouch,
        request: <WlTouch as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let wl_touch::Request::Release = request {
            state.touches.retain(|touch| touch.id() != resource.id());
        }
    }
}

// -- zwp_linux_dmabuf_v1 --
impl GlobalDispatch<ZwpLinuxDmabufV1, ()> for Compositor {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwpLinuxDmabufV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let dmabuf = data_init.init(resource, ());
        // v4+ clients use get_default_feedback / get_surface_feedback
        // instead of the deprecated format/modifier events.
        if dmabuf.version() >= 4 {
            return;
        }
        if dmabuf.version() >= 3 {
            // Advertise DRM format modifiers that the Vulkan device can
            // actually import.  This ensures clients (Chromium, mpv, …)
            // allocate DMA-BUFs with a tiling layout the compositor can
            // handle natively on the GPU, avoiding broken CPU mmap
            // fallbacks for vendor-specific tiled VRAM.
            if let Some(ref vk) = state.vulkan_renderer
                && !vk.supported_dmabuf_modifiers.is_empty()
            {
                for &(drm_fmt, modifier) in &vk.supported_dmabuf_modifiers {
                    let mod_hi = (modifier >> 32) as u32;
                    let mod_lo = (modifier & 0xFFFFFFFF) as u32;
                    dmabuf.modifier(drm_fmt, mod_hi, mod_lo);
                }
            }
            // When Vulkan has no DMA-BUF extensions (SHM-only mode) we
            // intentionally advertise zero modifiers so clients fall back
            // to wl_shm.
        } else if state
            .vulkan_renderer
            .as_ref()
            .is_some_and(|vk| vk.has_dmabuf())
        {
            dmabuf.format(drm_fourcc::ARGB8888);
            dmabuf.format(drm_fourcc::XRGB8888);
            dmabuf.format(drm_fourcc::ABGR8888);
            dmabuf.format(drm_fourcc::XBGR8888);
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpLinuxDmabufV1,
        request: <ZwpLinuxDmabufV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwp_linux_dmabuf_v1::Request;
        match request {
            Request::CreateParams { params_id } => {
                data_init.init(params_id, ());
            }
            Request::GetDefaultFeedback { id } => {
                let fb = data_init.init(id, ());
                state.send_dmabuf_feedback(&fb);
            }
            Request::GetSurfaceFeedback { id, .. } => {
                let fb = data_init.init(id, ());
                state.send_dmabuf_feedback(&fb);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwpLinuxDmabufFeedbackV1,
        _request: <ZwpLinuxDmabufFeedbackV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Only request is Destroy, handled automatically.
    }
}

// -- zwp_linux_buffer_params_v1 --
impl Dispatch<ZwpLinuxBufferParamsV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &ZwpLinuxBufferParamsV1,
        request: <ZwpLinuxBufferParamsV1 as Resource>::Request,
        _: &(),
        dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwp_linux_buffer_params_v1::Request;
        let params_id = resource.id();
        match request {
            Request::Add {
                fd,
                plane_idx: _,
                offset,
                stride,
                modifier_hi,
                modifier_lo,
            } => {
                let modifier = ((modifier_hi as u64) << 32) | (modifier_lo as u64);
                let entry = state
                    .dmabuf_params
                    .entry(params_id.clone())
                    .or_insert_with(|| DmaBufParamsPending {
                        resource: resource.clone(),
                        planes: Vec::new(),
                        modifier,
                    });
                entry.modifier = modifier;
                entry.planes.push(DmaBufPlane { fd, offset, stride });
            }
            Request::Create {
                width,
                height,
                format,
                flags,
            } => {
                let pending = state.dmabuf_params.remove(&params_id);
                let (planes, modifier) = match pending {
                    Some(p) => (p.planes, p.modifier),
                    None => {
                        resource.failed();
                        return;
                    }
                };
                let y_invert = flags
                    .into_result()
                    .ok()
                    .is_some_and(|f| f.contains(zwp_linux_buffer_params_v1::Flags::YInvert));
                match client.create_resource::<WlBuffer, DmaBufBufferData, Compositor>(
                    dh,
                    1,
                    DmaBufBufferData {
                        width,
                        height,
                        fourcc: format,
                        modifier,
                        planes,
                        y_invert,
                    },
                ) {
                    Ok(buffer) => resource.created(&buffer),
                    Err(_) => resource.failed(),
                }
            }
            Request::CreateImmed {
                buffer_id,
                width,
                height,
                format,
                flags,
            } => {
                let (planes, modifier) = state
                    .dmabuf_params
                    .remove(&params_id)
                    .map(|p| (p.planes, p.modifier))
                    .unwrap_or_default();
                let y_invert = flags
                    .into_result()
                    .ok()
                    .is_some_and(|f| f.contains(zwp_linux_buffer_params_v1::Flags::YInvert));
                data_init.init(
                    buffer_id,
                    DmaBufBufferData {
                        width,
                        height,
                        fourcc: format,
                        modifier,
                        planes,
                        y_invert,
                    },
                );
            }
            Request::Destroy => {
                state.dmabuf_params.remove(&params_id);
            }
            _ => {}
        }
    }
}

// -- wp_fractional_scale_manager_v1 --
impl GlobalDispatch<WpFractionalScaleManagerV1, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WpFractionalScaleManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WpFractionalScaleManagerV1,
        request: <WpFractionalScaleManagerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wp_fractional_scale_manager_v1::Request;
        match request {
            Request::GetFractionalScale { id, surface } => {
                let fs = data_init.init(id, ());
                let surface = surface.id();
                // Send this surface's own preferred scale immediately.
                let sid = state.find_toplevel_root(&surface).1;
                fs.preferred_scale(sid.map_or(120, |sid| state.surface_scale_120(sid)) as u32);
                state.fractional_scales.push(SurfaceFractionalScale {
                    resource: fs,
                    surface,
                });
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

// -- wp_fractional_scale_v1 --
impl Dispatch<WpFractionalScaleV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &WpFractionalScaleV1,
        _: <WpFractionalScaleV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // Only request is Destroy.
        state
            .fractional_scales
            .retain(|fs| fs.resource.id() != resource.id());
    }
}

// -- wp_viewporter --
impl GlobalDispatch<WpViewporter, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WpViewporter>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpViewporter, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpViewporter,
        request: <WpViewporter as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wp_viewporter::Request;
        match request {
            Request::GetViewport { id, surface } => {
                // Associate the viewport with the surface's ObjectId so
                // SetDestination can update the right Surface.
                let obj_id = surface.id();
                data_init.init(id, obj_id);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

// -- wp_viewport --
impl Dispatch<WpViewport, ObjectId> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WpViewport,
        request: <WpViewport as Resource>::Request,
        surface_obj_id: &ObjectId,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wayland_protocols::wp::viewporter::server::wp_viewport::Request;
        match request {
            Request::SetDestination { width, height } => {
                if let Some(surf) = state.surfaces.get_mut(surface_obj_id) {
                    // width/height of -1 means unset (revert to buffer size).
                    if width > 0 && height > 0 {
                        surf.pending_viewport_destination = Some((width, height));
                    } else {
                        surf.pending_viewport_destination = None;
                    }
                }
            }
            Request::SetSource {
                x,
                y,
                width,
                height,
            } => {
                if let Some(surf) = state.surfaces.get_mut(surface_obj_id) {
                    // All four at -1 unsets the crop; the protocol lets a
                    // client re-declare the whole buffer that way.  Anything
                    // else with a non-positive extent is not a rectangle we
                    // can sample, so treat it as unset rather than dividing
                    // by it later.
                    surf.pending_viewport_source = if width > 0.0 && height > 0.0 {
                        Some((x, y, width, height))
                    } else {
                        None
                    };
                }
            }
            Request::Destroy => {
                // The crop and scale belong to the wl_surface, not to this
                // object, and the spec removes both when it goes — applied
                // on the next commit like any other surface state.
                //
                // Destroying the viewport is the other spec-sanctioned way
                // back to the whole buffer, alongside `set_source(-1, …)`.
                // Ignoring it here would leave the last crop in force and
                // the window squashed by its ratio for as long as the
                // surface lives, which is the same failure this commit is
                // fixing — just reached by the other door.
                if let Some(surf) = state.surfaces.get_mut(surface_obj_id) {
                    surf.pending_viewport_source = None;
                    surf.pending_viewport_destination = None;
                }
            }
            _ => {}
        }
    }
}

// =========================================================================
// NEW PROTOCOLS
// =========================================================================

// -- xdg_toplevel_drag_v1 --

impl GlobalDispatch<XdgToplevelDragManagerV1, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<XdgToplevelDragManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<XdgToplevelDragManagerV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        manager: &XdgToplevelDragManagerV1,
        request: <XdgToplevelDragManagerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use xdg_toplevel_drag_manager_v1::Request;
        match request {
            Request::GetXdgToplevelDrag { id, data_source } => {
                let Some(source_data) = data_source.data::<DataSourceData>() else {
                    manager.post_error(
                        xdg_toplevel_drag_manager_v1::Error::InvalidSource,
                        "unknown data source",
                    );
                    return;
                };
                let mut dnd = source_data.dnd.lock().unwrap();
                if dnd.used || dnd.toplevel_drag {
                    drop(dnd);
                    manager.post_error(
                        xdg_toplevel_drag_manager_v1::Error::InvalidSource,
                        "data source was already used for a toplevel drag",
                    );
                    return;
                }
                dnd.toplevel_drag = true;
                drop(dnd);

                let drag = data_init.init(
                    id,
                    XdgToplevelDragData {
                        source: data_source,
                        attached: std::sync::Mutex::new(None),
                    },
                );
                state.toplevel_drags.push(drag);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<XdgToplevelDragV1, XdgToplevelDragData> for Compositor {
    fn request(
        _state: &mut Self,
        _: &Client,
        drag: &XdgToplevelDragV1,
        request: <XdgToplevelDragV1 as Resource>::Request,
        data: &XdgToplevelDragData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use xdg_toplevel_drag_v1::Request;
        match request {
            Request::Attach {
                toplevel,
                x_offset: _,
                y_offset: _,
            } => {
                let Some(toplevel_data) = toplevel.data::<XdgToplevelData>() else {
                    return;
                };
                let mut attached = data.attached.lock().unwrap();
                if attached.is_some() {
                    drop(attached);
                    drag.post_error(
                        xdg_toplevel_drag_v1::Error::ToplevelAttached,
                        "a live toplevel is already attached",
                    );
                    return;
                }
                *attached = Some(toplevel_data.wl_surface_id.clone());
            }
            Request::Destroy => {
                let ended = data
                    .source
                    .data::<DataSourceData>()
                    .is_none_or(|source| source.dnd.lock().unwrap().ended);
                if !ended {
                    drag.post_error(
                        xdg_toplevel_drag_v1::Error::OngoingDrag,
                        "the underlying data-source drag has not ended",
                    );
                }
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _: wayland_server::backend::ClientId,
        resource: &XdgToplevelDragV1,
        _: &XdgToplevelDragData,
    ) {
        state
            .toplevel_drags
            .retain(|drag| drag.id() != resource.id());
    }
}

// -- wl_data_device_manager (clipboard) --

impl GlobalDispatch<WlDataDeviceManager, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlDataDeviceManager>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WlDataDeviceManager, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlDataDeviceManager,
        request: <WlDataDeviceManager as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wl_data_device_manager::Request;
        match request {
            Request::CreateDataSource { id } => {
                data_init.init(
                    id,
                    DataSourceData {
                        mime_types: std::sync::Mutex::new(Vec::new()),
                        dnd: std::sync::Mutex::new(DataSourceDndState::default()),
                    },
                );
            }
            Request::GetDataDevice { id, seat: _ } => {
                let dd = data_init.init(id, ());
                // A late-binding client must see the selection that already
                // exists, but answering here would land the event inside the
                // roundtrip a client makes while still building its clipboard
                // machinery — fatal for Qt. Focus is the protocol's cue and
                // the safe one; only a client that already holds focus (so is
                // long past that roundtrip) gets an answer now.
                if state
                    .keyboard_focus_wl()
                    .is_some_and(|wl| same_client(&dd, &wl))
                {
                    state.offer_clipboard_to(&dd);
                }
                state.data_devices.push(dd);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataSource, DataSourceData> for Compositor {
    fn request(
        _state: &mut Self,
        _: &Client,
        source: &WlDataSource,
        request: <WlDataSource as Resource>::Request,
        data: &DataSourceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wl_data_source::Request;
        match request {
            Request::Offer { mime_type } => {
                data.mime_types.lock().unwrap().push(mime_type);
            }
            Request::SetActions { dnd_actions } => {
                let Ok(actions) = dnd_actions.into_result() else {
                    source.post_error(
                        wl_data_source::Error::InvalidActionMask,
                        "set_actions contains an unknown action bit",
                    );
                    return;
                };
                let mut dnd = data.dnd.lock().unwrap();
                if dnd.used || dnd.actions.is_some() {
                    drop(dnd);
                    source.post_error(
                        wl_data_source::Error::InvalidSource,
                        "set_actions must be called exactly once before start_drag",
                    );
                    return;
                }
                dnd.actions = Some(actions);
            }
            Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _: wayland_server::backend::ClientId,
        resource: &WlDataSource,
        _: &DataSourceData,
    ) {
        if state
            .selection_source
            .as_ref()
            .is_some_and(|s| s.id() == resource.id())
        {
            state.selection_source = None;
            state.offer_clipboard_selection();
            let _ = state
                .event_tx
                .send(CompositorEvent::ClipboardOwner { wayland: false });
            (state.event_notify)();
        }
        // The drag source went away mid-drag (client destroyed it, or the
        // whole client disconnected): abort the session.  The target gets a
        // `leave`; the source itself cannot be told — it is gone.
        let aborts = state
            .client_drag
            .as_ref()
            .is_some_and(|drag| drag.source.id() == resource.id());
        if aborts
            && let Some(drag) = state.client_drag.take()
            && let Some(target) = drag.target
            && !target.dropped
        {
            target.device.leave();
        }
    }
}

impl Dispatch<WlDataDevice, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        device: &WlDataDevice,
        request: <WlDataDevice as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wl_data_device::Request;
        match request {
            Request::SetSelection { source, serial: _ } => {
                if let Some(source) = source.as_ref()
                    && let Some(data) = source.data::<DataSourceData>()
                {
                    let mut dnd = data.dnd.lock().unwrap();
                    if dnd.used {
                        drop(dnd);
                        device.post_error(
                            wl_data_device::Error::UsedSource,
                            "a data source cannot be reused for set_selection",
                        );
                        return;
                    }
                    if dnd.toplevel_drag {
                        drop(dnd);
                        source.post_error(
                            wl_data_source::Error::InvalidSource,
                            "a source reserved for xdg-toplevel-drag cannot be a selection",
                        );
                        return;
                    }
                    if dnd.actions.is_some() {
                        drop(dnd);
                        source.post_error(
                            wl_data_source::Error::InvalidSource,
                            "a source with DnD actions cannot be used as a selection",
                        );
                        return;
                    }
                    dnd.used = true;
                }
                // Client ownership is direct: advertise this source to every
                // data device and splice each receive fd back to it.  The
                // browser text mirror below is only a convenience, never the
                // transport between two Wayland clients.
                if let Some(previous) = state.selection_source.take()
                    && !source
                        .as_ref()
                        .is_some_and(|current| current.id() == previous.id())
                {
                    previous.cancelled();
                }
                state.external_clipboard = None;
                state.selection_source = source.clone();
                state.offer_clipboard_selection();
                let _ = state.event_tx.send(CompositorEvent::ClipboardOwner {
                    wayland: source.is_some(),
                });
                (state.event_notify)();
                // Try to read text content and emit an event.
                if let Some(ref src) = source {
                    let data = src.data::<DataSourceData>().unwrap();
                    let mimes = data.mime_types.lock().unwrap();
                    let text_mime = mimes
                        .iter()
                        .find(|m| {
                            m.as_str() == "text/plain;charset=utf-8"
                                || m.as_str() == "text/plain"
                                || m.as_str() == "UTF8_STRING"
                        })
                        .cloned();
                    drop(mimes);
                    if let Some(mime) = text_mime {
                        state.read_data_source_and_emit(src, &mime);
                    }
                }
            }
            Request::StartDrag {
                source,
                origin,
                icon,
                serial,
            } => {
                // Icon surfaces are not mapped or positioned anywhere — a
                // client that relies on seeing its drag icon gets none.
                // The transfer itself is unaffected.
                if icon.is_some() && state.verbose {
                    eprintln!("[dnd] start_drag icon surface ignored (not implemented)");
                }
                let Some(source) = source else {
                    return; // start_drag without a source carries no data
                };
                let mimes = source
                    .data::<DataSourceData>()
                    .map(|d| d.mime_types.lock().unwrap().clone())
                    .unwrap_or_default();
                let Some(data) = source.data::<DataSourceData>() else {
                    return;
                };
                let mut dnd = data.dnd.lock().unwrap();
                if dnd.used {
                    drop(dnd);
                    device.post_error(
                        wl_data_device::Error::UsedSource,
                        "a data source cannot be reused for start_drag",
                    );
                    return;
                }
                let source_actions = if source.version() < 3 {
                    // Before v3, Copy is implicit and set_actions does not
                    // exist.
                    DndAction::Copy
                } else {
                    let Some(actions) = dnd.actions else {
                        drop(dnd);
                        source.post_error(
                            wl_data_source::Error::InvalidSource,
                            "a v3 drag source must call set_actions before start_drag",
                        );
                        return;
                    };
                    actions
                };
                dnd.used = true;
                dnd.ended = false;
                drop(dnd);
                // Only a valid new source supersedes an existing session: a
                // browser-driven target gets a leave and a prior client
                // source is cancelled.
                state.drag_end(true);
                state.client_drag_cancel(true);
                if state.verbose {
                    eprintln!(
                        "[dnd] start_drag origin={:?} source_actions={source_actions:?} mimes={mimes:?}",
                        origin.id()
                    );
                }
                let touch_grab = state
                    .active_touches
                    .iter()
                    .find(|(_, active)| {
                        active.down_serial == serial && same_client(&active.target, &origin)
                    })
                    .map(|(&(owner_id, browser_id), active)| TouchDragGrab {
                        owner_id,
                        browser_id,
                        surface_id: active.surface_id,
                    });
                if let Some(grab) = touch_grab {
                    // The drag takes the seat over, which is what
                    // `wl_touch.cancel` means. Sending it is what a physical
                    // compositor does, and it is the only way to leave the
                    // client's contact set consistent: `cancel` covers the whole
                    // sequence, so contacts the drag swallows cannot be reported
                    // individually afterwards. The grab above is captured first
                    // precisely because this empties `active_touches`.
                    state.retire_touch_contacts(Some(grab.owner_id));
                }
                state.client_drag = Some(ClientDragState {
                    source,
                    origin,
                    mimes,
                    target: None,
                    source_actions,
                    dropped: false,
                    touch_grab,
                });
            }
            Request::Release => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _: wayland_server::backend::ClientId,
        resource: &WlDataDevice,
        _: &(),
    ) {
        state.data_devices.retain(|d| d.id() != resource.id());
        // Before drop, loss of the target only clears focus and the source
        // drag may continue. After drop it ends the session: no target is
        // left that can finish the transfer.
        let client_target_dropped = state
            .client_drag
            .as_ref()
            .and_then(|drag| drag.target.as_ref())
            .filter(|target| target.device.id() == resource.id())
            .map(|target| target.dropped);
        match client_target_dropped {
            Some(true) => state.client_drag_cancel(false),
            Some(false) => {
                state.client_drag_depart_target(false);
            }
            None => {}
        }
        // Same for a browser-driven drag whose target went away.
        if state
            .drag
            .as_ref()
            .is_some_and(|d| d.device.id() == resource.id())
        {
            state.drag = None;
        }
    }
}

impl Dispatch<WlDataOffer, DataOfferData> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        offer: &WlDataOffer,
        request: <WlDataOffer as Resource>::Request,
        data: &DataOfferData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wl_data_offer::Request;
        match &data.kind {
            DataOfferKind::ClientDrag => {
                // A client-initiated drag: this offer is the splice point
                // between the target and the drag source.
                if data.finished.load(std::sync::atomic::Ordering::Relaxed)
                    && !matches!(&request, Request::Destroy)
                {
                    offer.post_error(
                        wl_data_offer::Error::InvalidFinish,
                        "only destroy is legal after finish",
                    );
                    return;
                }
                match request {
                    Request::Accept { mime_type, .. } => {
                        let mut notify_source = None;
                        if let Some(drag) = state.client_drag.as_mut()
                            && let Some(t) = drag.target.as_mut()
                            && t.offer.id() == offer.id()
                        {
                            t.accepted_mime = mime_type.clone();
                            notify_source = Some(drag.source.clone());
                        }
                        if let Some(source) = notify_source {
                            // `accept` is protocol feedback. Chromium does
                            // not use target as its data trigger (receive is),
                            // but other sources may use it for UI feedback.
                            source.target(mime_type.clone());
                        }
                        if state.verbose {
                            eprintln!("[dnd] target accept mime={mime_type:?}");
                        }
                    }
                    Request::Receive { mime_type, fd } => {
                        // Forward to the source; the fd must stay alive
                        // until the flush hands it across, so flush here
                        // rather than letting the OwnedFd close first.
                        let forward = state
                            .client_drag
                            .as_ref()
                            .and_then(|drag| drag.target.as_ref())
                            .is_some_and(|t| t.offer.id() == offer.id());
                        if forward {
                            if let Some(drag) = state.client_drag.as_ref() {
                                drag.source.send(mime_type.clone(), fd.as_fd());
                            }
                            let _ = state.display_handle.flush_clients();
                        }
                        if state.verbose {
                            eprintln!("[dnd] target receive mime={mime_type:?} forward={forward}");
                        }
                    }
                    Request::Finish => {
                        let valid = state
                            .client_drag
                            .as_ref()
                            .and_then(|drag| drag.target.as_ref())
                            .is_some_and(|t| {
                                t.offer.id() == offer.id()
                                    && t.dropped
                                    && t.accepted_mime.is_some()
                                    && matches!(
                                        t.action,
                                        Some(action)
                                            if action == DndAction::Copy
                                                || action == DndAction::Move
                                    )
                            });
                        if !valid {
                            offer.post_error(
                                wl_data_offer::Error::InvalidFinish,
                                "finish requires a dropped, accepted Copy or Move offer",
                            );
                            return;
                        }
                        data.finished
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        if let Some(drag) = state.client_drag.take()
                            && drag.source.version() >= 3
                        {
                            drag.source.dnd_finished();
                        }
                        if state.verbose {
                            eprintln!("[dnd] target finish complete=true");
                        }
                    }
                    Request::SetActions {
                        dnd_actions,
                        preferred_action,
                    } => {
                        let Ok(actions) = dnd_actions.into_result() else {
                            offer.post_error(
                                wl_data_offer::Error::InvalidActionMask,
                                "set_actions contains an unknown action bit",
                            );
                            return;
                        };
                        let Ok(preferred) = preferred_action.into_result() else {
                            offer.post_error(
                                wl_data_offer::Error::InvalidAction,
                                "set_actions has an unknown preferred action",
                            );
                            return;
                        };
                        let active = state.client_drag.as_ref().is_some_and(|drag| {
                            drag.target
                                .as_ref()
                                .is_some_and(|t| t.offer.id() == offer.id())
                        });
                        // A set_actions already queued when the pointer left
                        // belongs to a stale offer. The peer has also been
                        // sent leave and will destroy it; do not turn that
                        // cross-client race into a protocol error.
                        if !active {
                            return;
                        }
                        let preferred_valid = preferred.is_empty()
                            || (preferred.bits().count_ones() == 1 && actions.contains(preferred));
                        if !preferred_valid {
                            offer.post_error(
                                wl_data_offer::Error::InvalidAction,
                                "preferred action must be one action from the destination mask",
                            );
                            return;
                        }
                        let mut on_target = false;
                        if let Some(drag) = state.client_drag.as_mut()
                            && let Some(t) = drag.target.as_mut()
                            && t.offer.id() == offer.id()
                        {
                            t.offer_actions = actions;
                            t.preferred_action = preferred;
                            on_target = true;
                        }
                        if on_target {
                            state.client_drag_renegotiate();
                        }
                        if state.verbose {
                            eprintln!(
                                "[dnd] target set_actions actions={actions:?} preferred={preferred:?}"
                            );
                        }
                    }
                    Request::Destroy => {
                        let dropped = state
                            .client_drag
                            .as_ref()
                            .and_then(|drag| drag.target.as_ref())
                            .filter(|target| target.offer.id() == offer.id())
                            .map(|target| target.dropped);
                        if let Some(dropped) = dropped {
                            // An explicit pre-drop cancellation still ends
                            // the entered session with leave. Disconnect
                            // teardown cannot send it and stays in destroyed.
                            state.client_drag_cancel(!dropped);
                        }
                    }
                    _ => {}
                }
            }
            DataOfferKind::BrowserDrag => {
                // A browser-initiated drag.  The payload arrives with the
                // DROP; until then, receives are parked rather than
                // answered empty (see `DragSessionState::parked`).  After
                // the drop, `receive` serves the staged payload by mime,
                // empty for a mime it never carried.
                match request {
                    Request::Receive { mime_type, fd } => {
                        // A planned uri-list is servable at enter:
                        // Chromium's enter-time fetch completes during
                        // hover, which is what fires its page-level
                        // dragenter (and shows the drop target) before the
                        // mouse release.
                        // A late request on an offer from a previous enter
                        // must never see the replacement session's plan or
                        // payload.  Offer resources outlive compositor-side
                        // session state until their client destroys them.
                        let active = state.drag.as_ref().filter(|d| d.offer.id() == offer.id());
                        let planned = active.and_then(|d| {
                            if mime_type == "text/uri-list" && !d.dropped {
                                d.planned_uri_list.clone()
                            } else {
                                None
                            }
                        });
                        let staged = active.and_then(|d| {
                            browser_drag_bytes(&d.offers, &mime_type).map(<[u8]>::to_vec)
                        });
                        match planned.or(staged) {
                            Some(bytes) => {
                                use std::io::Write;
                                let mut f = std::fs::File::from(fd);
                                let _ = f.write_all(&bytes);
                            }
                            None => {
                                let parkable = state
                                    .drag
                                    .as_ref()
                                    .is_some_and(|d| d.offer.id() == offer.id() && !d.dropped);
                                if parkable && let Some(d) = state.drag.as_mut() {
                                    d.parked.push((mime_type, fd));
                                }
                                // Otherwise (no session, or the drop already
                                // landed without this mime) the fd closes
                                // empty.
                            }
                        }
                    }
                    Request::Finish => {
                        if let Some(ref mut d) = state.drag
                            && d.offer.id() == offer.id()
                        {
                            d.finished = true;
                        }
                    }
                    Request::SetActions { dnd_actions, .. } => {
                        // There is no source to negotiate with, but the
                        // destination's mask still has to be answered:
                        // Chromium takes its negotiated operation from the
                        // `action` event and refuses the drop without one.
                        // A browser drop is a copy, so Copy is the whole
                        // source side; an empty intersection is NONE.
                        if state
                            .drag
                            .as_ref()
                            .is_some_and(|d| d.offer.id() == offer.id())
                        {
                            let dst = dnd_actions
                                .into_result()
                                .unwrap_or_else(|_| DndAction::empty());
                            let negotiated =
                                negotiate_dnd_action(DndAction::Copy, dst, DndAction::Copy)
                                    .unwrap_or_else(DndAction::empty);
                            offer.action(negotiated);
                        }
                    }
                    Request::Destroy => {
                        if let Some(d) = state.drag.as_ref()
                            && d.offer.id() == offer.id()
                        {
                            state.drag = None;
                        }
                    }
                    _ => {} // Accept
                }
            }
            DataOfferKind::ClipboardExternal(cb) => {
                match request {
                    // A type we never offered gets the fd closed empty, not the
                    // bytes we happen to be holding.
                    Request::Receive { mime_type, fd } => {
                        if cb.offers(&mime_type) {
                            use std::io::Write;
                            let mut f = std::fs::File::from(fd);
                            let _ = f.write_all(&cb.data);
                        }
                    }
                    Request::Destroy => {}
                    _ => {} // Accept, Finish, SetActions — DnD
                }
            }
            DataOfferKind::ClipboardSource { source, mime_types } => match request {
                Request::Receive { mime_type, fd } => {
                    if mime_types.contains(&mime_type) {
                        // Keep the fd alive through the flush that hands it
                        // to the owner, exactly like a client-drag splice.
                        source.send(mime_type, fd.as_fd());
                        let _ = state.display_handle.flush_clients();
                    }
                }
                Request::Destroy => {}
                _ => {} // Accept, Finish, SetActions — DnD
            },
        }
    }

    fn destroyed(
        state: &mut Self,
        _: wayland_server::backend::ClientId,
        resource: &WlDataOffer,
        data: &DataOfferData,
    ) {
        match &data.kind {
            DataOfferKind::ClientDrag => {
                let dropped = state
                    .client_drag
                    .as_ref()
                    .and_then(|drag| drag.target.as_ref())
                    .filter(|target| target.offer.id() == resource.id())
                    .map(|target| target.dropped);
                match dropped {
                    Some(true) => state.client_drag_cancel(false),
                    // Destroying the active offer is the destination's
                    // explicit cancellation (ordinary leaves already clear
                    // `target` before the client destroys its stale offer).
                    Some(false) => state.client_drag_cancel(false),
                    None => return,
                }
                if state.verbose {
                    eprintln!(
                        "[dnd] target offer destroyed dropped={}",
                        dropped.unwrap_or(false)
                    );
                }
            }
            DataOfferKind::BrowserDrag => {
                if state
                    .drag
                    .as_ref()
                    .is_some_and(|drag| drag.offer.id() == resource.id())
                {
                    state.drag = None;
                }
            }
            DataOfferKind::ClipboardExternal(_) | DataOfferKind::ClipboardSource { .. } => {}
        }
    }
}

impl Compositor {
    /// Create a pipe, ask the data source to write into it, read the result,
    /// and emit a `ClipboardContent` event.
    fn read_data_source_and_emit(&mut self, source: &WlDataSource, mime_type: &str) {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return;
        }
        let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        source.send(mime_type.to_string(), write_fd.as_fd());
        let _ = self.display_handle.flush_clients();
        // Non-blocking read with a modest limit.
        unsafe {
            libc::fcntl(read_fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        // Give the client a moment to write.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            let n = unsafe {
                libc::read(
                    read_fd.as_raw_fd(),
                    tmp.as_mut_ptr() as *mut libc::c_void,
                    tmp.len(),
                )
            };
            if n <= 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n as usize]);
            if buf.len() > 1024 * 1024 {
                break; // 1 MiB cap
            }
        }
        if !buf.is_empty() {
            let _ = self.event_tx.send(CompositorEvent::ClipboardContent {
                mime_type: mime_type.to_string(),
                data: buf,
            });
            (self.event_notify)();
        }
    }

    /// Hand the current clipboard selection to one data device, or clear it.
    /// External selections are served from pinned bytes; Wayland-client
    /// selections are fd-spliced directly back to their source.
    fn offer_clipboard_to(&self, dd: &WlDataDevice) {
        let (mimes, kind) = if let Some(ref cb) = self.external_clipboard {
            (
                cb.mime_types(),
                DataOfferKind::ClipboardExternal(cb.clone()),
            )
        } else if let Some(ref source) = self.selection_source {
            let mime_types = source
                .data::<DataSourceData>()
                .map(|d| d.mime_types.lock().unwrap().clone())
                .unwrap_or_default();
            (
                mime_types.clone(),
                DataOfferKind::ClipboardSource {
                    source: source.clone(),
                    mime_types,
                },
            )
        } else {
            dd.selection(None);
            return;
        };
        let Some(client) = dd.client() else {
            return;
        };
        let Ok(offer) = client.create_resource::<WlDataOffer, DataOfferData, Compositor>(
            &self.display_handle,
            dd.version(),
            DataOfferData::new(kind),
        ) else {
            return;
        };
        dd.data_offer(&offer);
        for mime in mimes {
            offer.offer(mime);
        }
        dd.selection(Some(&offer));
    }

    /// Hand both selections to the devices belonging to `wl`'s client.
    ///
    /// Keyboard focus is the protocol's cue to deliver a selection, and it is
    /// also the earliest cue a client can safely take one on: Qt binds its
    /// data device from inside the platform-integration constructor and its
    /// `selection` handler dereferences the integration pointer that
    /// constructor has not returned yet, so a selection answered at bind time
    /// segfaults the client before it ever draws (Zoom).
    fn offer_selections_to_client(&self, wl: &WlSurface) {
        for dd in &self.data_devices {
            if same_client(dd, wl) {
                self.offer_clipboard_to(dd);
            }
        }
        for pd in &self.primary_devices {
            if same_client(pd, wl) {
                self.offer_primary_to(pd);
            }
        }
    }

    /// Push the current clipboard selection to all connected data devices.
    fn offer_clipboard_selection(&mut self) {
        for dd in &self.data_devices {
            self.offer_clipboard_to(dd);
        }
        let _ = self.display_handle.flush_clients();
    }

    // -- Browser-initiated drag and drop --
    //
    // A drag that starts on the user's desktop has no Wayland client behind
    // it, so the compositor drives the `wl_data_device` drag session itself
    // with a null source: it enters with a compositor-owned offer, forwards
    // motion, and on drop hands the staged payload over through the offer's
    // `receive`.  There is no source to notify, so `dnd_drop_performed` /
    // `dnd_finished` are never sent.

    /// Convert composited-frame physical coordinates to a hit-tested
    /// surface-local logical position, exactly as `PointerMotion` does.
    fn frame_to_surface_tree(
        &self,
        surface_id: u16,
        x: f64,
        y: f64,
    ) -> Option<(ObjectId, f64, f64)> {
        let (mut x, mut y) =
            if let Some(&(cw, ch, lw, lh)) = self.last_reported_size.get(&surface_id) {
                let sx = if cw > 0 { lw as f64 / cw as f64 } else { 1.0 };
                let sy = if ch > 0 { lh as f64 / ch as f64 } else { 1.0 };
                (x * sx, y * sy)
            } else {
                (x, y)
            };
        if let Some((gx, gy, _, _)) = self
            .toplevel_surface_ids
            .get(&surface_id)
            .and_then(|rid| self.surfaces.get(rid))
            .and_then(|s| s.xdg_geometry)
        {
            x += gx as f64;
            y += gy as f64;
        }
        self.toplevel_surface_ids
            .get(&surface_id)
            .cloned()
            .map(|root_id| (root_id, x, y))
    }

    fn drag_target(&self, surface_id: u16, x: f64, y: f64) -> Option<(WlSurface, f64, f64)> {
        let (root_id, x, y) = self.frame_to_surface_tree(surface_id, x, y)?;
        self.hit_test_surface_at(&root_id, x, y)
    }

    /// Enter a drag session on the surface under (`x`, `y`), advertising
    /// `mimes`.  A second enter retargets: the old session is left first.
    fn drag_enter(
        &mut self,
        surface_id: u16,
        x: f64,
        y: f64,
        mimes: &[String],
        planned_uri_list: Option<Vec<u8>>,
    ) {
        // A browser drag supersedes a client-initiated one: the source is
        // told its drag was cancelled, its target gets a leave.
        self.client_drag_cancel(true);
        self.drag_end(true);
        let Some((wl_surface, lx, ly)) = self.drag_target(surface_id, x, y) else {
            return;
        };
        let serial = self.next_serial();
        for dd in &self.data_devices {
            if !same_client(dd, &wl_surface) {
                continue;
            }
            let Some(client) = dd.client() else {
                continue;
            };
            let Ok(offer) = client.create_resource::<WlDataOffer, DataOfferData, Compositor>(
                &self.display_handle,
                dd.version(),
                DataOfferData::new(DataOfferKind::BrowserDrag),
            ) else {
                continue;
            };
            dd.data_offer(&offer);
            for mime in mimes {
                offer.offer(mime.clone());
            }
            // Chromium initializes its negotiated action to NONE and only
            // updates it from source_actions/action events; without one it
            // refuses the drop outright, however the drag ends.
            if offer.version() >= 3 {
                offer.source_actions(DndAction::Copy);
            }
            // wl_data_device.enter's source is allow-null exactly for this:
            // a drag with no client source behind it.
            dd.enter(serial, &wl_surface, lx, ly, Some(&offer));
            self.drag = Some(DragSessionState {
                device: dd.clone(),
                offer,
                offers: Vec::new(),
                planned_uri_list,
                parked: Vec::new(),
                dropped: false,
                finished: false,
            });
            // One drag targets one device; further devices of the same
            // client are its other seat bindings, not other targets.
            break;
        }
    }

    /// Land the drop: hand the payload to the session, send `drop`, then end
    /// the target focus with `leave`.  Chromium uses that final leave to tear
    /// down its native/DOM drag state; without it an app can accept the data
    /// while leaving its drag overlay visible.  The offer stays alive for
    /// post-drop `receive`/`finish` requests.
    fn drag_drop(&mut self, surface_id: u16, x: f64, y: f64, offers: Vec<(String, Vec<u8>)>) {
        if self.drag.is_none() {
            return;
        }
        // A final motion so the drop lands where the pointer is, not where
        // the last MOTION happened to leave it.
        if let Some((_, lx, ly)) = self.drag_target(surface_id, x, y)
            && let Some(ref drag) = self.drag
        {
            drag.device.motion(elapsed_ms(), lx, ly);
        }
        if let Some(ref mut drag) = self.drag {
            drag.offers = offers;
            // Answer the receives an eager fetcher (Chromium reads every
            // supported mime at enter) parked with us — before the drop
            // event goes out, so the fetched snapshot is complete by the
            // time the client processes the drop.  A mime the drop did not
            // stage closes empty.
            let parked = std::mem::take(&mut drag.parked);
            for (mime, fd) in parked {
                if let Some(bytes) = browser_drag_bytes(&drag.offers, &mime) {
                    use std::io::Write;
                    let mut f = std::fs::File::from(fd);
                    let _ = f.write_all(bytes);
                }
            }
            drag.dropped = true;
            drag.device.drop();
            drag.device.leave();
        }
    }

    /// End the session, optionally telling the target (`leave`).  A session
    /// that already dropped already received its terminal `leave`, but its
    /// offer may still be in `receive`/`finish`, so only the state's session
    /// slot is cleared.  Dropping the offer handle does not destroy it
    /// client-side — the client owns that and will `destroy` it.
    fn drag_end(&mut self, leave: bool) {
        if let Some(drag) = self.drag.take()
            && leave
            && !drag.dropped
        {
            drag.device.leave();
        }
    }

    // -- Client-initiated drag and drop (wl_data_device.start_drag) --
    //
    // The compositor owns the implicit grab: while a client drag is active
    // and undropped, PointerMotion/PointerButton commands drive the session
    // instead of reaching wl_pointer.  Enter/motion/leave follow the
    // hit-tested surface under the point; a release drops on the current
    // target or cancels at the source.

    /// End the current target focus while keeping the source drag alive so
    /// it can enter another surface.
    fn client_drag_depart_target(&mut self, leave: bool) -> bool {
        let Some(drag) = self.client_drag.as_mut() else {
            return false;
        };
        let Some(target) = drag.target.take() else {
            return false;
        };
        if leave && !target.dropped {
            target.device.leave();
        }
        if !target.dropped {
            drag.source.target(None);
            if drag.source.version() >= 3 {
                drag.source.action(DndAction::empty());
            }
        }
        true
    }

    /// Abort the entire source drag. A pre-drop target is left and its
    /// source feedback is reset; after drop, only cancellation remains.
    fn client_drag_cancel(&mut self, leave: bool) {
        let Some(mut drag) = self.client_drag.take() else {
            return;
        };
        if let Some(target) = drag.target.take()
            && !target.dropped
        {
            if leave {
                target.device.leave();
            }
            drag.source.target(None);
            if drag.source.version() >= 3 {
                drag.source.action(DndAction::empty());
            }
        }
        Self::mark_data_source_drag_ended(&drag.source);
        drag.source.cancelled();
    }

    fn mark_data_source_drag_ended(source: &WlDataSource) {
        if let Some(data) = source.data::<DataSourceData>() {
            data.dnd.lock().unwrap().ended = true;
        }
    }

    /// Forget an xdg-toplevel-drag attachment when its role is destroyed or
    /// its surface is unmapped. The protocol permits another attach after it.
    fn detach_toplevel_drag_surface(&self, surface_id: &ObjectId) {
        for drag in &self.toplevel_drags {
            let Some(data) = drag.data::<XdgToplevelDragData>() else {
                continue;
            };
            let mut attached = data.attached.lock().unwrap();
            if attached.as_ref() == Some(surface_id) {
                *attached = None;
            }
        }
    }

    /// The window carried by xdg-toplevel-drag is visual payload, not a drop
    /// destination. This matters if the frontend mounts the detached Brave
    /// window as a pane before the physical drag has ended.
    fn is_current_toplevel_drag_attachment(&self, surface: &WlSurface) -> bool {
        let Some(source_id) = self.client_drag.as_ref().map(|drag| drag.source.id()) else {
            return false;
        };
        let root_id = self.find_toplevel_root(&surface.id()).0;
        self.toplevel_drags.iter().any(|drag| {
            drag.data::<XdgToplevelDragData>().is_some_and(|data| {
                data.source.id() == source_id
                    && data.attached.lock().unwrap().as_ref() == Some(&root_id)
            })
        })
    }

    /// Whether pointer input currently belongs to a client drag.
    fn client_pointer_drag_grabbed(&self) -> bool {
        self.client_drag
            .as_ref()
            .is_some_and(|drag| !drag.dropped && drag.touch_grab.is_none())
    }

    /// The contact a live touch drag follows, if there is one.
    fn client_touch_drag_contact(&self) -> Option<TouchDragGrab> {
        self.client_drag
            .as_ref()
            .filter(|drag| !drag.dropped)
            .and_then(|drag| drag.touch_grab)
    }

    fn client_touch_drag_active(&self) -> bool {
        self.client_touch_drag_contact().is_some()
    }

    /// Drive the session to the surface under (`x`, `y`): leave the old
    /// target on crossing, enter (with a fresh offer) on arrival, motion
    /// while inside.
    fn client_drag_motion(&mut self, surface_id: u16, x: f64, y: f64) {
        let hit = self
            .drag_target(surface_id, x, y)
            .filter(|(surface, _, _)| !self.is_current_toplevel_drag_attachment(surface));
        // Crossed off the current target's surface?
        let crossed = match (
            self.client_drag.as_ref().and_then(|d| d.target.as_ref()),
            &hit,
        ) {
            (Some(t), Some((wl, _, _))) => t.surface.id() != wl.id(),
            (Some(_), None) => true,
            (None, _) => false,
        };
        if crossed {
            self.client_drag_depart_target(true);
        }
        let Some((wl_surface, lx, ly)) = hit else {
            return;
        };
        let already_inside = self
            .client_drag
            .as_ref()
            .and_then(|d| d.target.as_ref())
            .is_some();
        if already_inside {
            if let Some(drag) = self.client_drag.as_ref()
                && let Some(t) = drag.target.as_ref()
            {
                t.device.motion(elapsed_ms(), lx, ly);
            }
            return;
        }
        // Enter: hand the target's client an offer advertising the source's
        // mime list, on the first data device it bound.
        let Some(drag) = self.client_drag.as_ref() else {
            return;
        };
        let mimes = drag.mimes.clone();
        let source_actions = drag.source_actions;
        let mut entered: Option<(WlDataDevice, WlDataOffer)> = None;
        for dd in &self.data_devices {
            if !same_client(dd, &wl_surface) {
                continue;
            }
            let Some(client) = dd.client() else {
                continue;
            };
            let Ok(offer) = client.create_resource::<WlDataOffer, DataOfferData, Compositor>(
                &self.display_handle,
                dd.version(),
                DataOfferData::new(DataOfferKind::ClientDrag),
            ) else {
                continue;
            };
            // The target has not declared its action mask yet. Advertise
            // the source and enter; negotiation begins when set_actions
            // arrives for this offer.
            dd.data_offer(&offer);
            for mime in &mimes {
                offer.offer(mime.clone());
            }
            // v3 destinations cannot choose an action until they know the
            // source mask.  The protocol requires this immediately after
            // the offer is created, before enter; action follows only after
            // the destination replies with set_actions.
            if offer.version() >= 3 {
                offer.source_actions(source_actions);
            }
            entered = Some((dd.clone(), offer));
            break;
        }
        let Some((device, offer)) = entered else {
            return;
        };
        let legacy_action = if offer.version() < 3 && source_actions.contains(DndAction::Copy) {
            Some(DndAction::Copy)
        } else {
            None
        };
        let serial = self.next_serial();
        device.enter(serial, &wl_surface, lx, ly, Some(&offer));
        if let Some(drag) = self.client_drag.as_mut() {
            if offer.version() < 3 && drag.source.version() >= 3 {
                drag.source
                    .action(legacy_action.unwrap_or_else(DndAction::empty));
            }
            drag.target = Some(ClientDragTarget {
                device,
                offer,
                surface: wl_surface,
                accepted_mime: None,
                offer_actions: if legacy_action.is_some() {
                    DndAction::Copy
                } else {
                    DndAction::empty()
                },
                preferred_action: legacy_action.unwrap_or_else(DndAction::empty),
                action: legacy_action,
                action_announced: legacy_action.is_some(),
                dropped: false,
            });
        }
    }

    /// The button went up: drop on the current target (the source learns
    /// `dnd_finished` when the target finishes), or cancel at the source
    /// when there is none.  Either way the grab is over.
    fn client_drag_release(&mut self) {
        let Some(drag) = self.client_drag.as_ref() else {
            return;
        };
        let valid = drag.target.as_ref().is_some_and(|target| {
            target.action.is_some()
                && (target.offer.version() < 3 || target.accepted_mime.is_some())
        });
        if self.verbose {
            eprintln!(
                "[dnd] release origin={:?} accepted_mime={:?} action={:?} valid={valid}",
                drag.origin.id(),
                drag.target
                    .as_ref()
                    .and_then(|target| target.accepted_mime.as_ref()),
                drag.target.as_ref().and_then(|target| target.action),
            );
        }
        if !valid {
            self.client_drag_cancel(true);
            return;
        }
        let Some(drag) = self.client_drag.as_mut() else {
            return;
        };
        drag.dropped = true;
        if let Some(t) = drag.target.as_mut() {
            t.dropped = true;
            t.device.drop();
            Self::mark_data_source_drag_ended(&drag.source);
            if drag.source.version() >= 3 {
                drag.source.dnd_drop_performed();
            }
        }
    }

    /// Recompute the selected action after the destination's `set_actions`.
    /// NONE is a real, transient result: keep the pointer focus and announce
    /// it so a later destination update can make the same offer viable.
    fn client_drag_renegotiate(&mut self) {
        let Some(drag) = self.client_drag.as_mut() else {
            return;
        };
        let Some(t) = drag.target.as_mut() else {
            return;
        };
        let new = negotiate_dnd_action(drag.source_actions, t.offer_actions, t.preferred_action);
        if !t.action_announced || t.action != new {
            t.action = new;
            t.action_announced = true;
            let action = new.unwrap_or_else(DndAction::empty);
            t.offer.action(action);
            if drag.source.version() >= 3 {
                drag.source.action(action);
            }
        }
    }

    /// Hand the current primary selection to one device, or clear it there.
    ///
    /// PRIMARY has two possible owners. A Wayland client owns it by setting
    /// a `zwp_primary_selection_source_v1`, and the compositor splices
    /// `receive` straight through to it. The browser owns it by pushing
    /// bytes ([`CompositorCommand::PrimaryOffer`]), which the compositor
    /// then serves itself — the web platform exposes no PRIMARY to read on
    /// demand, so the bytes have to arrive up front rather than be fetched
    /// when a client asks.
    ///
    /// At most one of the two holds it at a time: taking PRIMARY on either
    /// side clears the other, so precedence is never ambiguous.
    fn offer_primary_to(&self, pd: &ZwpPrimarySelectionDeviceV1) {
        let (mimes, external) = if let Some(ref cb) = self.external_primary {
            (cb.mime_types(), true)
        } else if let Some(ref src) = self.primary_source {
            let mimes = src
                .data::<PrimarySourceData>()
                .map(|d| d.mime_types.lock().unwrap().clone())
                .unwrap_or_default();
            (mimes, false)
        } else {
            pd.selection(None);
            return;
        };
        let Some(client) = pd.client() else {
            return;
        };
        let Ok(offer) = client
            .create_resource::<ZwpPrimarySelectionOfferV1, PrimaryOfferData, Compositor>(
                &self.display_handle,
                pd.version(),
                PrimaryOfferData { external },
            )
        else {
            return;
        };
        pd.data_offer(&offer);
        for mime in mimes {
            offer.offer(mime);
        }
        pd.selection(Some(&offer));
    }

    /// Push the current primary selection to every connected device.
    fn offer_primary_selection(&mut self) {
        for pd in &self.primary_devices {
            self.offer_primary_to(pd);
        }
        let _ = self.display_handle.flush_clients();
    }
}

// -- zwp_primary_selection --

impl GlobalDispatch<ZwpPrimarySelectionDeviceManagerV1, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwpPrimarySelectionDeviceManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceManagerV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpPrimarySelectionDeviceManagerV1,
        request: <ZwpPrimarySelectionDeviceManagerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwp_primary_selection_device_manager_v1::Request;
        match request {
            Request::CreateSource { id } => {
                data_init.init(
                    id,
                    PrimarySourceData {
                        mime_types: std::sync::Mutex::new(Vec::new()),
                    },
                );
            }
            Request::GetDevice { id, seat: _ } => {
                let pd = data_init.init(id, ());
                // Deferred to keyboard focus for the reason `GetDataDevice`
                // spells out; a client already holding focus is safe to
                // answer straight away.
                if state
                    .keyboard_focus_wl()
                    .is_some_and(|wl| same_client(&pd, &wl))
                {
                    state.offer_primary_to(&pd);
                }
                state.primary_devices.push(pd);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwpPrimarySelectionSourceV1, PrimarySourceData> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwpPrimarySelectionSourceV1,
        request: <ZwpPrimarySelectionSourceV1 as Resource>::Request,
        data: &PrimarySourceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use zwp_primary_selection_source_v1::Request;
        match request {
            Request::Offer { mime_type } => {
                data.mime_types.lock().unwrap().push(mime_type);
            }
            Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _: wayland_server::backend::ClientId,
        resource: &ZwpPrimarySelectionSourceV1,
        _: &PrimarySourceData,
    ) {
        if state
            .primary_source
            .as_ref()
            .is_some_and(|s| s.id() == resource.id())
        {
            state.primary_source = None;
            // The offers pointing at it are now unbacked; withdraw them
            // rather than leave clients pasting from a dead source.
            state.offer_primary_selection();
        }
    }
}

impl Dispatch<ZwpPrimarySelectionDeviceV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpPrimarySelectionDeviceV1,
        request: <ZwpPrimarySelectionDeviceV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use zwp_primary_selection_device_v1::Request;
        match request {
            Request::SetSelection { source, serial: _ } => {
                // The displaced owner is told it lost the selection, so it
                // can drop whatever it was holding to serve it.
                if let Some(prev) = state.primary_source.take()
                    && source.as_ref().is_none_or(|s| s.id() != prev.id())
                {
                    prev.cancelled();
                }
                // A Wayland client taking PRIMARY displaces the browser's
                // copy, keeping the two owners mutually exclusive.  Leaving
                // it would shadow the client that just claimed it, since
                // the external selection is served in preference.
                state.external_primary = None;
                state.primary_source = source;
                state.offer_primary_selection();
            }
            Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _: wayland_server::backend::ClientId,
        resource: &ZwpPrimarySelectionDeviceV1,
        _: &(),
    ) {
        state.primary_devices.retain(|d| d.id() != resource.id());
    }
}

impl Dispatch<ZwpPrimarySelectionOfferV1, PrimaryOfferData> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpPrimarySelectionOfferV1,
        request: <ZwpPrimarySelectionOfferV1 as Resource>::Request,
        data: &PrimaryOfferData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use zwp_primary_selection_offer_v1::Request;
        match request {
            Request::Receive { mime_type, fd } => {
                if data.external {
                    if let Some(ref cb) = state.external_primary
                        && cb.offers(&mime_type)
                    {
                        use std::io::Write;
                        let mut f = std::fs::File::from(fd);
                        let _ = f.write_all(&cb.data);
                    }
                } else if let Some(ref src) = state.primary_source {
                    src.send(mime_type, fd.as_fd());
                }
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

// -- zwp_pointer_constraints_v1 --

impl GlobalDispatch<ZwpPointerConstraintsV1, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwpPointerConstraintsV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpPointerConstraintsV1, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwpPointerConstraintsV1,
        request: <ZwpPointerConstraintsV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwp_pointer_constraints_v1::Request;
        match request {
            Request::LockPointer {
                id,
                surface: _,
                pointer: _,
                region: _,
                lifetime: _,
            } => {
                let lp = data_init.init(id, ());
                // Immediately grant the lock (headless — no physical pointer to contest).
                lp.locked();
            }
            Request::ConfinePointer {
                id,
                surface: _,
                pointer: _,
                region: _,
                lifetime: _,
            } => {
                let cp = data_init.init(id, ());
                cp.confined();
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwpLockedPointerV1,
        _: <ZwpLockedPointerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // SetCursorPositionHint, SetRegion, Destroy — no-ops for headless.
    }
}

impl Dispatch<ZwpConfinedPointerV1, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ZwpConfinedPointerV1,
        _: <ZwpConfinedPointerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // SetRegion, Destroy — no-ops for headless.
    }
}

// -- zwp_relative_pointer_manager_v1 --

impl GlobalDispatch<ZwpRelativePointerManagerV1, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwpRelativePointerManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpRelativePointerManagerV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpRelativePointerManagerV1,
        request: <ZwpRelativePointerManagerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwp_relative_pointer_manager_v1::Request;
        match request {
            Request::GetRelativePointer { id, pointer: _ } => {
                let rp = data_init.init(id, ());
                state.relative_pointers.push(rp);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &ZwpRelativePointerV1,
        _: <ZwpRelativePointerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // Only request is Destroy.
        state
            .relative_pointers
            .retain(|rp| rp.id() != resource.id());
    }
}

// -- zwp_text_input_v3 --

impl GlobalDispatch<ZwpTextInputManagerV3, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwpTextInputManagerV3>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpTextInputManagerV3,
        request: <ZwpTextInputManagerV3 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwp_text_input_manager_v3::Request;
        match request {
            Request::GetTextInput { id, seat: _ } => {
                let ti = data_init.init(id, ());
                let entered_surface = state
                    .keyboard_focus_wl()
                    .filter(|focused| same_client(&ti, focused));
                if let Some(ref focused) = entered_surface {
                    ti.enter(focused);
                }
                state.text_inputs.push(TextInputState {
                    resource: ti,
                    entered_surface,
                    enabled: false,
                    pending_enabled: false,
                    pending_enabled_changed: false,
                    pending_show_requested: false,
                    content_hint: 0,
                    content_purpose: 0,
                    pending_content_hint: 0,
                    pending_content_purpose: 0,
                    pending_content_type_changed: false,
                    cursor_rect: None,
                    pending_cursor_rect: None,
                    preedit_shown: false,
                    commits: 0,
                });
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwpTextInputV3, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &ZwpTextInputV3,
        request: <ZwpTextInputV3 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use zwp_text_input_v3::Request;
        if matches!(request, Request::Destroy) {
            let removed = state
                .text_inputs
                .iter()
                .find(|t| t.resource.id() == resource.id())
                .and_then(|ti| ti.enabled.then(|| ti.entered_surface.clone()).flatten());
            state
                .text_inputs
                .retain(|t| t.resource.id() != resource.id());
            if let Some(entered) = removed
                && let Some(surface_id) = state.find_toplevel_root(&entered.id()).1
            {
                state.emit_surface_text_input(surface_id, false, false, 0, 0, None);
            }
            return;
        }
        let Some(index) = state
            .text_inputs
            .iter()
            .position(|t| t.resource.id() == resource.id())
        else {
            return;
        };
        let mut committed = None;
        match request {
            // enable/disable are double-buffered: they name the pending
            // state, and only the commit below promotes it.
            Request::Enable => {
                let ti = &mut state.text_inputs[index];
                ti.pending_enabled = true;
                ti.pending_enabled_changed = true;
                ti.pending_show_requested = true;
                // Enable resets all text-input state. SetContentType may
                // follow in the same pending batch and replace these.
                ti.pending_content_hint = 0;
                ti.pending_content_purpose = 0;
                ti.pending_content_type_changed = true;
                ti.pending_cursor_rect = None;
                // enable "resets all state associated with ... preedit_string",
                // so the client is about to be showing nothing.
                ti.preedit_shown = false;
            }
            Request::Disable => {
                let ti = &mut state.text_inputs[index];
                ti.pending_enabled = false;
                ti.pending_enabled_changed = true;
                ti.pending_show_requested = false;
                ti.pending_content_hint = 0;
                ti.pending_content_purpose = 0;
                ti.pending_content_type_changed = true;
                ti.pending_cursor_rect = None;
            }
            Request::SetContentType { hint, purpose } => {
                use wayland_server::WEnum;
                let hint = match hint {
                    WEnum::Value(hint) => hint.bits(),
                    WEnum::Unknown(raw) => raw,
                };
                let purpose = match purpose {
                    WEnum::Value(purpose) => purpose as u32,
                    WEnum::Unknown(raw) => raw,
                };
                let ti = &mut state.text_inputs[index];
                ti.pending_content_hint = hint;
                ti.pending_content_purpose = purpose;
                ti.pending_content_type_changed = true;
            }
            // Where the app is drawing the text under edit.  The browser
            // parks its hidden IME capture element over this rectangle, so
            // the host's candidate window opens at the app's caret instead
            // of the corner of the screen.
            Request::SetCursorRectangle {
                x,
                y,
                width,
                height,
            } => {
                state.text_inputs[index].pending_cursor_rect = Some((x, y, width, height));
            }
            Request::Commit => {
                let ti = &mut state.text_inputs[index];
                // The commit count advances even while no surface is entered;
                // it is the serial required by any later `done` event.
                ti.commits = ti.commits.wrapping_add(1);
                // A moved caret is worth a message of its own, but only while
                // the input is on: an off input has nowhere to draw it, and
                // re-broadcasting "disabled" on every stray commit is noise.
                // Compare against the committed value rather than tracking a
                // dirty flag — apps re-send the same rectangle on every
                // keystroke, and each one would otherwise wake every viewer.
                let rect_changed = ti.pending_enabled && ti.pending_cursor_rect != ti.cursor_rect;
                let changed =
                    ti.pending_enabled_changed || ti.pending_content_type_changed || rect_changed;
                let requested = ti.pending_show_requested;
                ti.pending_enabled_changed = false;
                ti.pending_show_requested = false;
                ti.pending_content_type_changed = false;

                // Requests after leave are ignored until the next enter.
                if let Some(entered) = ti.entered_surface.clone() {
                    ti.enabled = ti.pending_enabled;
                    ti.content_hint = ti.pending_content_hint;
                    ti.content_purpose = ti.pending_content_purpose;
                    ti.cursor_rect = ti.pending_cursor_rect;
                    if changed {
                        committed = Some((
                            entered,
                            ti.enabled,
                            requested && ti.enabled,
                            ti.content_hint,
                            ti.content_purpose,
                            ti.enabled.then_some(ti.cursor_rect).flatten(),
                        ));
                    }
                }
            }
            // SetSurroundingText, SetTextChangeCause — informational to the
            // browser keyboard path; ignored for now.
            _ => {}
        }
        if let Some((entered, enabled, requested, hint, purpose, cursor_rect)) = committed
            && let Some(surface_id) = state.find_toplevel_root(&entered.id()).1
        {
            state.emit_surface_text_input(
                surface_id,
                enabled,
                requested,
                hint,
                purpose,
                cursor_rect,
            );
        }
    }
}

// -- xdg_activation_v1 --

impl GlobalDispatch<XdgActivationV1, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<XdgActivationV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<XdgActivationV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &XdgActivationV1,
        request: <XdgActivationV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use xdg_activation_v1::Request;
        match request {
            Request::GetActivationToken { id } => {
                let serial = state.next_activation_token;
                state.next_activation_token = serial.wrapping_add(1);
                data_init.init(id, ActivationTokenData { serial });
            }
            Request::Activate { token: _, surface } => {
                // Tokens are issued unvalidated, so there is nothing to
                // check here.  Pane focus is managed externally by the
                // browser/CLI, but "always granted" must not mean "silently
                // dropped": forward the request so the frontend can raise
                // and focus the matching pane.  An ignored activation is
                // what strands an Electron app that asks to come back
                // (Slack on a notification click) behind everything else.
                if let Some(surf) = state.surfaces.get(&surface.id())
                    && surf.surface_id > 0
                {
                    let _ = state.event_tx.send(CompositorEvent::SurfaceActivated {
                        surface_id: surf.surface_id,
                    });
                    (state.event_notify)();
                }
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<XdgActivationTokenV1, ActivationTokenData> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        resource: &XdgActivationTokenV1,
        request: <XdgActivationTokenV1 as Resource>::Request,
        data: &ActivationTokenData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use xdg_activation_token_v1::Request;
        match request {
            Request::Commit => {
                // Issue a token immediately — the headless compositor doesn't
                // need to validate app_id / surface / serial.
                resource.done(format!("blit-token-{}", data.serial));
            }
            Request::SetSerial { .. } | Request::SetAppId { .. } | Request::SetSurface { .. } => {}
            Request::Destroy => {}
            _ => {}
        }
    }
}

// -- wp_cursor_shape_manager_v1 --

impl GlobalDispatch<WpCursorShapeManagerV1, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WpCursorShapeManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WpCursorShapeManagerV1,
        request: <WpCursorShapeManagerV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use wp_cursor_shape_manager_v1::Request;
        match request {
            Request::GetPointer {
                cursor_shape_device,
                pointer: _,
            } => {
                data_init.init(cursor_shape_device, ());
            }
            Request::GetTabletToolV2 {
                cursor_shape_device,
                tablet_tool: _,
            } => {
                data_init.init(cursor_shape_device, ());
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WpCursorShapeDeviceV1,
        request: <WpCursorShapeDeviceV1 as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wp_cursor_shape_device_v1::Request;
        match request {
            Request::SetShape { serial: _, shape } => {
                use wayland_server::WEnum;
                use wp_cursor_shape_device_v1::Shape;
                let name = match shape {
                    WEnum::Value(Shape::Default) => "default",
                    WEnum::Value(Shape::ContextMenu) => "context-menu",
                    WEnum::Value(Shape::Help) => "help",
                    WEnum::Value(Shape::Pointer) => "pointer",
                    WEnum::Value(Shape::Progress) => "progress",
                    WEnum::Value(Shape::Wait) => "wait",
                    WEnum::Value(Shape::Cell) => "cell",
                    WEnum::Value(Shape::Crosshair) => "crosshair",
                    WEnum::Value(Shape::Text) => "text",
                    WEnum::Value(Shape::VerticalText) => "vertical-text",
                    WEnum::Value(Shape::Alias) => "alias",
                    WEnum::Value(Shape::Copy) => "copy",
                    WEnum::Value(Shape::Move) => "move",
                    WEnum::Value(Shape::NoDrop) => "no-drop",
                    WEnum::Value(Shape::NotAllowed) => "not-allowed",
                    WEnum::Value(Shape::Grab) => "grab",
                    WEnum::Value(Shape::Grabbing) => "grabbing",
                    WEnum::Value(Shape::EResize) => "e-resize",
                    WEnum::Value(Shape::NResize) => "n-resize",
                    WEnum::Value(Shape::NeResize) => "ne-resize",
                    WEnum::Value(Shape::NwResize) => "nw-resize",
                    WEnum::Value(Shape::SResize) => "s-resize",
                    WEnum::Value(Shape::SeResize) => "se-resize",
                    WEnum::Value(Shape::SwResize) => "sw-resize",
                    WEnum::Value(Shape::WResize) => "w-resize",
                    WEnum::Value(Shape::EwResize) => "ew-resize",
                    WEnum::Value(Shape::NsResize) => "ns-resize",
                    WEnum::Value(Shape::NeswResize) => "nesw-resize",
                    WEnum::Value(Shape::NwseResize) => "nwse-resize",
                    WEnum::Value(Shape::ColResize) => "col-resize",
                    WEnum::Value(Shape::RowResize) => "row-resize",
                    WEnum::Value(Shape::AllScroll) => "all-scroll",
                    WEnum::Value(Shape::ZoomIn) => "zoom-in",
                    WEnum::Value(Shape::ZoomOut) => "zoom-out",
                    _ => "default",
                };
                let _ = state.event_tx.send(CompositorEvent::SurfaceCursor {
                    surface_id: state.cursor_target_sid(),
                    cursor: CursorImage::Named(name.to_string()),
                });
                (state.event_notify)();
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

// -- Client data --
impl wayland_server::backend::ClientData for ClientState {
    fn initialized(&self, _: wayland_server::backend::ClientId) {}
    fn disconnected(
        &self,
        _: wayland_server::backend::ClientId,
        _: wayland_server::backend::DisconnectReason,
    ) {
        self.cleanup_needed.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct CompositorHandle {
    pub event_rx: mpsc::Receiver<CompositorEvent>,
    pub command_tx: mpsc::Sender<CompositorCommand>,
    pub socket_name: String,
    /// Whether the compositor's Vulkan renderer supports Vulkan Video encode.
    pub vulkan_video_encode: bool,
    /// Whether the compositor's Vulkan renderer supports Vulkan Video AV1 encode.
    pub vulkan_video_encode_av1: bool,
    foreign_exports: Arc<RwLock<HashMap<String, u16>>>,
    thread: std::thread::JoinHandle<()>,
    frame_clock_thread: std::thread::JoinHandle<()>,
    frame_clock_tx: mpsc::Sender<FrameClockCommand>,
    frame_clock_requests: Arc<AtomicU32>,
    shutdown: Arc<AtomicBool>,
    loop_signal: LoopSignal,
}

impl CompositorHandle {
    pub fn wake(&self) {
        self.loop_signal.wakeup();
    }

    /// Drive one surface's Wayland frame callbacks from a clock that is
    /// independent of server encode and network-delivery work.
    pub fn set_frame_interval(&self, surface_id: u16, interval: Option<std::time::Duration>) {
        let _ = self.frame_clock_tx.send(FrameClockCommand::Set {
            surface_id,
            interval,
        });
    }

    /// Number of fixed-clock frame requests emitted since the last call.
    pub fn take_frame_clock_requests(&self) -> u32 {
        self.frame_clock_requests.swap(0, Ordering::Relaxed)
    }

    /// Resolve a portal `wayland:<xdg-foreign-v2 handle>` to a live semantic
    /// toplevel ID. Empty, malformed, expired, and non-Wayland parents fail
    /// closed to the portal authority fallback.
    pub fn resolve_foreign_parent(&self, parent: &str) -> Option<u16> {
        let handle = parent.strip_prefix("wayland:")?;
        if handle.len() != 32 || !handle.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        self.foreign_exports.read().ok()?.get(handle).copied()
    }

    /// Stop the compositor and wait for it to finish tearing down.
    ///
    /// Simply dropping the handle leaves the compositor running instead --
    /// there is no orderly stop without calling this.
    pub fn stop(self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.frame_clock_tx.send(FrameClockCommand::Shutdown);
        self.loop_signal.wakeup();
        let _ = self.frame_clock_thread.join();
        let _ = self.thread.join();
    }
}

enum FrameClockCommand {
    Set {
        surface_id: u16,
        interval: Option<std::time::Duration>,
    },
    Shutdown,
}

#[derive(Clone, Copy)]
struct FrameClockEntry {
    interval: std::time::Duration,
    next: std::time::Instant,
}

fn update_frame_clock(
    clocks: &mut FxHashMap<u16, FrameClockEntry>,
    surface_id: u16,
    interval: Option<std::time::Duration>,
) {
    let Some(interval) = interval.filter(|interval| !interval.is_zero()) else {
        clocks.remove(&surface_id);
        return;
    };
    let now = std::time::Instant::now();
    match clocks.get_mut(&surface_id) {
        Some(clock) if clock.interval == interval => {}
        Some(clock) => {
            clock.interval = interval;
            clock.next = now;
        }
        None => {
            clocks.insert(
                surface_id,
                FrameClockEntry {
                    interval,
                    next: now,
                },
            );
        }
    }
}

fn consume_frame_clock_deadline(
    deadline: &mut std::time::Instant,
    now: std::time::Instant,
    interval: std::time::Duration,
) -> std::time::Instant {
    let elapsed = now.saturating_duration_since(*deadline).as_nanos() / interval.as_nanos();
    let elapsed = u32::try_from(elapsed).unwrap_or(u32::MAX);
    let presentation_at = deadline
        .checked_add(interval.saturating_mul(elapsed))
        .unwrap_or(now);
    *deadline = presentation_at
        .checked_add(interval)
        .unwrap_or(now + interval);
    presentation_at
}

fn run_frame_clock(
    rx: mpsc::Receiver<FrameClockCommand>,
    command_tx: mpsc::Sender<CompositorCommand>,
    loop_signal: LoopSignal,
    requests: Arc<AtomicU32>,
    shutdown: Arc<AtomicBool>,
) {
    let mut clocks: FxHashMap<u16, FrameClockEntry> = FxHashMap::default();
    loop {
        let now = std::time::Instant::now();
        let wait = clocks
            .values()
            .map(|clock| clock.next)
            .min()
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or(std::time::Duration::from_secs(3600));
        match rx.recv_timeout(wait) {
            Ok(FrameClockCommand::Set {
                surface_id,
                interval,
            }) => update_frame_clock(&mut clocks, surface_id, interval),
            Ok(FrameClockCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        // Coalesce a burst of subscription/rate updates before deciding
        // which deadlines are due.
        while let Ok(command) = rx.try_recv() {
            match command {
                FrameClockCommand::Set {
                    surface_id,
                    interval,
                } => update_frame_clock(&mut clocks, surface_id, interval),
                FrameClockCommand::Shutdown => return,
            }
        }

        let now = std::time::Instant::now();
        let mut sent = 0u32;
        for (&surface_id, clock) in &mut clocks {
            if clock.next > now {
                continue;
            }
            let presentation_at =
                consume_frame_clock_deadline(&mut clock.next, now, clock.interval);
            if command_tx
                .send(CompositorCommand::RequestFrame {
                    surface_id,
                    presentation_at,
                })
                .is_ok()
            {
                sent = sent.saturating_add(1);
            }
        }
        if sent > 0 {
            requests.fetch_add(sent, Ordering::Relaxed);
            loop_signal.wakeup();
        }
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
    }
}

pub fn spawn_compositor(
    verbose: bool,
    event_notify: Arc<dyn Fn() + Send + Sync>,
    gpu_device: &str,
) -> CompositorHandle {
    spawn_compositor_inner(verbose, event_notify, gpu_device, true)
}

/// Start a compositor without probing or initializing a renderer.
///
/// This is intended for protocol tests whose assertions only depend on
/// Wayland state and events. Keeping those tests off Vulkan avoids creating a
/// GPU device per test, which is both expensive and prone to driver contention
/// when the Rust test harness runs them concurrently.
#[doc(hidden)]
pub fn spawn_compositor_without_renderer(
    verbose: bool,
    event_notify: Arc<dyn Fn() + Send + Sync>,
) -> CompositorHandle {
    spawn_compositor_inner(verbose, event_notify, "", false)
}

fn spawn_compositor_inner(
    verbose: bool,
    event_notify: Arc<dyn Fn() + Send + Sync>,
    gpu_device: &str,
    enable_renderer: bool,
) -> CompositorHandle {
    let _gpu_device = gpu_device.to_string();
    let (event_tx, event_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let (socket_tx, socket_rx) = mpsc::sync_channel(1);
    let (signal_tx, signal_rx) = mpsc::sync_channel::<LoopSignal>(1);
    let (caps_tx, caps_rx) = mpsc::sync_channel::<(bool, bool)>(1);
    let foreign_exports = Arc::new(RwLock::new(HashMap::new()));
    let compositor_foreign_exports = foreign_exports.clone();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|p| {
            let probe = p.join(".blit-probe");
            if std::fs::write(&probe, b"").is_ok() {
                let _ = std::fs::remove_file(&probe);
                true
            } else {
                false
            }
        })
        .unwrap_or_else(std::env::temp_dir);

    let runtime_dir_clone = runtime_dir.clone();
    let thread = std::thread::Builder::new()
        .name("compositor".into())
        .spawn(move || {
            unsafe { std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir_clone) };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_compositor(
                    event_tx,
                    command_rx,
                    socket_tx,
                    signal_tx,
                    caps_tx,
                    event_notify,
                    shutdown_clone,
                    verbose,
                    _gpu_device,
                    enable_renderer,
                    compositor_foreign_exports,
                );
            }));
            if let Err(e) = result {
                let msg = if let Some(s) = e.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = e.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                eprintln!("[compositor] PANIC: {msg}");
            }
        })
        .expect("failed to spawn compositor thread");

    let socket_name = socket_rx.recv().expect("compositor failed to start");
    let socket_name = runtime_dir
        .join(&socket_name)
        .to_string_lossy()
        .into_owned();
    let loop_signal = signal_rx
        .recv()
        .expect("compositor failed to send loop signal");
    let (vulkan_video_encode, vulkan_video_encode_av1) = caps_rx.recv().unwrap_or((false, false));
    let (frame_clock_tx, frame_clock_rx) = mpsc::channel();
    let frame_clock_requests = Arc::new(AtomicU32::new(0));
    let frame_clock_thread = {
        let command_tx = command_tx.clone();
        let loop_signal = loop_signal.clone();
        let requests = frame_clock_requests.clone();
        let shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("compositor-frame-clock".into())
            .spawn(move || {
                run_frame_clock(frame_clock_rx, command_tx, loop_signal, requests, shutdown);
            })
            .expect("failed to spawn compositor frame clock")
    };

    CompositorHandle {
        event_rx,
        command_tx,
        socket_name,
        thread,
        frame_clock_thread,
        frame_clock_tx,
        frame_clock_requests,
        shutdown,
        vulkan_video_encode,
        vulkan_video_encode_av1,
        foreign_exports,
        loop_signal,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_compositor(
    event_tx: mpsc::Sender<CompositorEvent>,
    command_rx: mpsc::Receiver<CompositorCommand>,
    socket_tx: mpsc::SyncSender<String>,
    signal_tx: mpsc::SyncSender<LoopSignal>,
    caps_tx: mpsc::SyncSender<(bool, bool)>,
    event_notify: Arc<dyn Fn() + Send + Sync>,
    shutdown: Arc<AtomicBool>,
    verbose: bool,
    gpu_device: String,
    enable_renderer: bool,
    foreign_exports: Arc<RwLock<HashMap<String, u16>>>,
) {
    let mut event_loop: EventLoop<Compositor> =
        EventLoop::try_new().expect("failed to create event loop");
    let loop_signal = event_loop.get_signal();

    let display: Display<Compositor> = Display::new().expect("failed to create display");
    let dh = display.handle();

    // Probe Vulkan early so we know whether DMA-BUF is available
    // before registering Wayland globals.
    if enable_renderer {
        eprintln!("[compositor] trying Vulkan renderer for {gpu_device}");
    }
    let vulkan_renderer = enable_renderer
        .then(|| super::vulkan_render::VulkanRenderer::try_new(&gpu_device))
        .flatten();
    let has_dmabuf = vulkan_renderer.as_ref().is_some_and(|vk| vk.has_dmabuf());
    eprintln!(
        "[compositor] Vulkan renderer: {} (dmabuf={})",
        vulkan_renderer.is_some(),
        has_dmabuf,
    );
    if enable_renderer && vulkan_renderer.is_none() {
        eprintln!(
            "[compositor] WARNING: no Vulkan renderer — clients can connect but NO frames will be composited (windows will never appear)."
        );
        eprintln!(
            "[compositor] WARNING: install a Vulkan driver; on GPU-less hosts a software driver works (e.g. `apt install mesa-vulkan-drivers` for lavapipe)."
        );
    }

    // Create globals.
    dh.create_global::<Compositor, WlCompositor, ()>(6, ());
    dh.create_global::<Compositor, WlSubcompositor, ()>(1, ());
    dh.create_global::<Compositor, XdgWmBase, ()>(6, ());
    dh.create_global::<Compositor, ZxdgExporterV2, ()>(1, ());
    dh.create_global::<Compositor, WlShm, ()>(1, ());
    // No session-wide output: a screen is published per client on connect
    // and per toplevel thereafter, each visible only to its owner.
    dh.create_global::<Compositor, WlSeat, ()>(9, ());
    // Only advertise zwp_linux_dmabuf_v1 when the Vulkan device can
    // actually import DMA-BUFs.  Advertising the global with zero
    // formats confuses clients (Chrome, mpv) into not falling back to
    // wl_shm.
    if has_dmabuf {
        dh.create_global::<Compositor, ZwpLinuxDmabufV1, ()>(4, ());
    }
    // Explicit sync: only meaningful with DMA-BUF, and only when the
    // kernel driver supports timeline syncobjs.  Without this global,
    // NVIDIA clients fall back to implicit fencing the driver does not
    // implement, and their GPU writes race our sampling.
    let syncobj_device = if has_dmabuf {
        crate::drm_syncobj::DrmSyncobjDevice::open(&gpu_device)
    } else {
        None
    };
    if syncobj_device.is_some() {
        dh.create_global::<Compositor, WpLinuxDrmSyncobjManagerV1, ()>(1, ());
        eprintln!("[compositor] explicit sync (wp_linux_drm_syncobj_v1) enabled");
    }
    dh.create_global::<Compositor, WpViewporter, ()>(1, ());
    dh.create_global::<Compositor, WpFractionalScaleManagerV1, ()>(1, ());
    dh.create_global::<Compositor, ZxdgDecorationManagerV1, ()>(1, ());
    dh.create_global::<Compositor, WlDataDeviceManager, ()>(3, ());
    dh.create_global::<Compositor, XdgToplevelDragManagerV1, ()>(1, ());
    dh.create_global::<Compositor, ZwpPointerConstraintsV1, ()>(1, ());
    dh.create_global::<Compositor, ZwpRelativePointerManagerV1, ()>(1, ());
    dh.create_global::<Compositor, XdgActivationV1, ()>(1, ());
    dh.create_global::<Compositor, WpCursorShapeManagerV1, ()>(1, ());
    dh.create_global::<Compositor, ZwpPrimarySelectionDeviceManagerV1, ()>(1, ());
    dh.create_global::<Compositor, WpPresentation, ()>(1, ());
    dh.create_global::<Compositor, ZwpTextInputManagerV3, ()>(1, ());

    // XKB keymap.
    let keymap_string = include_str!("../data/us-qwerty.xkb");
    let mut keymap_data = keymap_string.as_bytes().to_vec();
    keymap_data.push(0); // null-terminate

    // Listening socket.
    let listening_socket = wayland_server::ListeningSocket::bind_auto("wayland", 0..33)
        .unwrap_or_else(|e| {
            let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "(unset)".into());
            panic!("failed to create wayland socket in XDG_RUNTIME_DIR={dir}: {e}\nhint: ensure the directory exists and is writable by the current user");
        });
    let socket_name = listening_socket
        .socket_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    socket_tx.send(socket_name).unwrap();
    let _ = signal_tx.send(loop_signal.clone());

    let cleanup_needed = Arc::new(AtomicBool::new(false));
    let mut compositor = Compositor {
        display_handle: dh,
        cleanup_needed: Arc::clone(&cleanup_needed),
        surfaces: FxHashMap::default(),
        regions: FxHashMap::default(),
        toplevel_surface_ids: FxHashMap::default(),
        foreign_exports,
        foreign_export_objects: FxHashMap::default(),
        screencast_surfaces: FxHashSet::default(),
        last_request_frame_ms: FxHashMap::default(),
        last_topless_frame_ms: FxHashMap::default(),
        pending_request_frames: FxHashMap::default(),
        frame_callback_toplevels: FxHashSet::default(),
        client_identity: FxHashMap::default(),
        next_surface_id: 1,
        shm_pools: FxHashMap::default(),
        surface_meta: FxHashMap::default(),
        dmabuf_params: FxHashMap::default(),
        vulkan_renderer,
        output_width: 1920,
        output_height: 1080,
        output_refresh_mhz: 60_000,
        surface_scales: FxHashMap::default(),
        output_slots: FxHashMap::default(),
        retired_output_globals: Vec::new(),
        next_output_slot: 0,
        xwayland_pid: None,
        client_pids: FxHashMap::default(),
        xwayland_clients: FxHashSet::default(),
        outputs: Vec::new(),
        seats: Vec::new(),
        keyboards: Vec::new(),
        pointers: Vec::new(),
        touches: Vec::new(),
        touch_enabled: false,
        touch_pacer: TouchPacer::default(),
        input_time_anchor: None,
        touch_time_anchor: None,
        touch_time_last_arrival: None,
        last_input_time: None,
        active_touches: HashMap::new(),
        keyboard_keymap_data: keymap_data,
        mods_depressed: 0,
        mods_locked: 0,
        serial: 0,
        event_tx,
        event_notify,
        loop_signal: loop_signal.clone(),
        pending_commits: HashMap::new(),
        pending_encoded: Vec::new(),
        pending_native_sizes: FxHashMap::default(),
        pending_recomposite_toplevels: FxHashMap::default(),
        deferred_buffer_holds: FxHashMap::default(),
        focused_surface_id: 0,
        pointer_entered_id: None,
        pointer_entered_local: (0.0, 0.0),
        pointer_frame_positions: FxHashMap::default(),
        current_cursor_surface: None,
        pending_kb_reenter: false,
        gpu_device,
        verbose,
        shutdown: shutdown.clone(),
        last_reported_size: FxHashMap::default(),
        surface_sizes: FxHashMap::default(),
        positioners: FxHashMap::default(),
        fractional_scales: Vec::new(),
        data_devices: Vec::new(),
        selection_source: None,
        external_clipboard: None,
        drag: None,
        client_drag: None,
        toplevel_drags: Vec::new(),
        primary_devices: Vec::new(),
        primary_source: None,
        external_primary: None,
        relative_pointers: Vec::new(),
        axis_scale: HashMap::new(),
        text_inputs: Vec::new(),
        next_activation_token: 1,
        popup_grab_stack: Vec::new(),
        kb_focus_popup: None,
        popup_dismiss_button: None,
        held_buffers: FxHashMap::default(),
        syncobj_device,
        syncobj_timelines: FxHashMap::default(),
        awaiting_acquire: FxHashMap::default(),
        cursor_rgba: FxHashMap::default(),
        last_cursor: FxHashMap::default(),
    };

    // Report Vulkan Video encode capabilities to the server.
    {
        let (vve, vve_av1) = compositor
            .vulkan_renderer
            .as_ref()
            .map(|vk| (vk.has_video_encode(), vk.has_video_encode_av1()))
            .unwrap_or((false, false));
        let _ = caps_tx.send((vve, vve_av1));
    }

    let handle = event_loop.handle();
    // Every per-client disconnect monitor watches the read half.  Dropping the
    // sole write half on any run_compositor exit wakes them all, so their
    // cloned client sockets cannot outlive the compositor Display.
    let (monitor_cancel_read, _monitor_cancel_write) =
        std::os::unix::net::UnixStream::pair().expect("failed to create client-monitor cancel fd");

    // Insert display fd source.
    let display_source = Generic::new(display, Interest::READ, calloop::Mode::Level);
    handle
        .insert_source(display_source, |_, display, state| {
            let d = unsafe { display.get_mut() };
            if let Err(e) = d.dispatch_clients(state)
                && state.verbose
            {
                eprintln!("[compositor] dispatch_clients error: {e}");
            }
            if let Err(e) = d.flush_clients()
                && state.verbose
            {
                eprintln!("[compositor] flush_clients error: {e}");
            }
            Ok(PostAction::Continue)
        })
        .expect("failed to insert display source");

    // Insert listening socket.
    let socket_source = Generic::new(listening_socket, Interest::READ, calloop::Mode::Level);
    let monitor_cancel = monitor_cancel_read
        .try_clone()
        .expect("failed to clone client-monitor cancel fd");
    handle
        .insert_source(socket_source, move |_, socket, state| {
            let ls = unsafe { socket.get_mut() };
            if let Some(client_stream) = ls.accept().ok().flatten() {
                // The shared socket says nothing about who connected: anything
                // that inherited WAYLAND_DISPLAY can reach it.
                accept_client(state, client_stream, None, &monitor_cancel);
            }
            Ok(PostAction::Continue)
        })
        .expect("failed to insert listening socket");

    if verbose {
        eprintln!("[compositor] entering event loop");
    }

    while !shutdown.load(Ordering::Relaxed) {
        // Process commands.
        let mut drained_wayland_before_frame = false;
        while let Ok(cmd) = command_rx.try_recv() {
            // A client commonly queues its next wl_surface.frame request at
            // the same time the server's display-rate tick wakes this loop.
            // Dispatch already-readable Wayland requests before the first
            // RequestFrame in the batch; otherwise the tick can observe no
            // callback, then dispatch it immediately afterwards and leave it
            // waiting for the following refresh interval.
            if matches!(cmd, CompositorCommand::RequestFrame { .. })
                && !drained_wayland_before_frame
            {
                if let Err(e) =
                    event_loop.dispatch(Some(std::time::Duration::ZERO), &mut compositor)
                    && verbose
                {
                    eprintln!("[compositor] pre-frame event loop error: {e}");
                }
                compositor.cleanup_disconnected_clients();
                drained_wayland_before_frame = true;
            }
            match cmd {
                CompositorCommand::Shutdown => {
                    shutdown.store(true, Ordering::Relaxed);
                    return;
                }
                // Handled here rather than in `handle_command` because it needs
                // the event loop's handle to register a new source, which a
                // `&mut Compositor` method has no access to.
                CompositorCommand::AddAppSocket { fd, identity } => {
                    let listener = std::os::unix::net::UnixListener::from(fd);
                    if let Err(e) = listener.set_nonblocking(true) {
                        eprintln!("[compositor] app socket set_nonblocking failed: {e}");
                        continue;
                    }
                    let identity = Arc::new(identity);
                    let label = identity.app_id.clone();
                    let Ok(cancel) = monitor_cancel_read.try_clone() else {
                        eprintln!("[compositor] app socket {label}: cancel fd clone failed");
                        continue;
                    };
                    let source = Generic::new(listener, Interest::READ, calloop::Mode::Level);
                    let inserted = handle.insert_source(source, move |_, listener, state| {
                        // Level-triggered, so one accept per readiness is
                        // enough; a second pending connection wakes us again.
                        match unsafe { listener.get_mut() }.accept() {
                            Ok((client_stream, _)) => {
                                accept_client(
                                    state,
                                    client_stream,
                                    Some(Arc::clone(&identity)),
                                    &cancel,
                                );
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(e) if state.verbose => {
                                eprintln!("[compositor] app socket accept error: {e}");
                            }
                            Err(_) => {}
                        }
                        Ok(PostAction::Continue)
                    });
                    match inserted {
                        Ok(_token) => {
                            if verbose {
                                eprintln!("[compositor] app socket registered for {label}");
                            }
                        }
                        Err(e) => {
                            eprintln!("[compositor] app socket {label} not registered: {e}");
                        }
                    }
                }
                other => compositor.handle_command(other),
            }
        }
        compositor.dispatch_due_touches();

        // Shorten the dispatch timeout when the Vulkan renderer has
        // in-flight GPU work so we poll for completion promptly.
        let mut poll_timeout = if compositor
            .vulkan_renderer
            .as_ref()
            .is_some_and(|vk| vk.has_pending())
            || !compositor.awaiting_acquire.is_empty()
        {
            std::time::Duration::from_millis(1)
        } else {
            std::time::Duration::from_secs(1)
        };
        if let Some(deadline) = compositor.next_touch_deadline() {
            poll_timeout =
                poll_timeout.min(deadline.saturating_duration_since(std::time::Instant::now()));
        }
        if let Err(e) = event_loop.dispatch(Some(poll_timeout), &mut compositor)
            && verbose
        {
            eprintln!("[compositor] event loop error: {e}");
        }
        // A touch deadline may be what ended the dispatch wait. Deliver it
        // before render/cleanup work can add jitter to Chromium's receipt time.
        compositor.dispatch_due_touches();
        // Explicit destroy handlers remove their own state. The full
        // liveness scan exists for clients that disappear without them.
        compositor.cleanup_disconnected_clients();

        // Install commits whose explicit-sync acquire points signalled
        // since the last pass, before the recomposite drain below so their
        // content reaches this pass's composite.
        compositor.promote_ready_acquires();

        // Check for completed Vulkan GPU work.  This runs independently
        // of surface commits so completed frames are flushed to the
        // server without waiting for the next Wayland event.  One submit
        // can yield multiple results (one per per-client downscale target
        // plus the native composite).
        if let Some(ref mut vk) = compositor.vulkan_renderer {
            // Deferred submits used to be cleaned up lazily at the next
            // render, which was fine when they only owned GPU objects.
            // They now carry client-visible wl_buffer releases and
            // explicit-sync release points: left undrained across an idle
            // spell, a client waiting on those points stalls its own GPU
            // work, which stalls its commits, which is what was keeping
            // the compositor busy — a standstill no page refresh can
            // break.  One batched fence probe per pass keeps them moving.
            vk.drain_deferred_submits();
            let (native, retired) = vk.try_retire_pending();
            if !retired.is_empty() {
                // Each retired result carries its surface, and scale is a
                // property of that surface.
                // Drive SurfaceResized off the size this submission actually
                // composited at, when no fresh handle_surface_commit has
                // populated pending_native_sizes.  Not off the largest
                // result: those include the per-client downscale targets,
                // and one registered before a shrink out-areas the real
                // native, so the reported size would flap between the new
                // size and the stale target — which then feeds the wrong
                // aspect back into every viewer's encode target.
                //
                // Only for a surface nobody has sized, though.  A sized one
                // has an authoritative answer in `surface_sizes`, and this
                // submission may have been in flight when a resize landed:
                // reporting what it composited at would walk the size
                // backwards to the pane the user has already left.
                if let Some((sid, nw, nh)) = native
                    && compositor.native_composite_size(sid).is_none()
                {
                    let s120_u32 = compositor.surface_scale_120(sid) as u32;
                    let log_w = (nw * 120).div_ceil(s120_u32);
                    let log_h = (nh * 120).div_ceil(s120_u32);
                    compositor
                        .pending_native_sizes
                        .entry(sid)
                        .or_insert((nw, nh, log_w, log_h));
                }
                for (sid, w, h, pixels, encoder_skip) in retired {
                    let s120_u32 = compositor.surface_scale_120(sid) as u32;
                    let log_w = (w * 120).div_ceil(s120_u32);
                    let log_h = (h * 120).div_ceil(s120_u32);
                    compositor
                        .pending_commits
                        .insert((sid, w, h), (log_w, log_h, pixels, encoder_skip));
                }
            }
        }

        // Drain deferred recomposites queued by per-client target
        // installs (Set/RegisterDownscaleTarget).  Only run when the
        // GPU pipeline is idle — `render_tree_sized` early-returns
        // when `pending_submit` is held, which would silently drop
        // the new submit and leave the freshly-installed target
        // empty.  Each recomposite submits one render, so process
        // one toplevel per iteration; remaining queued toplevels get
        // their turn after the next retire.
        let can_recomposite = compositor
            .vulkan_renderer
            .as_ref()
            .is_some_and(|vk| !vk.has_pending());
        if can_recomposite
            && let Some((&sid, &encoder_only)) =
                compositor.pending_recomposite_toplevels.iter().next()
        {
            compositor.pending_recomposite_toplevels.remove(&sid);
            if let Some(root_id) = compositor.toplevel_surface_ids.get(&sid).cloned() {
                // A full recomposite must publish pixels, but it need not
                // force the native CPU readback. Registered GPU/downscale
                // targets publish their own result; when none is usable the
                // renderer's native-readback plan falls back to BGRA. Keeping
                // the explicit request here made every commit deferred behind
                // an in-flight submit copy the full native frame to the CPU.
                compositor.composite_toplevel_into_pending(&root_id, sid, encoder_only);
                // Wake the loop so the retire path runs again
                // promptly — without an explicit wakeup the loop
                // would idle on its 1s dispatch timeout instead of
                // the 1ms has_pending poll.
                loop_signal.wakeup();
            }
            // Release whatever a deferred commit was still holding for this
            // toplevel.  The composite above has read it, so the client is
            // free to draw over it again — the same point on the timeline
            // `handle_surface_commit` releases at when it composites inline.
            //
            // Unconditional, including when the toplevel resolved to nothing:
            // then no composite will ever read these, and holding them would
            // strand the client's buffers for the life of the surface.  A
            // newer commit may have superseded an entry in the meantime, in
            // which case `apply_pending_state` already released the old
            // buffer and this releases the new one it just composited.
            if let Some(surface_ids) = compositor.deferred_buffer_holds.remove(&sid) {
                for surface_id in surface_ids {
                    // A commit may have parked on its acquire point since
                    // this hold was recorded — the held buffer is then the
                    // surface's displayed content again and must stay held
                    // until the parked buffer's promotion supersedes it.
                    if compositor.awaiting_acquire.contains_key(&surface_id) {
                        continue;
                    }
                    if let Some(held) = compositor.held_buffers.remove(&surface_id) {
                        // "Read" = the recomposite's fence has signalled,
                        // not merely submitted — same gating as the inline
                        // path.
                        compositor.release_held(held);
                    }
                }
            }
        }

        if !compositor.pending_commits.is_empty()
            || !compositor.pending_encoded.is_empty()
            || !compositor.pending_native_sizes.is_empty()
        {
            compositor.flush_pending_commits();
        }

        if let Err(e) = compositor.display_handle.flush_clients()
            && verbose
        {
            eprintln!("[compositor] flush error: {e}");
        }
    }

    if verbose {
        eprintln!("[compositor] event loop exited");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FrameClockEntry, PendingDamage, consume_frame_clock_deadline, dir_is_chromium,
        native_size_after_render, next_surface_id_after, parent_pid, scan_free_surface_id,
        shm_damage_rects, update_frame_clock,
    };
    use crate::vulkan_render::ShmDamageRect;
    use rustc_hash::FxHashMap;

    /// A scratch directory that removes itself, so the marker test can
    /// stage an executable's neighbours without a temp-file dependency.
    struct ScratchDir(std::path::PathBuf);
    impl ScratchDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("blit-axis-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Self(dir)
        }
        fn with(self, file: &str) -> Self {
            std::fs::write(self.0.join(file), b"").expect("write marker");
            self
        }
    }
    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Recognising the X11 bridge means walking up from the pid on the other
    /// end of a connection, because Xwayland connects on its own behalf and
    /// is a child of the process blit actually started.
    #[test]
    fn parent_pid_finds_the_process_that_spawned_a_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        assert_eq!(parent_pid(child.id()), Some(std::process::id()));
        let _ = child.kill();
        let _ = child.wait();
        // A reaped pid has no `/proc` entry, and the walk has to end rather
        // than climb something that has been recycled.
        assert_eq!(parent_pid(u32::MAX), None);
    }

    /// Chromium and Electron both need the smooth axis in detent units, and
    /// both are recognisable only by the runtime payload beside the binary:
    /// an Electron app renames the executable to whatever it likes.
    #[test]
    fn chromium_is_recognised_by_its_runtime_payload() {
        assert!(dir_is_chromium(
            &ScratchDir::new("crashpad")
                .with("chrome_crashpad_handler")
                .0
        ));
        assert!(dir_is_chromium(
            &ScratchDir::new("snapshot")
                .with("v8_context_snapshot.bin")
                .0
        ));
        assert!(dir_is_chromium(
            &ScratchDir::new("icu").with("icudtl.dat").0
        ));
    }

    /// Everything else keeps pixels, which is what the protocol specifies.
    #[test]
    fn an_ordinary_binary_is_not_chromium() {
        assert!(!dir_is_chromium(
            &ScratchDir::new("plain").with("alacritty").0
        ));
        assert!(!dir_is_chromium(std::path::Path::new(
            "/nonexistent/blit/axis/scale"
        )));
    }

    #[test]
    fn scan_skips_zero_and_wraps() {
        assert_eq!(next_surface_id_after(1), 2);
        assert_eq!(next_surface_id_after(u16::MAX), 1, "0 is never handed out");

        // Seeded past the end of a small taken set, the scan wraps around
        // to the first free id rather than giving up.
        let taken: std::collections::HashSet<u16> = (1..=10).collect();
        assert_eq!(scan_free_surface_id(5, |id| taken.contains(&id)), Some(11));
        assert_eq!(
            scan_free_surface_id(u16::MAX, |id| taken.contains(&id)),
            Some(u16::MAX)
        );
    }

    #[test]
    fn frame_clock_skips_missed_ticks_without_drifting() {
        let base = std::time::Instant::now();
        let interval = std::time::Duration::from_millis(8);
        let mut deadline = base + interval;
        let late = base + std::time::Duration::from_millis(21);

        let presentation = consume_frame_clock_deadline(&mut deadline, late, interval);

        assert_eq!(presentation, base + interval * 2);
        assert_eq!(deadline, base + interval * 3);
    }

    #[test]
    fn unchanged_frame_clock_update_preserves_phase() {
        let interval = std::time::Duration::from_millis(8);
        let next = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut clocks = FxHashMap::from_iter([(1, FrameClockEntry { interval, next })]);

        update_frame_clock(&mut clocks, 1, Some(interval));

        assert_eq!(clocks[&1].next, next);
    }

    /// The whole point of the Option: a full space must refuse, not alias.
    /// The old scan broke out of the loop on exhaustion exactly as it did on
    /// success and returned the occupied id it started from, so two live
    /// toplevels shared one and destroying either unregistered both.
    #[test]
    fn scan_refuses_when_every_id_is_taken() {
        assert_eq!(scan_free_surface_id(1, |_| true), None);
        assert_eq!(scan_free_surface_id(40_000, |_| true), None);
        assert_eq!(scan_free_surface_id(u16::MAX, |_| true), None);
    }

    /// A 0 seed cannot occur today (the counter is initialised to 1 and kept
    /// non-zero) but the old loop would have hung on it forever.
    #[test]
    fn scan_tolerates_a_zero_seed() {
        assert_eq!(scan_free_surface_id(0, |_| false), Some(1));
        assert_eq!(scan_free_surface_id(0, |_| true), None);
    }

    #[test]
    fn sidebar_target_is_not_promoted_to_native_size() {
        // An unsized 1342x1118 app can publish only a 512x426 sidebar frame.
        // Native metadata comes from the submission, never that frame list.
        let submitted_native = Some((7, 1342, 1118));
        let sidebar_frame = (7, 512, 426);

        let native = native_size_after_render(7, None, submitted_native, 120);

        assert_eq!(native, Some((7, 1342, 1118, 1342, 1118)));
        assert_ne!(
            native.map(|(_, w, h, _, _)| (w, h)),
            Some((sidebar_frame.1, sidebar_frame.2))
        );
    }

    #[test]
    fn absent_shm_damage_initializes_the_full_texture() {
        assert_eq!(
            shm_damage_rects(&[], 100, 80, 1, None, None),
            vec![ShmDamageRect {
                x: 0,
                y: 0,
                width: 100,
                height: 80,
            }]
        );
    }

    #[test]
    fn buffer_damage_is_clipped_without_changing_units() {
        let pending = [PendingDamage::Buffer {
            x: -10,
            y: 70,
            width: 40,
            height: 30,
        }];
        assert_eq!(
            shm_damage_rects(&pending, 100, 80, 2, None, None),
            vec![ShmDamageRect {
                x: 0,
                y: 70,
                width: 30,
                height: 10,
            }]
        );
    }

    #[test]
    fn surface_damage_uses_the_committed_buffer_scale() {
        let pending = [PendingDamage::Surface {
            x: 3,
            y: 4,
            width: 5,
            height: 6,
        }];
        assert_eq!(
            shm_damage_rects(&pending, 100, 80, 2, None, None),
            vec![ShmDamageRect {
                x: 6,
                y: 8,
                width: 10,
                height: 12,
            }]
        );
    }

    #[test]
    fn surface_damage_maps_through_viewport_crop_and_scale() {
        let pending = [PendingDamage::Surface {
            x: 3,
            y: 4,
            width: 5,
            height: 6,
        }];
        assert_eq!(
            shm_damage_rects(
                &pending,
                100,
                80,
                2,
                Some((10.5, 5.25, 20.0, 30.0)),
                Some((40, 60)),
            ),
            vec![ShmDamageRect {
                x: 24,
                y: 14,
                width: 5,
                height: 7,
            }]
        );
    }

    #[test]
    fn surface_damage_maps_destination_against_the_full_buffer() {
        let pending = [PendingDamage::Surface {
            x: 10,
            y: 5,
            width: 20,
            height: 10,
        }];
        assert_eq!(
            shm_damage_rects(&pending, 200, 100, 2, None, Some((50, 25))),
            vec![ShmDamageRect {
                x: 40,
                y: 20,
                width: 80,
                height: 40,
            }]
        );
    }
}
