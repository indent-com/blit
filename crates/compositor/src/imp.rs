//! Headless Wayland compositor using `wayland-server` directly.
//!
//! Handles
//! wl_compositor, wl_subcompositor, xdg_shell, wl_shm, wl_seat,
//! wl_output, and zwp_linux_dmabuf_v1.  Pixel data is read on every
//! commit and sent to the server via `CompositorEvent::SurfaceCommit`.

use crate::positioner::PositionerGeometry;
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, LoopSignal, PostAction};
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
use wayland_protocols::xdg::shell::server::xdg_popup::{self, XdgPopup};
use wayland_protocols::xdg::shell::server::xdg_positioner::XdgPositioner;
use wayland_protocols::xdg::shell::server::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::server::xdg_toplevel::{self, XdgToplevel};
use wayland_protocols::xdg::shell::server::xdg_wm_base::{self, XdgWmBase};
use wayland_server::protocol::wl_buffer::WlBuffer;
use wayland_server::protocol::wl_callback::WlCallback;
use wayland_server::protocol::wl_compositor::WlCompositor;
use wayland_server::protocol::wl_data_device::{self, WlDataDevice};
use wayland_server::protocol::wl_data_device_manager::{self, WlDataDeviceManager};
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
use wayland_server::backend::ObjectId;
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
    /// VA-API surface ready for VPP/encode — zero-copy path.
    VaSurface {
        surface_id: u32,
        va_display: usize,
        _fd: Arc<OwnedFd>,
    },
    /// Pre-encoded bitstream from Vulkan Video encoder.
    /// The compositor did render → NV12 compute → video encode in one shot.
    Encoded {
        data: Arc<Vec<u8>>,
        is_keyframe: bool,
        /// Codec flag matching SURFACE_FRAME_CODEC_* constants.
        codec_flag: u8,
    },
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
            PixelData::VaSurface { .. } | PixelData::Encoded { .. } => Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            PixelData::Bgra(v) | PixelData::Rgba(v) => v.is_empty(),
            PixelData::Encoded { data, .. } => data.is_empty(),
            PixelData::Nv12 { data, .. } => data.is_empty(),
            PixelData::DmaBuf { .. }
            | PixelData::VaSurface { .. }
            | PixelData::Nv12DmaBuf { .. } => false,
        }
    }

    pub fn is_dmabuf(&self) -> bool {
        matches!(self, PixelData::DmaBuf { .. })
    }

    pub fn is_va_surface(&self) -> bool {
        matches!(self, PixelData::VaSurface { .. })
    }
}

#[derive(Clone)]
pub enum CursorImage {
    Named(String),
    Custom {
        hotspot_x: u16,
        hotspot_y: u16,
        width: u16,
        height: u16,
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
    SurfaceCommit {
        surface_id: u16,
        width: u32,
        height: u32,
        pixels: PixelData,
        /// CLOCK_MONOTONIC milliseconds at commit time so the server can
        /// stamp surface frames with the source's presentation timing
        /// rather than the (jittery) encode-delivery wall clock.
        timestamp_ms: u32,
    },
    SurfaceTitle {
        surface_id: u16,
        title: String,
    },
    SurfaceAppId {
        surface_id: u16,
        app_id: String,
    },
    SurfaceResized {
        surface_id: u16,
        width: u16,
        height: u16,
    },
    ClipboardContent {
        mime_type: String,
        data: Vec<u8>,
    },
    SurfaceCursor {
        surface_id: u16,
        cursor: CursorImage,
    },
}

pub enum CompositorCommand {
    KeyInput {
        surface_id: u16,
        keycode: u32,
        pressed: bool,
    },
    PointerMotion {
        surface_id: u16,
        x: f64,
        y: f64,
    },
    PointerButton {
        surface_id: u16,
        button: u32,
        pressed: bool,
    },
    PointerAxis {
        surface_id: u16,
        axis: u8,
        value: f64,
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
    Capture {
        surface_id: u16,
        scale_120: u16,
        reply: mpsc::SyncSender<Option<(u32, u32, Vec<u8>)>>,
    },
    RequestFrame {
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
    /// Set externally-allocated DMA-BUF fds as GPU renderer output targets
    /// for a surface.  The compositor renders into these buffers so the
    /// encoder can zero-copy them as input.
    SetExternalOutputBuffers {
        surface_id: u32,
        buffers: Vec<ExternalOutputBuffer>,
    },
    /// Synthesize text input as key press/release sequences.
    TextInput {
        text: String,
    },
    /// Update the advertised output refresh rate (millihertz).
    SetRefreshRate {
        mhz: u32,
    },
    /// Set up a Vulkan Video encoder for a surface.
    SetVulkanEncoder {
        surface_id: u32,
        codec: u8,
        qp: u8,
        width: u32,
        height: u32,
    },
    /// Request a keyframe from the Vulkan Video encoder for a surface.
    RequestVulkanKeyframe {
        surface_id: u32,
    },
    /// Destroy the Vulkan Video encoder for a surface.
    DestroyVulkanEncoder {
        surface_id: u32,
    },
    Shutdown,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// Per-wl_surface state.  `pub(crate)` so render.rs can access fields.
pub(crate) struct Surface {
    pub surface_id: u16,
    pub wl_surface: WlSurface,

    // pending state
    pending_buffer: Option<WlBuffer>,
    pending_buffer_scale: i32,
    pending_damage: bool,
    pending_frame_callbacks: Vec<WlCallback>,
    pending_presentation_feedbacks: Vec<WpPresentationFeedback>,
    pending_opaque: bool,

    // committed state
    pub buffer_scale: i32,
    pub is_opaque: bool,

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

    title: String,
    app_id: String,

    // viewport
    pending_viewport_destination: Option<(i32, i32)>,
    /// Committed viewport destination (logical size declared by client via
    /// `wp_viewport.set_destination`).  Used by fractional-scale-aware clients
    /// (e.g. Chromium) that render at physical resolution with `buffer_scale=1`
    /// and rely on the viewport to declare the logical surface size.
    pub viewport_destination: Option<(i32, i32)>,

    is_cursor: bool,
    cursor_hotspot: (i32, i32),
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
        let w = width as u32;
        let h = height as u32;
        let s = stride as usize;
        let off = offset as usize;
        let row_bytes = w as usize * 4;
        let needed = off + s * (h as usize).saturating_sub(1) + row_bytes;
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

struct ClientState;
struct XdgSurfaceData {
    wl_surface_id: ObjectId,
}
struct XdgToplevelData {
    wl_surface_id: ObjectId,
}
struct XdgPopupData {
    wl_surface_id: ObjectId,
}
struct SubsurfaceData {
    wl_surface_id: ObjectId,
    parent_surface_id: ObjectId,
}

// -- Clipboard / data device data types --

struct DataSourceData {
    mime_types: std::sync::Mutex<Vec<String>>,
}

struct DataOfferData {
    /// If `true`, the offer represents external (browser/CLI) clipboard data
    /// stored in `Compositor::external_clipboard`.  Otherwise it is backed by
    /// a Wayland `wl_data_source`.
    external: bool,
}

/// Stored state for the external (browser/CLI) clipboard selection.
struct ExternalClipboard {
    mime_type: String,
    data: Vec<u8>,
}

struct PrimarySourceData {
    mime_types: std::sync::Mutex<Vec<String>>,
}
struct PrimaryOfferData {
    external: bool,
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
    /// Whether the client has sent `enable` (text input is active).
    enabled: bool,
}

/// Main compositor state.
struct Compositor {
    display_handle: DisplayHandle,
    surfaces: HashMap<ObjectId, Surface>,
    toplevel_surface_ids: HashMap<u16, ObjectId>,
    next_surface_id: u16,
    shm_pools: HashMap<ObjectId, Arc<ShmPool>>,
    /// Per-surface metadata (dimensions, scale, flags) populated at commit time.
    /// Replaces the old pixel_cache — pixel data now lives as persistent GPU
    /// textures inside VulkanRenderer.
    surface_meta: HashMap<ObjectId, super::render::SurfaceMeta>,
    dmabuf_params: HashMap<ObjectId, DmaBufParamsPending>,
    vulkan_renderer: Option<super::vulkan_render::VulkanRenderer>,
    output_width: i32,
    output_height: i32,
    /// Advertised refresh rate in millihertz.  Derived from the highest
    /// `display_fps` among connected browser clients.
    output_refresh_mhz: u32,
    /// Output scale in 1/120th units (wp_fractional_scale_v1 convention).
    /// 120 = 1×, 180 = 1.5×, 240 = 2×.  Derived from the browser's
    /// devicePixelRatio sent via C2S_SURFACE_RESIZE.
    output_scale_120: u16,
    outputs: Vec<WlOutput>,
    keyboards: Vec<WlKeyboard>,
    pointers: Vec<WlPointer>,
    keyboard_keymap_data: Vec<u8>,
    /// Currently depressed (held down) XKB modifier mask.
    mods_depressed: u32,
    /// CapsLock locked modifier mask (toggled on/off by CapsLock key).
    mods_locked: u32,
    serial: u32,
    event_tx: mpsc::Sender<CompositorEvent>,
    event_notify: Arc<dyn Fn() + Send + Sync>,
    loop_signal: LoopSignal,
    /// Pending per-surface commit data: `(phys_w, phys_h, log_w, log_h, pixels)`.
    pending_commits: HashMap<u16, (u32, u32, u32, u32, PixelData)>,
    focused_surface_id: u16,
    /// The wl_surface ObjectId the pointer is currently over (None = none).
    pointer_entered_id: Option<ObjectId>,
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
    last_reported_size: HashMap<u16, (u32, u32, u32, u32)>,
    /// Per-toplevel configured size.  Each surface can live in a
    /// differently-sized BSP pane, so we need to track sizes individually
    /// rather than relying on the single `output_width`/`output_height`.
    surface_sizes: HashMap<u16, (i32, i32)>,
    /// Pending positioner geometry, keyed by XdgPositioner protocol id.
    positioners: HashMap<ObjectId, PositionerState>,
    /// Active wp_fractional_scale_v1 objects.  When `output_scale_120`
    /// changes we send `preferred_scale` to every entry.
    fractional_scales: Vec<WpFractionalScaleV1>,

    // -- Clipboard --
    /// Active wl_data_device objects (one per seat binding).
    data_devices: Vec<WlDataDevice>,
    /// The wl_data_source that currently owns the clipboard selection (if any).
    /// Cleared when the source is destroyed or replaced.
    selection_source: Option<WlDataSource>,
    /// External clipboard data offered from the browser or CLI.
    external_clipboard: Option<ExternalClipboard>,

    // -- Primary selection --
    primary_devices: Vec<ZwpPrimarySelectionDeviceV1>,
    primary_source: Option<ZwpPrimarySelectionSourceV1>,
    external_primary: Option<ExternalClipboard>,

    // -- Relative pointer --
    relative_pointers: Vec<ZwpRelativePointerV1>,

    // -- Text input --
    /// Active zwp_text_input_v3 objects.  When the compositor receives
    /// composed text from the browser it delivers it via `commit_string`
    /// + `done` to the text_input object belonging to the focused surface.
    text_inputs: Vec<TextInputState>,
    /// Serial counter for `zwp_text_input_v3.done` events.  Incremented on
    /// every `done` event sent by the compositor.
    #[expect(dead_code)]
    text_input_serial: u32,

    // -- Activation --
    next_activation_token: u32,

    // -- Popup grab --
    /// Stack of grabbed xdg_popup surfaces (outermost first).  When the
    /// pointer clicks outside the topmost grabbed popup we send
    /// `xdg_popup.popup_done` to dismiss the popup chain.
    popup_grab_stack: Vec<ObjectId>,

    // -- DMA-BUF buffer hold --
    /// Buffers whose DMA-BUF content could not be eagerly snapshotted to
    /// CPU memory (e.g. tiled VRAM that cannot be mmap-read linearly, or
    /// fence not ready).  We hold the `WlBuffer` alive so the client
    /// cannot reuse it while the GPU texture still references the fd.
    /// Released when the surface commits a new buffer or is destroyed.
    held_buffers: HashMap<ObjectId, WlBuffer>,

    // -- Cursor pixel cache --
    /// CPU-accessible RGBA pixels for cursor surfaces.  Cursors aren't
    /// GPU-composited — they're sent as cursor image events.  Updated
    /// at cursor surface commit time.
    cursor_rgba: HashMap<ObjectId, (u32, u32, Vec<u8>)>,
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

    /// Switch keyboard (and text_input) focus from the current surface to
    /// `new_surface_id`.  Sends `wl_keyboard.leave` to the old surface's
    /// client and `wl_keyboard.enter` to the new surface's client, which is
    /// required by the Wayland protocol when focus changes between clients.
    fn set_keyboard_focus(&mut self, new_surface_id: u16) {
        let old_id = self.focused_surface_id;
        if old_id == new_surface_id {
            // Focus unchanged — still send enter so the client gets the
            // event (e.g. first toplevel), but skip leave.
            self.focused_surface_id = new_surface_id;
            if let Some(root_id) = self.toplevel_surface_ids.get(&new_surface_id)
                && let Some(wl_surface) = self.surfaces.get(root_id).map(|s| s.wl_surface.clone())
            {
                let serial = self.next_serial();
                for kb in &self.keyboards {
                    if same_client(kb, &wl_surface) {
                        kb.enter(serial, &wl_surface, vec![]);
                    }
                }
                for ti in &self.text_inputs {
                    if same_client(&ti.resource, &wl_surface) {
                        ti.resource.enter(&wl_surface);
                    }
                }
            }
            return;
        }

        // Leave the old surface.
        if old_id != 0
            && let Some(old_root) = self.toplevel_surface_ids.get(&old_id)
            && let Some(old_wl) = self.surfaces.get(old_root).map(|s| s.wl_surface.clone())
        {
            let serial = self.next_serial();
            for kb in &self.keyboards {
                if same_client(kb, &old_wl) {
                    kb.leave(serial, &old_wl);
                }
            }
            for ti in &self.text_inputs {
                if same_client(&ti.resource, &old_wl) {
                    ti.resource.leave(&old_wl);
                }
            }
        }

        self.focused_surface_id = new_surface_id;

        // Enter the new surface.
        if let Some(root_id) = self.toplevel_surface_ids.get(&new_surface_id)
            && let Some(wl_surface) = self.surfaces.get(root_id).map(|s| s.wl_surface.clone())
        {
            let serial = self.next_serial();
            for kb in &self.keyboards {
                if same_client(kb, &wl_surface) {
                    kb.enter(serial, &wl_surface, vec![]);
                }
            }
            for ti in &self.text_inputs {
                if same_client(&ti.resource, &wl_surface) {
                    ti.resource.enter(&wl_surface);
                }
            }
        }
    }

    fn allocate_surface_id(&mut self) -> u16 {
        let mut id = self.next_surface_id;
        let start = id;
        loop {
            if !self.toplevel_surface_ids.contains_key(&id) {
                break;
            }
            id = id.wrapping_add(1);
            if id == 0 {
                id = 1;
            }
            if id == start {
                break;
            }
        }
        self.next_surface_id = id.wrapping_add(1);
        if self.next_surface_id == 0 {
            self.next_surface_id = 1;
        }
        id
    }

    fn flush_pending_commits(&mut self) {
        for (surface_id, (width, height, log_w, log_h, pixels)) in self.pending_commits.drain() {
            let prev = self.last_reported_size.get(&surface_id).copied();
            if prev.is_none() || prev.map(|(pw, ph, _, _)| (pw, ph)) != Some((width, height)) {
                self.last_reported_size
                    .insert(surface_id, (width, height, log_w, log_h));
                let _ = self.event_tx.send(CompositorEvent::SurfaceResized {
                    surface_id,
                    width: width as u16,
                    height: height as u16,
                });
            }
            let _ = self.event_tx.send(CompositorEvent::SurfaceCommit {
                surface_id,
                width,
                height,
                pixels,
                timestamp_ms: elapsed_ms(),
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
            .is_some_and(|s| s.pending_buffer.is_some());
        {
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
                // no compositing will run to consume it.
                if let Some(held) = self.held_buffers.remove(surface_id) {
                    held.release();
                }
                // Fire any pending frame callbacks so the client doesn't
                // stall.
                self.fire_surface_frame_callbacks(surface_id);
                let _ = self.display_handle.flush_clients();
                return;
            }
        };

        // Composite at the output scale so HiDPI clients are rendered
        // at full resolution.  Use the browser's requested size as the
        // target so the frame fits the canvas without letterboxing.
        let s120 = self.output_scale_120;
        let target_phys = self.surface_sizes.get(&toplevel_sid).map(|&(lw, lh)| {
            let pw = super::render::to_physical(lw as u32, s120 as u32);
            let ph = super::render::to_physical(lh as u32, s120 as u32);
            (pw, ph)
        });
        let composited = if let Some(ref mut vk) = self.vulkan_renderer {
            vk.render_tree_sized(
                &root_id,
                &self.surfaces,
                &self.surface_meta,
                s120,
                target_phys,
                toplevel_sid,
            )
        } else {
            None
        };

        if let Some((result_sid, w, h, ref pixels)) = composited
            && !pixels.is_empty()
        {
            let kind = match pixels {
                PixelData::Bgra(_) => "bgra",
                PixelData::Rgba(_) => "rgba",
                PixelData::Nv12 { .. } => "nv12",
                PixelData::VaSurface { .. } => "va-surface",
                PixelData::Nv12DmaBuf { .. } => "nv12-dmabuf",
                PixelData::Encoded { .. } => "vulkan-encoded",
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
            // Determine the logical size for pointer coordinate mapping.
            // The composited frame's physical dimensions (w, h) must pair
            // with a logical size that preserves the true DPR ratio so the
            // PointerMotion handler can convert browser pixel coords back
            // to Wayland logical coords correctly.  The simplest correct
            // approach is to derive logical size directly from the physical
            // size and the output scale — this works regardless of whether
            // the frame came from the Vulkan renderer (which targets the
            // browser's requested size) or the CPU renderer (which targets
            // the xdg_geometry content area).  The PointerMotion handler
            // separately adds the xdg_geometry offset to translate from
            // composited-frame space into surface-tree space, so the
            // logical size here should represent the full composited frame,
            // not just the xdg_geometry window extents.
            let s120_u32 = (s120 as u32).max(120);
            let log_w = (w * 120).div_ceil(s120_u32);
            let log_h = (h * 120).div_ceil(s120_u32);
            self.pending_commits
                .insert(result_sid, (w, h, log_w, log_h, composited.unwrap().3));
        }

        // Compositing is done — the VulkanRenderer holds its own dup'd
        // fd reference to the DMA-BUF via the persistent texture cache.
        // Release the held buffer so the client can reuse it for the
        // next frame.
        if let Some(held) = self.held_buffers.remove(surface_id) {
            held.release();
        }

        // Always fire frame callbacks after processing a commit, so
        // clients can continue their render loop.  Without this, clients
        // stall when the server doesn't send RequestFrame (e.g. during
        // resize or when no subscribers are connected).
        self.fire_frame_callbacks_for_toplevel(toplevel_sid);

        // After an output scale change, re-send keyboard leave/enter on
        // the first commit so clients (especially Firefox) resume input
        // processing.  Deferred to here so the client has processed the
        // reconfigure before we re-enter.
        if self.pending_kb_reenter {
            self.pending_kb_reenter = false;
            let root_ids: Vec<ObjectId> = self.toplevel_surface_ids.values().cloned().collect();
            for root_id in root_ids {
                let wl = self.surfaces.get(&root_id).map(|s| s.wl_surface.clone());
                if let Some(wl) = wl {
                    let serial = self.next_serial();
                    for kb in &self.keyboards {
                        if same_client(kb, &wl) {
                            kb.leave(serial, &wl);
                        }
                    }
                    let serial = self.next_serial();
                    for kb in &self.keyboards {
                        if same_client(kb, &wl) {
                            kb.enter(serial, &wl, vec![]);
                        }
                    }
                }
            }
            let _ = self.display_handle.flush_clients();
        }

        if self.verbose {
            let cache_entries = self.surface_meta.len();
            let has_pending = self.pending_commits.contains_key(&toplevel_sid);
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
    /// surface whose pixel bounds contain (`x`, `y`).  Returns
    /// `(wl_surface, local_x, local_y)` with coordinates relative to the
    /// hit surface.  Falls back to the root surface when nothing else matches.
    fn hit_test_surface_at(
        &self,
        root_id: &ObjectId,
        x: f64,
        y: f64,
    ) -> Option<(WlSurface, f64, f64)> {
        self.hit_test_recursive(root_id, x, y, 0, 0).or_else(|| {
            // Fallback: return the root surface with the original coords.
            self.surfaces
                .get(root_id)
                .map(|s| (s.wl_surface.clone(), x, y))
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
        let sx = offset_x + surf.subsurface_position.0;
        let sy = offset_y + surf.subsurface_position.1;

        // Children are ordered back-to-front; iterate in reverse for topmost.
        for child_id in surf.children.iter().rev() {
            if let Some(hit) = self.hit_test_recursive(child_id, x, y, sx, sy) {
                return Some(hit);
            }
        }

        // Check this surface's bounds (logical coordinates).
        if let Some(sm) = self.surface_meta.get(surface_id) {
            let s = sm.scale.max(1) as f64;
            let (w, h) = (sm.width, sm.height);
            // Prefer viewport destination for logical size (fractional-scale
            // clients set buffer_scale=1 and declare logical size via viewport).
            let (lw, lh) = surf
                .viewport_destination
                .filter(|&(dw, dh)| dw > 0 && dh > 0)
                .map(|(dw, dh)| (dw as f64, dh as f64))
                .unwrap_or((w as f64 / s, h as f64 / s));
            let lx = x - sx as f64;
            let ly = y - sy as f64;
            if lx >= 0.0 && ly >= 0.0 && lx < lw && ly < lh {
                return Some((surf.wl_surface.clone(), lx, ly));
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
    fn apply_pending_state(&mut self, surface_id: &ObjectId) {
        let (buffer, scale, is_cursor) = {
            let Some(surf) = self.surfaces.get_mut(surface_id) else {
                return;
            };
            let buffer = surf.pending_buffer.take();
            let scale = surf.pending_buffer_scale;
            surf.buffer_scale = scale;
            surf.viewport_destination = surf.pending_viewport_destination;
            surf.is_opaque = surf.pending_opaque;
            surf.pending_damage = false;
            if let Some(pos) = surf.pending_subsurface_position.take() {
                surf.subsurface_position = pos;
            }
            (buffer, scale, surf.is_cursor)
        };
        let Some(buf) = buffer else { return };

        // Release any previously held buffer for this surface — the new
        // commit supersedes it.
        if let Some(old) = self.held_buffers.remove(surface_id) {
            old.release();
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
            if w > 0
                && h > 0
                && let Some(ref mut vk) = self.vulkan_renderer
            {
                let swap_rb =
                    !matches!(format, wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888);
                let force_opaque =
                    matches!(format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Xbgr8888);
                let row_bytes = w as usize * 4;
                let uploaded = shm
                    .pool
                    .with_mmap(|slice| {
                        if offset + stride * (h as usize - 1) + row_bytes > slice.len() {
                            return false;
                        }
                        vk.upload_surface_shm_mmap(
                            surface_id,
                            slice,
                            offset,
                            stride,
                            w,
                            h,
                            swap_rb,
                            force_opaque,
                        )
                    })
                    .unwrap_or(false);
                if uploaded {
                    self.surface_meta.insert(
                        surface_id.clone(),
                        super::render::SurfaceMeta {
                            width: w,
                            height: h,
                            scale,
                            y_invert: false,
                        },
                    );
                    buf.release();
                    return;
                }
            }
        }

        if let Some((w, h, pixels)) = self.read_buffer(&buf) {
            let y_invert = matches!(pixels, PixelData::DmaBuf { y_invert: true, .. });

            // Upload the surface's pixel data as a persistent GPU texture.
            if let Some(ref mut vk) = self.vulkan_renderer {
                vk.upload_surface(surface_id, &pixels, w, h);
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
                self.held_buffers.insert(surface_id.clone(), buf);
            } else {
                // SHM buffers are snapshotted into the GPU texture.
                // Release immediately so the client can reuse the buffer.
                buf.release();
            }
        } else {
            buf.release();
        }
    }

    fn fire_surface_frame_callbacks(&mut self, surface_id: &ObjectId) {
        let (callbacks, feedbacks) = {
            let Some(surf) = self.surfaces.get_mut(surface_id) else {
                return;
            };
            (
                std::mem::take(&mut surf.pending_frame_callbacks),
                std::mem::take(&mut surf.pending_presentation_feedbacks),
            )
        };
        let time = elapsed_ms();
        for cb in callbacks {
            cb.done(time);
        }
        if !feedbacks.is_empty() {
            let (sec, nsec) = monotonic_timespec();
            // Send sync_output for each feedback, then presented().
            // refresh=0 means unknown (headless, no real display).
            for fb in feedbacks {
                for output in &self.outputs {
                    if same_client(&fb, output) {
                        fb.sync_output(output);
                    }
                }
                // refresh in nanoseconds (millihertz → ns: 1e12 / mhz)
                let refresh_ns = if self.output_refresh_mhz > 0 {
                    (1_000_000_000_000u64 / self.output_refresh_mhz as u64) as u32
                } else {
                    0
                };
                fb.presented(
                    (sec >> 32) as u32,
                    sec as u32,
                    nsec as u32,
                    refresh_ns,
                    0, // seq_hi
                    0, // seq_lo
                    WpPresentationFeedbackKind::empty(),
                );
            }
        }
    }

    /// Remove surfaces whose underlying `WlSurface` is no longer alive.
    /// This handles the case where a Wayland client process exits or crashes
    /// without explicitly destroying its surfaces — `dispatch_clients()`
    /// marks the resources as dead, and we clean up here.
    fn cleanup_dead_surfaces(&mut self) {
        // Purge stale protocol objects from disconnected clients.
        self.fractional_scales.retain(|fs| fs.is_alive());
        self.outputs.retain(|o| o.is_alive());
        self.keyboards.retain(|k| k.is_alive());
        self.pointers.retain(|p| p.is_alive());
        self.data_devices.retain(|d| d.is_alive());
        self.primary_devices.retain(|d| d.is_alive());
        self.relative_pointers.retain(|p| p.is_alive());
        self.text_inputs.retain(|ti| ti.resource.is_alive());
        self.shm_pools.retain(|_, p| p.resource.is_alive());
        self.dmabuf_params.retain(|_, p| p.resource.is_alive());
        self.positioners.retain(|_, p| p.resource.is_alive());

        let dead: Vec<ObjectId> = self
            .surfaces
            .iter()
            .filter(|(_, surf)| !surf.wl_surface.is_alive())
            .map(|(id, _)| id.clone())
            .collect();

        for proto_id in &dead {
            self.surface_meta.remove(proto_id);
            if let Some(ref mut vk) = self.vulkan_renderer {
                vk.remove_surface(proto_id);
            }
            if let Some(held) = self.held_buffers.remove(proto_id) {
                held.release();
            }
            if let Some(surf) = self.surfaces.remove(proto_id) {
                // Discard any pending presentation feedbacks — the surface
                // died before the frame was ever presented.
                for fb in surf.pending_presentation_feedbacks {
                    fb.discarded();
                }
                if let Some(ref parent_id) = surf.parent_surface_id
                    && let Some(parent) = self.surfaces.get_mut(parent_id)
                {
                    parent.children.retain(|c| c != proto_id);
                }
                if surf.surface_id > 0 {
                    self.toplevel_surface_ids.remove(&surf.surface_id);
                    self.last_reported_size.remove(&surf.surface_id);
                    self.surface_sizes.remove(&surf.surface_id);
                    let _ = self.event_tx.send(CompositorEvent::SurfaceDestroyed {
                        surface_id: surf.surface_id,
                    });
                    (self.event_notify)();
                }
            }
        }
    }

    fn fire_frame_callbacks_for_toplevel(&mut self, toplevel_sid: u16) {
        let Some(root_id) = self.toplevel_surface_ids.get(&toplevel_sid).cloned() else {
            return;
        };
        let tree = self.collect_surface_tree(&root_id);
        for sid in &tree {
            self.fire_surface_frame_callbacks(sid);
        }
        let _ = self.display_handle.flush_clients();
    }

    fn handle_cursor_commit(&mut self, surface_id: &ObjectId) {
        self.apply_pending_state(surface_id);
        let hotspot = self
            .surfaces
            .get(surface_id)
            .map(|s| s.cursor_hotspot)
            .unwrap_or((0, 0));
        if let Some((w, h, rgba)) = self.cursor_rgba.get(surface_id)
            && !rgba.is_empty()
        {
            let _ = self.event_tx.send(CompositorEvent::SurfaceCursor {
                surface_id: self.focused_surface_id,
                cursor: CursorImage::Custom {
                    hotspot_x: hotspot.0 as u16,
                    hotspot_y: hotspot.1 as u16,
                    width: *w as u16,
                    height: *h as u16,
                    rgba: rgba.clone(),
                },
            });
        }
        self.fire_surface_frame_callbacks(surface_id);
        let _ = self.display_handle.flush_clients();
    }

    fn handle_command(&mut self, cmd: CompositorCommand) {
        match cmd {
            CompositorCommand::KeyInput {
                surface_id: _,
                keycode,
                pressed,
            } => {
                let serial = self.next_serial();
                let time = elapsed_ms();
                let state = if pressed {
                    wl_keyboard::KeyState::Pressed
                } else {
                    wl_keyboard::KeyState::Released
                };
                let focused_wl = self
                    .toplevel_surface_ids
                    .get(&self.focused_surface_id)
                    .and_then(|root_id| self.surfaces.get(root_id))
                    .map(|s| s.wl_surface.clone());
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
            CompositorCommand::TextInput { text } => {
                let focused_wl = self
                    .toplevel_surface_ids
                    .get(&self.focused_surface_id)
                    .and_then(|root_id| self.surfaces.get(root_id))
                    .map(|s| s.wl_surface.clone());
                let Some(focused_wl) = focused_wl else { return };

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
                let saved_mods_depressed = self.mods_depressed;
                for ch in text.chars() {
                    if let Some((kc, need_shift)) = char_to_keycode(ch) {
                        let time = elapsed_ms();
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
                    }
                    // Non-ASCII characters without a text_input_v3 path
                    // are silently dropped.
                }
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
            CompositorCommand::PointerMotion { surface_id, x, y } => {
                let time = elapsed_ms();
                // The browser sends coordinates in the composited frame's
                // physical pixel space.  Convert to logical (surface-local)
                // coordinates using the actual composited-to-logical ratio
                // for this surface.
                let (mut x, mut y) =
                    if let Some(&(cw, ch, lw, lh)) = self.last_reported_size.get(&surface_id) {
                        let sx = if cw > 0 { lw as f64 / cw as f64 } else { 1.0 };
                        let sy = if ch > 0 { lh as f64 / ch as f64 } else { 1.0 };
                        (x * sx, y * sy)
                    } else {
                        (x, y)
                    };
                // The composited frame is cropped to xdg_geometry (if set),
                // so the browser's (0,0) corresponds to (geo_x, geo_y) in the
                // surface tree.  Offset accordingly.
                if let Some((gx, gy, _, _)) = self
                    .toplevel_surface_ids
                    .get(&surface_id)
                    .and_then(|rid| self.surfaces.get(rid))
                    .and_then(|s| s.xdg_geometry)
                {
                    x += gx as f64;
                    y += gy as f64;
                }
                // Hit-test the surface tree to find the actual target
                // (may be a subsurface or popup rather than the root).
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
                        target_wl.as_ref().map(|(pid, _, lx, ly)| format!(
                            "proto={pid:?} local=({lx:.1},{ly:.1})"
                        ))
                    );
                }
                if let Some((proto_id, wl_surface, lx, ly)) = target_wl {
                    if self.pointer_entered_id.as_ref() != Some(&proto_id) {
                        let serial = self.next_serial();
                        let matching_ptrs = self
                            .pointers
                            .iter()
                            .filter(|p| same_client(*p, &wl_surface))
                            .count();
                        eprintln!(
                            "[pointer-enter] proto={proto_id:?} matching_ptrs={matching_ptrs} total_ptrs={}",
                            self.pointers.len()
                        );
                        // Leave old surface.
                        if self.pointer_entered_id.is_some() {
                            let old_wl = self
                                .surfaces
                                .values()
                                .find(|s| Some(s.wl_surface.id()) == self.pointer_entered_id)
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
                        self.pointer_entered_id = Some(proto_id);
                    }
                    for ptr in &self.pointers {
                        if same_client(ptr, &wl_surface) {
                            ptr.motion(time, lx, ly);
                            ptr.frame();
                        }
                    }
                }
                // When no surface is hit, don't send motion events —
                // there is no valid surface-local coordinate to report.
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::PointerButton {
                surface_id: _,
                button,
                pressed,
            } => {
                let serial = self.next_serial();
                let time = elapsed_ms();
                let state = if pressed {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };

                // If a popup is grabbed and the pointer clicked outside
                // the popup chain, dismiss the topmost grabbed popup.
                if pressed && !self.popup_grab_stack.is_empty() {
                    let click_on_grabbed = self.pointer_entered_id.as_ref().is_some_and(|eid| {
                        self.popup_grab_stack.iter().any(|gid| {
                            self.surfaces
                                .get(gid)
                                .is_some_and(|s| s.wl_surface.id() == *eid)
                        })
                    });
                    if !click_on_grabbed {
                        // Dismiss from the topmost popup down.
                        while let Some(grab_wl_id) = self.popup_grab_stack.pop() {
                            if let Some(surf) = self.surfaces.get(&grab_wl_id)
                                && let Some(ref popup) = surf.xdg_popup
                            {
                                popup.popup_done();
                            }
                        }
                        let _ = self.display_handle.flush_clients();
                    }
                }

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
                let _ = self.display_handle.flush_clients();
            }
            CompositorCommand::PointerAxis {
                surface_id: _,
                axis,
                value,
            } => {
                let time = elapsed_ms();
                let wl_axis = if axis == 0 {
                    wl_pointer::Axis::VerticalScroll
                } else {
                    wl_pointer::Axis::HorizontalScroll
                };
                let focused_wl = self
                    .surfaces
                    .values()
                    .find(|s| Some(s.wl_surface.id()) == self.pointer_entered_id)
                    .map(|s| s.wl_surface.clone());
                for ptr in &self.pointers {
                    if let Some(ref wl) = focused_wl
                        && same_client(ptr, wl)
                    {
                        ptr.axis(time, wl_axis, value);
                        ptr.frame();
                    }
                }
                let _ = self.display_handle.flush_clients();
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

                // Track whether output properties changed so we can batch
                // all events before a single output.done().
                let mut output_changed = false;

                // Update output scale (in 1/120th units) from the browser DPR.
                if scale_120 > 0 && scale_120 != self.output_scale_120 {
                    self.output_scale_120 = scale_120;
                    output_changed = true;
                }

                let s120 = self.output_scale_120 as i32;

                // Recompute output dimensions from scratch (start from 0,0)
                // so the output can shrink when surfaces get smaller or are
                // destroyed.  The previous fold started from (output_width,
                // output_height) which meant dimensions could only grow.
                let (max_w, max_h) = self
                    .surface_sizes
                    .values()
                    .fold((0i32, 0i32), |(mw, mh), &(sw, sh)| (mw.max(sw), mh.max(sh)));
                // Clamp to a sensible minimum so the output is never 0×0.
                let max_w = max_w.max(1);
                let max_h = max_h.max(1);
                if max_w != self.output_width || max_h != self.output_height {
                    self.output_width = max_w;
                    self.output_height = max_h;
                    output_changed = true;
                }

                // When any output property changed, re-send the full
                // sequence so clients see it as a display configuration
                // change: geometry → mode → scale → fractional_scale → done.
                if output_changed {
                    let int_scale = ((s120) + 119) / 120;
                    for output in &self.outputs {
                        output.geometry(
                            0,
                            0,
                            0,
                            0,
                            wl_output::Subpixel::None,
                            "blit".to_string(),
                            "virtual".to_string(),
                            wl_output::Transform::Normal,
                        );
                        // mode() takes physical pixels: logical × scale.
                        let mode_w = self.output_width * s120 / 120;
                        let mode_h = self.output_height * s120 / 120;
                        output.mode(
                            wl_output::Mode::Current | wl_output::Mode::Preferred,
                            mode_w,
                            mode_h,
                            self.output_refresh_mhz as i32,
                        );
                        if output.version() >= 2 {
                            output.scale(int_scale);
                        }
                    }
                    for fs in &self.fractional_scales {
                        fs.preferred_scale(s120 as u32);
                    }
                }

                // Single output.done() after all property changes, so the
                // client sees scale + mode atomically before the configure.
                if output_changed {
                    for output in &self.outputs {
                        if output.version() >= 2 {
                            output.done();
                        }
                    }
                }

                let states = xdg_toplevel_states(&[
                    xdg_toplevel::State::Activated,
                    xdg_toplevel::State::Maximized,
                ]);

                if output_changed {
                    // When output scale or dimensions changed, every
                    // toplevel needs a new configure so it re-renders at
                    // the correct density / size.
                    for (&sid, root_id) in &self.toplevel_surface_ids {
                        let (lw, lh) = self.surface_sizes.get(&sid).copied().unwrap_or((w, h));
                        if let Some(surf) = self.surfaces.get(root_id) {
                            if let Some(ref tl) = surf.xdg_toplevel {
                                tl.configure(lw, lh, states.clone());
                            }
                            if let Some(ref xs) = surf.xdg_surface {
                                let serial = self.serial.wrapping_add(1);
                                self.serial = serial;
                                xs.configure(serial);
                            }
                        }
                    }
                    // Fire frame callbacks so all clients repaint at new
                    // scale.
                    let all_sids: Vec<u16> = self.toplevel_surface_ids.keys().copied().collect();
                    for sid in all_sids {
                        self.fire_frame_callbacks_for_toplevel(sid);
                    }

                    // Reset pointer/keyboard state — scale change
                    // invalidates coordinate mappings.
                    self.pointer_entered_id = None;
                    self.pending_kb_reenter = true;
                } else {
                    // Only the target surface changed size — configure just
                    // that one.  This avoids disturbing other surfaces'
                    // frame callback / render cycle, which would race with
                    // the server's RequestFrame mechanism and stall them.
                    if let Some(root_id) = self.toplevel_surface_ids.get(&surface_id)
                        && let Some(surf) = self.surfaces.get(root_id)
                    {
                        if let Some(ref tl) = surf.xdg_toplevel {
                            tl.configure(w, h, states);
                        }
                        if let Some(ref xs) = surf.xdg_surface {
                            let serial = self.serial.wrapping_add(1);
                            self.serial = serial;
                            xs.configure(serial);
                        }
                    }
                    self.fire_frame_callbacks_for_toplevel(surface_id);
                }
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
                // Invalidate the Wayland-side selection — external takes over.
                self.selection_source = None;
                self.offer_external_clipboard();
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
                    self.output_scale_120
                };
                let result = if let Some(root_id) = self.toplevel_surface_ids.get(&surface_id) {
                    if let Some(ref mut vk) = self.vulkan_renderer {
                        vk.render_tree_sized(
                            root_id,
                            &self.surfaces,
                            &self.surface_meta,
                            cap_s120,
                            None,
                            surface_id,
                        )
                        .map(|(_sid, w, h, pixels)| {
                            let rgba = pixels.to_rgba(w, h);
                            (w, h, rgba)
                        })
                    } else {
                        None
                    }
                } else {
                    None
                };
                let _ = reply.send(result);
            }
            CompositorCommand::RequestFrame { surface_id } => {
                self.fire_frame_callbacks_for_toplevel(surface_id);
            }
            CompositorCommand::ReleaseKeys { keycodes } => {
                let time = elapsed_ms();
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
                buffers,
            } => {
                if let Some(ref mut vk) = self.vulkan_renderer {
                    vk.set_external_output_buffers(surface_id, buffers);
                }
            }
            CompositorCommand::SetRefreshRate { mhz } => {
                // Only update on meaningful changes (>2 Hz difference) to
                // avoid flooding clients with mode events from jittery
                // requestAnimationFrame measurements.
                let diff = (mhz as i64 - self.output_refresh_mhz as i64).unsigned_abs();
                if diff > 2000 && mhz > 0 {
                    self.output_refresh_mhz = mhz;
                    let s120 = self.output_scale_120 as i32;
                    let mode_w = self.output_width * s120 / 120;
                    let mode_h = self.output_height * s120 / 120;
                    for output in &self.outputs {
                        output.mode(
                            wl_output::Mode::Current | wl_output::Mode::Preferred,
                            mode_w,
                            mode_h,
                            mhz as i32,
                        );
                        if output.version() >= 2 {
                            output.done();
                        }
                    }
                    let _ = self.display_handle.flush_clients();
                }
            }
            CompositorCommand::SetVulkanEncoder {
                surface_id,
                codec,
                qp,
                width,
                height,
            } => {
                if let Some(ref mut vk) = self.vulkan_renderer {
                    vk.create_vulkan_encoder(surface_id, codec, qp, width, height);
                }
            }
            CompositorCommand::RequestVulkanKeyframe { surface_id } => {
                if let Some(ref mut vk) = self.vulkan_renderer {
                    vk.request_encoder_keyframe(surface_id);
                }
            }
            CompositorCommand::DestroyVulkanEncoder { surface_id } => {
                if let Some(ref mut vk) = self.vulkan_renderer {
                    vk.destroy_vulkan_encoder(surface_id);
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
            let mut mimes = vec![cb.mime_type.clone()];
            // Add standard text aliases.
            if cb.mime_type.starts_with("text/plain") {
                if cb.mime_type != "text/plain" {
                    mimes.push("text/plain".to_string());
                }
                if cb.mime_type != "text/plain;charset=utf-8" {
                    mimes.push("text/plain;charset=utf-8".to_string());
                }
                mimes.push("UTF8_STRING".to_string());
            }
            return mimes;
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
            let matches = cb.mime_type == mime_type
                || (cb.mime_type.starts_with("text/plain")
                    && (mime_type == "text/plain"
                        || mime_type == "text/plain;charset=utf-8"
                        || mime_type == "UTF8_STRING"));
            if matches {
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

/// Returns true when two Wayland resources belong to the same still-connected client.
fn same_client<R1: Resource, R2: Resource>(a: &R1, b: &R2) -> bool {
    match (a.client(), b.client()) {
        (Some(ca), Some(cb)) => ca.id() == cb.id(),
        _ => false,
    }
}

fn yuv420_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let y = (y as i32 - 16).max(0);
    let u = u as i32 - 128;
    let v = v as i32 - 128;
    let r = ((298 * y + 409 * v + 128) >> 8).clamp(0, 255) as u8;
    let g = ((298 * y - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u8;
    let b = ((298 * y + 516 * u + 128) >> 8).clamp(0, 255) as u8;
    [r, g, b]
}

/// Encode xdg_toplevel states as the raw byte array expected by the protocol.
fn xdg_toplevel_states(states: &[xdg_toplevel::State]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(states.len() * 4);
    for state in states {
        bytes.extend_from_slice(&(*state as u32).to_ne_bytes());
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
                        pending_damage: false,
                        pending_frame_callbacks: Vec::new(),
                        pending_presentation_feedbacks: Vec::new(),
                        pending_opaque: false,
                        buffer_scale: 1,
                        is_opaque: false,
                        parent_surface_id: None,
                        pending_subsurface_position: None,
                        subsurface_position: (0, 0),
                        children: Vec::new(),
                        xdg_surface: None,
                        xdg_toplevel: None,
                        xdg_popup: None,
                        xdg_geometry: None,
                        title: String::new(),
                        app_id: String::new(),
                        pending_viewport_destination: None,
                        viewport_destination: None,
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
            Request::Attach { buffer, x: _, y: _ } => {
                if let Some(surf) = state.surfaces.get_mut(&sid) {
                    surf.pending_buffer = buffer;
                }
            }
            Request::Damage { .. } | Request::DamageBuffer { .. } => {
                if let Some(surf) = state.surfaces.get_mut(&sid) {
                    surf.pending_damage = true;
                }
            }
            Request::Frame { callback } => {
                let cb = data_init.init(callback, ());
                if let Some(surf) = state.surfaces.get_mut(&sid) {
                    surf.pending_frame_callbacks.push(cb);
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
            Request::SetInputRegion { .. } => {}
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
                state.surface_meta.remove(&sid);
                state.cursor_rgba.remove(&sid);
                if let Some(ref mut vk) = state.vulkan_renderer {
                    vk.remove_surface(&sid);
                }
                if let Some(held) = state.held_buffers.remove(&sid) {
                    held.release();
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
                    if surf.surface_id > 0 {
                        state.toplevel_surface_ids.remove(&surf.surface_id);
                        state.last_reported_size.remove(&surf.surface_id);
                        state.surface_sizes.remove(&surf.surface_id);
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
        _: &mut Self,
        _: &Client,
        _: &WlRegion,
        _: <WlRegion as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
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
                let surface_id = state.allocate_surface_id();
                if let Some(surf) = state.surfaces.get_mut(&data.wl_surface_id) {
                    surf.xdg_toplevel = Some(toplevel.clone());
                    surf.surface_id = surface_id;
                }
                state
                    .toplevel_surface_ids
                    .insert(surface_id, data.wl_surface_id.clone());

                // Use a per-surface size if one was already configured
                // (e.g. the browser sent C2S_SURFACE_RESIZE before the
                // toplevel was created), otherwise fall back to the global
                // output dimensions.  surface_sizes stores logical pixels.
                let (cw, ch) = state
                    .surface_sizes
                    .get(&surface_id)
                    .copied()
                    .unwrap_or((state.output_width, state.output_height));
                let states = xdg_toplevel_states(&[
                    xdg_toplevel::State::Activated,
                    xdg_toplevel::State::Maximized,
                ]);
                toplevel.configure(cw, ch, states);
                let serial = state.next_serial();
                resource.configure(serial);

                // Keyboard focus — sends leave to the previously focused
                // surface's client before entering the new one.
                state.set_keyboard_focus(surface_id);
                // Tell the client which output its surface is on so it can
                // determine scale and start rendering.
                if let Some(surf) = state.surfaces.get(&data.wl_surface_id) {
                    for output in &state.outputs {
                        if same_client(output, &surf.wl_surface) {
                            surf.wl_surface.enter(output);
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
                        let s = (sm.scale).max(1);
                        let (lw, lh) = surf
                            .viewport_destination
                            .filter(|&(dw, dh)| dw > 0 && dh > 0)
                            .unwrap_or((sm.width as i32 / s, sm.height as i32 / s));
                        Some((0, 0, lw, lh))
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
        _: &XdgToplevel,
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
                state.surface_meta.remove(wl_surface_id);
                state.cursor_rgba.remove(wl_surface_id);
                if let Some(ref mut vk) = state.vulkan_renderer {
                    vk.remove_surface(wl_surface_id);
                }
                if let Some(held) = state.held_buffers.remove(wl_surface_id) {
                    held.release();
                }
                if let Some(surf) = state.surfaces.get_mut(wl_surface_id) {
                    let sid = surf.surface_id;
                    surf.xdg_toplevel = None;
                    if sid > 0 {
                        state.toplevel_surface_ids.remove(&sid);
                        state.last_reported_size.remove(&sid);
                        state.surface_sizes.remove(&sid);
                        let _ = state
                            .event_tx
                            .send(CompositorEvent::SurfaceDestroyed { surface_id: sid });
                        (state.event_notify)();
                        surf.surface_id = 0;
                    }
                }
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
                            let s = (sm.scale).max(1);
                            let (lw, lh) = surf
                                .viewport_destination
                                .filter(|&(dw, dh)| dw > 0 && dh > 0)
                                .unwrap_or((sm.width as i32 / s, sm.height as i32 / s));
                            Some((0, 0, lw, lh))
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
                // Remove from parent's children list.
                if let Some(parent_id) = state
                    .surfaces
                    .get(&data.wl_surface_id)
                    .and_then(|s| s.parent_surface_id.clone())
                    && let Some(parent) = state.surfaces.get_mut(&parent_id)
                {
                    parent.children.retain(|c| *c != data.wl_surface_id);
                }
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
}

// -- wl_output --
impl GlobalDispatch<WlOutput, ()> for Compositor {
    fn bind(
        state: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlOutput>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
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
        let s120 = state.output_scale_120 as i32;
        let mode_w = state.output_width * s120 / 120;
        let mode_h = state.output_height * s120 / 120;
        output.mode(
            wl_output::Mode::Current | wl_output::Mode::Preferred,
            mode_w,
            mode_h,
            state.output_refresh_mhz as i32,
        );
        if output.version() >= 2 {
            output.scale(((state.output_scale_120 as i32) + 119) / 120);
        }
        if output.version() >= 2 {
            output.done();
        }
        state.outputs.push(output);
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
            state.outputs.retain(|o| o.id() != resource.id());
        }
    }
}

// -- wl_seat --
impl GlobalDispatch<WlSeat, ()> for Compositor {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WlSeat>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let seat = data_init.init(resource, ());
        seat.capabilities(wl_seat::Capability::Keyboard | wl_seat::Capability::Pointer);
        if seat.version() >= 2 {
            seat.name("headless".to_string());
        }
    }
}

impl Dispatch<WlSeat, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlSeat,
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
                state.pointers.push(ptr);
            }
            Request::GetTouch { id } => {
                data_init.init(id, ());
            }
            Request::Release => {}
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
                } else {
                    let _ = state.event_tx.send(CompositorEvent::SurfaceCursor {
                        surface_id: state.focused_surface_id,
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

// -- wl_touch (stub) --
impl Dispatch<wayland_server::protocol::wl_touch::WlTouch, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wayland_server::protocol::wl_touch::WlTouch,
        _: <wayland_server::protocol::wl_touch::WlTouch as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
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
            Request::GetFractionalScale { id, surface: _ } => {
                let fs = data_init.init(id, ());
                // Send the current preferred scale immediately.
                fs.preferred_scale(state.output_scale_120 as u32);
                state.fractional_scales.push(fs);
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
            .retain(|fs| fs.id() != resource.id());
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
            Request::SetSource { .. } => {
                // Source crop — not needed for headless compositor.
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

// =========================================================================
// NEW PROTOCOLS
// =========================================================================

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
                    },
                );
            }
            Request::GetDataDevice { id, seat: _ } => {
                let dd = data_init.init(id, ());
                state.data_devices.push(dd);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataSource, DataSourceData> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &WlDataSource,
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
            Request::Destroy => {}
            _ => {} // SetActions — DnD, ignored
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
        }
    }
}

impl Dispatch<WlDataDevice, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlDataDevice,
        request: <WlDataDevice as Resource>::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wl_data_device::Request;
        match request {
            Request::SetSelection { source, serial: _ } => {
                state.selection_source = source.clone();
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
            Request::Release => {}
            _ => {} // StartDrag — ignored
        }
    }

    fn destroyed(
        state: &mut Self,
        _: wayland_server::backend::ClientId,
        resource: &WlDataDevice,
        _: &(),
    ) {
        state.data_devices.retain(|d| d.id() != resource.id());
    }
}

impl Dispatch<WlDataOffer, DataOfferData> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &WlDataOffer,
        request: <WlDataOffer as Resource>::Request,
        data: &DataOfferData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        use wl_data_offer::Request;
        match request {
            Request::Receive { mime_type, fd } => {
                if data.external {
                    // Write external clipboard data to the fd.
                    if let Some(ref cb) = state.external_clipboard
                        && (cb.mime_type == mime_type
                            || mime_type == "text/plain"
                            || mime_type == "text/plain;charset=utf-8"
                            || mime_type == "UTF8_STRING")
                    {
                        use std::io::Write;
                        let mut f = std::fs::File::from(fd);
                        let _ = f.write_all(&cb.data);
                    }
                } else if let Some(ref src) = state.selection_source {
                    // Forward to the Wayland data source.
                    src.send(mime_type, fd.as_fd());
                }
            }
            Request::Destroy => {}
            _ => {} // Accept, Finish, SetActions — DnD
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

    /// Push external clipboard to all connected wl_data_device objects.
    fn offer_external_clipboard(&mut self) {
        let Some(ref cb) = self.external_clipboard else {
            return;
        };
        let mime = cb.mime_type.clone();
        for dd in &self.data_devices {
            if let Some(client) = dd.client() {
                let offer = client
                    .create_resource::<WlDataOffer, DataOfferData, Compositor>(
                        &self.display_handle,
                        dd.version(),
                        DataOfferData { external: true },
                    )
                    .unwrap();
                dd.data_offer(&offer);
                offer.offer(mime.clone());
                // Offer standard text aliases.
                if mime.starts_with("text/plain") {
                    if mime != "text/plain" {
                        offer.offer("text/plain".to_string());
                    }
                    if mime != "text/plain;charset=utf-8" {
                        offer.offer("text/plain;charset=utf-8".to_string());
                    }
                    offer.offer("UTF8_STRING".to_string());
                }
                dd.selection(Some(&offer));
            }
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
                state.primary_source = source;
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
                    if let Some(ref cb) = state.external_primary {
                        use std::io::Write;
                        let mut f = std::fs::File::from(fd);
                        let _ = f.write_all(&cb.data);
                        let _ = mime_type; // accepted regardless
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
                state.text_inputs.push(TextInputState {
                    resource: ti,
                    enabled: false,
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
        match request {
            Request::Enable => {
                if let Some(ti) = state
                    .text_inputs
                    .iter_mut()
                    .find(|t| t.resource.id() == resource.id())
                {
                    ti.enabled = true;
                }
            }
            Request::Disable => {
                if let Some(ti) = state
                    .text_inputs
                    .iter_mut()
                    .find(|t| t.resource.id() == resource.id())
                {
                    ti.enabled = false;
                }
            }
            Request::Commit => {
                // Client acknowledges our last done; nothing to do.
            }
            Request::Destroy => {
                state
                    .text_inputs
                    .retain(|t| t.resource.id() != resource.id());
            }
            // SetSurroundingText, SetTextChangeCause, SetContentType,
            // SetCursorRectangle — informational; ignored for now.
            _ => {}
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
            Request::Activate {
                token: _,
                surface: _,
            } => {
                // In a headless compositor, activation requests are always
                // granted (focus is managed externally by the browser/CLI).
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
                    surface_id: state.focused_surface_id,
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
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct CompositorHandle {
    pub event_rx: mpsc::Receiver<CompositorEvent>,
    pub command_tx: mpsc::Sender<CompositorCommand>,
    pub socket_name: String,
    pub thread: std::thread::JoinHandle<()>,
    pub shutdown: Arc<AtomicBool>,
    /// Whether the compositor's Vulkan renderer supports Vulkan Video encode.
    pub vulkan_video_encode: bool,
    /// Whether the compositor's Vulkan renderer supports Vulkan Video AV1 encode.
    pub vulkan_video_encode_av1: bool,
    loop_signal: LoopSignal,
}

impl CompositorHandle {
    pub fn wake(&self) {
        self.loop_signal.wakeup();
    }
}

pub fn spawn_compositor(
    verbose: bool,
    event_notify: Arc<dyn Fn() + Send + Sync>,
    gpu_device: &str,
) -> CompositorHandle {
    let _gpu_device = gpu_device.to_string();
    let (event_tx, event_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let (socket_tx, socket_rx) = mpsc::sync_channel(1);
    let (signal_tx, signal_rx) = mpsc::sync_channel::<LoopSignal>(1);
    let (caps_tx, caps_rx) = mpsc::sync_channel::<(bool, bool)>(1);
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

    CompositorHandle {
        event_rx,
        command_tx,
        socket_name,
        thread,
        shutdown,
        vulkan_video_encode,
        vulkan_video_encode_av1,
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
) {
    let mut event_loop: EventLoop<Compositor> =
        EventLoop::try_new().expect("failed to create event loop");
    let loop_signal = event_loop.get_signal();

    let display: Display<Compositor> = Display::new().expect("failed to create display");
    let dh = display.handle();

    // Probe Vulkan early so we know whether DMA-BUF is available
    // before registering Wayland globals.
    eprintln!("[compositor] trying Vulkan renderer for {gpu_device}");
    let vulkan_renderer = super::vulkan_render::VulkanRenderer::try_new(&gpu_device);
    let has_dmabuf = vulkan_renderer.as_ref().is_some_and(|vk| vk.has_dmabuf());
    eprintln!(
        "[compositor] Vulkan renderer: {} (dmabuf={})",
        vulkan_renderer.is_some(),
        has_dmabuf,
    );

    // Create globals.
    dh.create_global::<Compositor, WlCompositor, ()>(6, ());
    dh.create_global::<Compositor, WlSubcompositor, ()>(1, ());
    dh.create_global::<Compositor, XdgWmBase, ()>(6, ());
    dh.create_global::<Compositor, WlShm, ()>(1, ());
    dh.create_global::<Compositor, WlOutput, ()>(4, ());
    dh.create_global::<Compositor, WlSeat, ()>(9, ());
    // Only advertise zwp_linux_dmabuf_v1 when the Vulkan device can
    // actually import DMA-BUFs.  Advertising the global with zero
    // formats confuses clients (Chrome, mpv) into not falling back to
    // wl_shm.
    if has_dmabuf {
        dh.create_global::<Compositor, ZwpLinuxDmabufV1, ()>(4, ());
    }
    dh.create_global::<Compositor, WpViewporter, ()>(1, ());
    dh.create_global::<Compositor, WpFractionalScaleManagerV1, ()>(1, ());
    dh.create_global::<Compositor, ZxdgDecorationManagerV1, ()>(1, ());
    dh.create_global::<Compositor, WlDataDeviceManager, ()>(3, ());
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

    let mut compositor = Compositor {
        display_handle: dh,
        surfaces: HashMap::new(),
        toplevel_surface_ids: HashMap::new(),
        next_surface_id: 1,
        shm_pools: HashMap::new(),
        surface_meta: HashMap::new(),
        dmabuf_params: HashMap::new(),
        vulkan_renderer,
        output_width: 1920,
        output_height: 1080,
        output_refresh_mhz: 60_000,
        output_scale_120: 120,
        outputs: Vec::new(),
        keyboards: Vec::new(),
        pointers: Vec::new(),
        keyboard_keymap_data: keymap_data,
        mods_depressed: 0,
        mods_locked: 0,
        serial: 0,
        event_tx,
        event_notify,
        loop_signal: loop_signal.clone(),
        pending_commits: HashMap::new(),
        focused_surface_id: 0,
        pointer_entered_id: None,
        pending_kb_reenter: false,
        gpu_device,
        verbose,
        shutdown: shutdown.clone(),
        last_reported_size: HashMap::new(),
        surface_sizes: HashMap::new(),
        positioners: HashMap::new(),
        fractional_scales: Vec::new(),
        data_devices: Vec::new(),
        selection_source: None,
        external_clipboard: None,
        primary_devices: Vec::new(),
        primary_source: None,
        external_primary: None,
        relative_pointers: Vec::new(),
        text_inputs: Vec::new(),
        text_input_serial: 0,
        next_activation_token: 1,
        popup_grab_stack: Vec::new(),
        held_buffers: HashMap::new(),
        cursor_rgba: HashMap::new(),
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
            state.cleanup_dead_surfaces();
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
    handle
        .insert_source(socket_source, |_, socket, state| {
            let ls = unsafe { socket.get_mut() };
            if let Some(client_stream) = ls.accept().ok().flatten()
                && let Err(e) = state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState))
                && state.verbose
            {
                eprintln!("[compositor] insert_client error: {e}");
            }
            Ok(PostAction::Continue)
        })
        .expect("failed to insert listening socket");

    if verbose {
        eprintln!("[compositor] entering event loop");
    }

    while !shutdown.load(Ordering::Relaxed) {
        // Process commands.
        while let Ok(cmd) = command_rx.try_recv() {
            match cmd {
                CompositorCommand::Shutdown => {
                    shutdown.store(true, Ordering::Relaxed);
                    return;
                }
                other => compositor.handle_command(other),
            }
        }

        // Shorten the dispatch timeout when the Vulkan renderer has
        // in-flight GPU work so we poll for completion promptly.
        let poll_timeout = if compositor
            .vulkan_renderer
            .as_ref()
            .is_some_and(|vk| vk.has_pending())
        {
            std::time::Duration::from_millis(1)
        } else {
            std::time::Duration::from_secs(1)
        };

        if let Err(e) = event_loop.dispatch(Some(poll_timeout), &mut compositor)
            && verbose
        {
            eprintln!("[compositor] event loop error: {e}");
        }

        // Check for completed Vulkan GPU work.  This runs independently
        // of surface commits so completed frames are flushed to the
        // server without waiting for the next Wayland event.
        if let Some(ref mut vk) = compositor.vulkan_renderer
            && let Some((sid, w, h, pixels)) = vk.try_retire_pending()
        {
            let s120_u32 = (compositor.output_scale_120 as u32).max(120);
            let log_w = (w * 120).div_ceil(s120_u32);
            let log_h = (h * 120).div_ceil(s120_u32);
            compositor
                .pending_commits
                .insert(sid, (w, h, log_w, log_h, pixels));
        }

        if !compositor.pending_commits.is_empty() {
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
