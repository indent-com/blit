use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufMappingMode};
use smithay::backend::allocator::{Buffer, Fourcc, Modifier, Format as DmabufFormat};
use smithay::backend::input::{Axis, ButtonState, KeyState};
use smithay::delegate_compositor;
use smithay::delegate_cursor_shape;
use smithay::delegate_data_device;
use smithay::delegate_dmabuf;
use smithay::delegate_fractional_scale;
use smithay::delegate_output;
use smithay::delegate_text_input_manager;
use smithay::delegate_primary_selection;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_viewporter;
use smithay::delegate_xdg_activation;
use smithay::delegate_xdg_decoration;
use smithay::delegate_xdg_shell;
use smithay::delegate_xdg_toplevel_icon;
use smithay::desktop::{Space, Window};
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, LoopSignal, PostAction};
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::utils::{Serial, Transform, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    self, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
    with_states, with_surface_tree_downward, TraversalAction, get_parent,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
    set_data_device_focus, set_data_device_selection, request_data_device_client_selection,
};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf};
use smithay::wayland::drm_syncobj::{DrmSyncobjHandler, DrmSyncobjState, supports_syncobj_eventfd};
use smithay::backend::drm::DrmDeviceFd;
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shm::{BufferData, ShmHandler, ShmState, with_buffer_contents};
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::tablet_manager::TabletSeatHandler;
use smithay::wayland::fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState};
use smithay::wayland::selection::primary_selection::{PrimarySelectionHandler, PrimarySelectionState, set_primary_focus};
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::wayland::xdg_toplevel_icon::XdgToplevelIconHandler;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;

/// Pixel data in its native format, avoiding unnecessary colorspace conversions.
///
/// The inner buffers are wrapped in `Arc` so that cloning `PixelData` is O(1)
/// (refcount bump) rather than a multi-megabyte memcpy.
#[derive(Clone)]
pub enum PixelData {
    /// BGRA packed pixels (from Wayland SHM buffers).
    /// Layout: [B, G, R, A] per pixel, row-major, no padding.
    Bgra(Arc<Vec<u8>>),
    /// RGBA packed pixels (legacy path / capture conversions).
    /// Layout: [R, G, B, A] per pixel, row-major, no padding.
    Rgba(Arc<Vec<u8>>),
    /// NV12 planar: Y plane followed by interleaved UV plane.
    /// `y_stride` and `uv_stride` may differ from width (DMA-BUF padding).
    /// Stored contiguously: `data[..y_stride*height]` is Y,
    /// `data[y_stride*height..]` is UV.
    Nv12 {
        data: Arc<Vec<u8>>,
        y_stride: usize,
        uv_stride: usize,
    },
    /// DMA-BUF file descriptor for zero-copy GPU encoding.
    ///
    /// The fd is dup'd from the Wayland buffer — the original wl_buffer has
    /// been released, but this fd keeps the DMA-BUF kernel object alive.
    /// The encoder imports this directly into GPU memory (VA-API VPP or
    /// CUDA-EGL) without any CPU-side pixel copies.
    DmaBuf {
        fd: Arc<std::os::fd::OwnedFd>,
        /// DRM fourcc (e.g. DRM_FORMAT_ARGB8888)
        fourcc: u32,
        /// DRM format modifier (e.g. DRM_FORMAT_MOD_LINEAR)
        modifier: u64,
        stride: u32,
        offset: u32,
    },
}

/// A single layer in a composited surface tree.
#[derive(Clone)]
pub struct PixelLayer {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub pixels: PixelData,
}

/// DRM fourcc constants used in PixelData::DmaBuf.
pub mod drm_fourcc {
    /// DRM_FORMAT_ARGB8888 — [B,G,R,A] in memory on little-endian.
    pub const ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
    /// DRM_FORMAT_XRGB8888 — [B,G,R,X] in memory on little-endian.
    pub const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
    /// DRM_FORMAT_ABGR8888 — [R,G,B,A] in memory on little-endian.
    pub const ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
    /// DRM_FORMAT_XBGR8888 — [R,G,B,X] in memory on little-endian.
    pub const XBGR8888: u32 = u32::from_le_bytes(*b"XB24");
    /// DRM_FORMAT_NV12
    pub const NV12: u32 = u32::from_le_bytes(*b"NV12");
}

impl PixelData {
    /// Convert to RGBA for consumers that require it (screenshots, etc.).
    /// Panics for DmaBuf variant — callers must handle that case separately.
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
                fd, fourcc, stride, ..
            } => {
                // Try CPU readback of DMA-BUF via sync + mmap.
                use std::os::fd::AsRawFd;
                let raw = fd.as_raw_fd();
                let stride_usize = *stride as usize;
                let map_size = stride_usize * h;
                if map_size == 0 {
                    return Vec::new();
                }
                // Sync to ensure GPU writes are visible to CPU.
                let sync_start: u64 = 1 | 4; // DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ
                unsafe { libc::ioctl(raw, 0x40086200u64 as _, &sync_start) };
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
                    return Vec::new();
                }
                let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, map_size) };
                let row_bytes = w * 4;
                let mut pixels = Vec::with_capacity(w * h * 4);
                for row in 0..h {
                    let start = row * stride_usize;
                    if start + row_bytes <= slice.len() {
                        pixels.extend_from_slice(&slice[start..start + row_bytes]);
                    }
                }
                // Force alpha=255 for XRGB/XBGR formats.
                if matches!(*fourcc, drm_fourcc::XRGB8888 | drm_fourcc::XBGR8888) {
                    for px in pixels.chunks_exact_mut(4) {
                        px[3] = 255;
                    }
                }
                let sync_end: u64 = 2 | 4; // DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ
                unsafe {
                    libc::ioctl(raw, 0x40086200u64 as _, &sync_end);
                    libc::munmap(ptr, map_size);
                }
                pixels
            }
        }
    }

    /// Return the pixel data as BGRA bytes (for compositing).
    /// For DmaBuf, attempts mmap readback.  Returns empty on failure.
    pub fn to_bgra_vec(&self, width: u32, height: u32) -> Vec<u8> {
        match self {
            PixelData::Bgra(v) => v.as_ref().to_vec(),
            PixelData::Rgba(v) => {
                // RGBA → BGRA swap
                let mut bgra = v.as_ref().to_vec();
                for px in bgra.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                bgra
            }
            PixelData::DmaBuf {
                fd, stride, fourcc, ..
            } => {
                // Try mmap readback for linear DMA-BUFs.
                use std::os::fd::AsRawFd;
                let raw = fd.as_raw_fd();
                let s = *stride as usize;
                let h = height as usize;
                let w = width as usize;
                let map_size = s * h;
                if map_size == 0 {
                    return Vec::new();
                }
                let sync_start: u64 = 1 | 4;
                unsafe { libc::ioctl(raw, 0x40086200u64 as _, &sync_start) };
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
                    return Vec::new();
                }
                let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, map_size) };
                let row_bytes = w * 4;
                let is_bgr = matches!(fourcc, 0x34325241 | 0x34325258);
                let mut bgra = Vec::with_capacity(w * h * 4);
                for row in 0..h {
                    let src = &slice[row * s..row * s + row_bytes.min(slice.len() - row * s)];
                    if is_bgr {
                        bgra.extend_from_slice(src);
                    } else {
                        for px in src.chunks_exact(4) {
                            bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                        }
                    }
                }
                let sync_end: u64 = 2 | 4;
                unsafe {
                    libc::ioctl(raw, 0x40086200u64 as _, &sync_end);
                    libc::munmap(ptr, map_size);
                }
                bgra
            }
            _ => Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            PixelData::Bgra(v) | PixelData::Rgba(v) => v.is_empty(),
            PixelData::Nv12 { data, .. } => data.is_empty(),
            PixelData::DmaBuf { .. } => false,
        }
    }

    /// Returns true if this is a DMA-BUF reference (GPU-resident).
    pub fn is_dmabuf(&self) -> bool {
        matches!(self, PixelData::DmaBuf { .. })
    }
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
        surface_id: u16,
        mime_type: String,
        data: Vec<u8>,
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
        /// DPR in 1/120th units (Wayland convention): 120 = 1×, 240 = 2×.  0 = unchanged.
        scale_120: u16,
    },
    SurfaceFocus {
        surface_id: u16,
    },
    SurfaceClose {
        surface_id: u16,
    },
    ClipboardOffer {
        surface_id: u16,
        mime_type: String,
        data: Vec<u8>,
    },
    Capture {
        surface_id: u16,
        reply: mpsc::SyncSender<Option<(u32, u32, Vec<u8>)>>,
    },
    /// Fire pending wl_surface.frame callbacks for a surface so the
    /// client will paint and commit its next frame.  Send this when
    /// the server is ready to consume a new frame (streaming or capture).
    RequestFrame {
        surface_id: u16,
    },
    /// Release the given keys (evdev keycodes) in the compositor's XKB
    /// state.  Sent by the server when a transport client disconnects so
    /// that stuck modifiers / runaway key-repeat don't persist.
    ReleaseKeys {
        keycodes: Vec<u32>,
    },
    Shutdown,
}

struct SurfaceInfo {
    surface_id: u16,
    window: Window,
    last_width: u32,
    last_height: u32,
    last_title: String,
    last_app_id: String,
    /// Suppress repeated "commit with no readable buffer" warnings.
    unreadable_warned: bool,
}

struct ClientData {
    compositor_state: CompositorClientState,
}

impl smithay::reexports::wayland_server::backend::ClientData for ClientData {
    fn initialized(&self, _client_id: smithay::reexports::wayland_server::backend::ClientId) {}
    fn disconnected(
        &self,
        _client_id: smithay::reexports::wayland_server::backend::ClientId,
        _reason: smithay::reexports::wayland_server::backend::DisconnectReason,
    ) {
    }
}

pub struct Compositor {
    display_handle: DisplayHandle,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    seat_state: SeatState<Self>,
    data_device_state: DataDeviceState,
    #[allow(dead_code)]
    viewporter_state: ViewporterState,
    #[allow(dead_code)]
    xdg_decoration_state: XdgDecorationState,
    dmabuf_state: DmabufState,
    #[allow(dead_code)]
    dmabuf_global: DmabufGlobal,
    syncobj_state: Option<DrmSyncobjState>,
    primary_selection_state: PrimarySelectionState,
    activation_state: XdgActivationState,
    seat: Seat<Self>,
    output: Output,
    space: Space<Window>,

    surfaces: HashMap<u64, SurfaceInfo>,
    surface_lookup: HashMap<u16, u64>,
    next_surface_id: u16,

    event_tx: mpsc::Sender<CompositorEvent>,
    event_notify: Arc<dyn Fn() + Send + Sync>,
    loop_signal: LoopSignal,

    verbose: bool,

    /// The surface_id of the currently keyboard-focused surface (0 = none).
    focused_surface_id: u16,

    /// Buffered pixel data from commits within the current event-loop
    /// dispatch.  Flushed after dispatch returns so that only the LAST
    /// commit per surface per iteration is sent to the server.
    pending_commits: HashMap<u16, (u32, u32, PixelData)>,

    /// Per-wl_surface cache of the last successfully read pixel data.
    /// Keyed by wl_surface protocol_id.  When a toplevel commits, its
    /// subsurfaces may not have re-committed since the last read;
    /// `BufferAssignment::NewBuffer` is only set on the subsurface's own
    /// commit.  Without this cache, subsurface content silently disappears
    /// from the composite whenever the parent commits independently.
    surface_pixel_cache: HashMap<u64, (u32, u32, PixelData)>,

    /// GPU renderer for compositing surfaces.  None if GPU unavailable.
    gpu_renderer: Option<super::render::SurfaceRenderer>,
}

impl Compositor {
    /// Send buffered SurfaceCommit events to the server.  Called after
    /// event_loop.dispatch() returns so that multiple commits within a
    /// single dispatch cycle are coalesced into one event per surface.
    fn flush_pending_commits(&mut self) {
        for (surface_id, (width, height, pixels)) in self.pending_commits.drain() {
            let _ = self.event_tx.send(CompositorEvent::SurfaceCommit {
                surface_id,
                width,
                height,
                pixels,
            });
        }
        (self.event_notify)();
    }

    fn allocate_surface_id(&mut self) -> u16 {
        // Skip IDs already in use to prevent collisions after u16 wraparound
        // (possible in long-running sessions with >65535 surface creates).
        let mut id = self.next_surface_id;
        let start = id;
        loop {
            if !self.surface_lookup.contains_key(&id) {
                break;
            }
            id = id.wrapping_add(1);
            if id == 0 {
                id = 1;
            }
            if id == start {
                // Exhausted all IDs — extremely unlikely (65535 concurrent surfaces).
                break;
            }
        }
        self.next_surface_id = id.wrapping_add(1);
        if self.next_surface_id == 0 {
            self.next_surface_id = 1;
        }
        id
    }

    fn handle_command(&mut self, cmd: CompositorCommand) {
        match cmd {
            CompositorCommand::KeyInput {
                surface_id,
                keycode,
                pressed,
            } => {
                if let Some(&obj_id) = self.surface_lookup.get(&surface_id)
                    && let Some(info) = self.surfaces.get(&obj_id)
                    && let Some(toplevel) = info.window.toplevel()
                    && let Some(keyboard) = self.seat.get_keyboard()
                {
                    if self.verbose {
                        eprintln!(
                            "[compositor] key: sid={surface_id} evdev={keycode} pressed={pressed}"
                        );
                    }
                    let serial = SERIAL_COUNTER.next_serial();
                    let time = elapsed_ms();
                    let state = if pressed {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    };
                    keyboard.set_focus(self, Some(toplevel.wl_surface().clone()), serial);
                    // smithay expects XKB keycodes (evdev + 8).  The browser
                    // sends raw evdev scancodes, so we add the offset here,
                    // matching what smithay's libinput and winit backends do.
                    keyboard.input::<(), _>(
                        self,
                        (keycode + 8).into(),
                        state,
                        serial,
                        time,
                        |_, _, _| FilterResult::Forward,
                    );
                }
            }
            CompositorCommand::PointerMotion { surface_id, x, y } => {
                if let Some(&obj_id) = self.surface_lookup.get(&surface_id)
                    && let Some(info) = self.surfaces.get(&obj_id)
                    && let Some(toplevel) = info.window.toplevel()
                    && let Some(pointer) = self.seat.get_pointer()
                {
                    let serial = SERIAL_COUNTER.next_serial();
                    let time = elapsed_ms();
                    let wl_surface = toplevel.wl_surface().clone();
                    // smithay's ClickGrab redirects all motion events to the
                    // surface that received the original button-press, ignoring
                    // the focus we pass here.  In blit every surface has its
                    // own canvas, so a stale grab (e.g. mouseup lost when the
                    // user switched surfaces) would permanently block input to
                    // every other surface.  Clear the grab when the target
                    // surface differs from the grabbed one.
                    if pointer.is_grabbed() {
                        let stale = pointer
                            .grab_start_data()
                            .and_then(|d| d.focus.as_ref().map(|(s, _)| s.id() != wl_surface.id()))
                            .unwrap_or(false);
                        if stale {
                            pointer.unset_grab(self, serial, time);
                        }
                    }
                    pointer.motion(
                        self,
                        Some((wl_surface, (0.0, 0.0).into())),
                        &MotionEvent {
                            location: (x, y).into(),
                            serial,
                            time,
                        },
                    );
                    pointer.frame(self);
                }
            }
            CompositorCommand::PointerButton {
                surface_id,
                button,
                pressed,
            } => {
                if let Some(&obj_id) = self.surface_lookup.get(&surface_id)
                    && self.surfaces.contains_key(&obj_id)
                    && let Some(pointer) = self.seat.get_pointer()
                {
                    let serial = SERIAL_COUNTER.next_serial();
                    let state = if pressed {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    };
                    pointer.button(
                        self,
                        &ButtonEvent {
                            button,
                            state,
                            serial,
                            time: elapsed_ms(),
                        },
                    );
                    pointer.frame(self);
                }
            }
            CompositorCommand::PointerAxis {
                surface_id: _,
                axis,
                value,
            } => {
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let ax = if axis == 0 {
                    Axis::Vertical
                } else {
                    Axis::Horizontal
                };
                pointer.axis(self, AxisFrame::new(elapsed_ms()).value(ax, value));
                pointer.frame(self);
            }
            CompositorCommand::SurfaceResize {
                surface_id: _,
                width,
                height,
                scale_120,
            } => {
                // Update output scale if the client reported a DPR.
                // scale_120 is already in Wayland fractional_scale units (1/120th).
                let scale_frac = if scale_120 >= 120 {
                    scale_120 as f64
                } else {
                    120.0
                };
                let cur = self.output.current_scale().fractional_scale();
                if (cur - scale_frac).abs() > 0.01 {
                    // Integer scale for wl_output: round to nearest.
                    let int_scale = ((scale_frac / 120.0) + 0.5) as i32;
                    self.output.change_current_state(
                        None,
                        None,
                        Some(smithay::output::Scale::Custom {
                            advertised_integer: int_scale.max(1),
                            fractional: scale_frac,
                        }),
                        None,
                    );
                }

                // width/height are in physical pixels.  Convert to logical
                // pixels for the toplevel configure (Wayland uses logical).
                let scale_f = scale_frac / 120.0;
                let logical_w = ((width as f64) / scale_f).round() as i32;
                let logical_h = ((height as f64) / scale_f).round() as i32;

                // Update the output mode to match the physical size.
                let mode = smithay::output::Mode {
                    size: (width as i32, height as i32).into(),
                    refresh: 60_000,
                };
                self.output
                    .change_current_state(Some(mode), None, None, None);
                self.output.set_preferred(mode);

                // Configure all toplevel surfaces to fill the output.
                for info in self.surfaces.values() {
                    if let Some(toplevel) = info.window.toplevel() {
                        toplevel.with_pending_state(|state| {
                            state.size = Some((logical_w.max(1), logical_h.max(1)).into());
                        });
                        toplevel.send_pending_configure();
                    }
                }
                // Refresh so surfaces receive the updated output scale via
                // wl_surface.enter and wp_fractional_scale_v1.preferred_scale.
                self.space.refresh();
            }
            CompositorCommand::SurfaceFocus { surface_id } => {
                if let Some(&obj_id) = self.surface_lookup.get(&surface_id)
                    && let Some(info) = self.surfaces.get(&obj_id)
                    && let Some(toplevel) = info.window.toplevel()
                    && let Some(keyboard) = self.seat.get_keyboard()
                {
                    let serial = SERIAL_COUNTER.next_serial();
                    keyboard.set_focus(self, Some(toplevel.wl_surface().clone()), serial);
                }
            }
            CompositorCommand::SurfaceClose { surface_id } => {
                if let Some(&obj_id) = self.surface_lookup.get(&surface_id)
                    && let Some(info) = self.surfaces.get(&obj_id)
                    && let Some(toplevel) = info.window.toplevel()
                {
                    toplevel.send_close();
                }
            }
            CompositorCommand::ClipboardOffer {
                surface_id: _,
                mime_type,
                data,
            } => {
                // Inject the browser/remote clipboard as a compositor-owned
                // selection so that focused Wayland clients can paste it.
                let mime_types = if mime_type == "text/plain" {
                    vec![
                        "text/plain".to_string(),
                        "text/plain;charset=utf-8".to_string(),
                        "UTF8_STRING".to_string(),
                        "TEXT".to_string(),
                        "STRING".to_string(),
                    ]
                } else {
                    vec![mime_type]
                };
                set_data_device_selection(
                    &self.display_handle,
                    &self.seat,
                    mime_types,
                    Arc::new(data),
                );
            }
            CompositorCommand::Capture { surface_id, reply } => {
                let result = if let Some(&obj_id) = self.surface_lookup.get(&surface_id)
                    && let Some(info) = self.surfaces.get(&obj_id)
                    && let Some(toplevel) = info.window.toplevel()
                {
                    let wl_surface = toplevel.wl_surface().clone();
                    self.read_surface_pixels(&wl_surface)
                } else {
                    None
                };
                let _ = reply.send(result);
            }
            CompositorCommand::RequestFrame { surface_id } => {
                self.fire_frame_callbacks(surface_id);
            }
            CompositorCommand::ReleaseKeys { keycodes } => {
                if let Some(keyboard) = self.seat.get_keyboard() {
                    let serial = SERIAL_COUNTER.next_serial();
                    let time = elapsed_ms();
                    for keycode in keycodes {
                        keyboard.input::<(), _>(
                            self,
                            (keycode + 8).into(),
                            KeyState::Released,
                            serial,
                            time,
                            |_, _, _| FilterResult::Forward,
                        );
                    }
                }
            }
            CompositorCommand::Shutdown => {
                self.loop_signal.stop();
            }
        }
    }

    /// Fire pending `wl_surface.frame` callbacks for a specific surface.
    fn fire_frame_callbacks(&self, surface_id: u16) {
        if let Some(&obj_id) = self.surface_lookup.get(&surface_id)
            && let Some(info) = self.surfaces.get(&obj_id)
            && let Some(toplevel) = info.window.toplevel()
        {
            let surface = toplevel.wl_surface().clone();
            let time = elapsed_ms();
            let mut fired = 0u32;
            with_surface_tree_downward(
                &surface,
                (),
                |_, _, &()| TraversalAction::DoChildren(()),
                |_, states, &()| {
                    for callback in states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .frame_callbacks
                        .drain(..)
                    {
                        callback.done(time);
                        fired += 1;
                    }
                },
                |_, _, &()| true,
            );
            let _ = fired;
        }
    }

    fn read_surface_pixels(&mut self, surface: &WlSurface) -> Option<(u32, u32, Vec<u8>)> {
        let mut result: Option<(u32, u32, PixelData)> = None;
        with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            let attrs = guard.current();
            if let Some(compositor::BufferAssignment::NewBuffer(buffer)) = attrs.buffer.as_ref() {
                result = read_shm_buffer(buffer);
                if result.is_none()
                    && let Ok(dmabuf) = get_dmabuf(buffer)
                {
                    result = read_dmabuf_pixels(dmabuf);
                }
            }
        });
        // Convert to RGBA for the capture path (used only by dead-code Capture command).
        result.map(|(w, h, pd)| (w, h, pd.to_rgba(w, h)))
    }
}

/// Read an SHM buffer into `PixelData::Bgra`, forcing alpha=255 for
/// XRGB8888 format (where the alpha byte is undefined and typically 0).
fn read_shm_buffer(buffer: &wl_buffer::WlBuffer) -> Option<(u32, u32, PixelData)> {
    let mut result = None;
    let _ = with_buffer_contents(buffer, |ptr, len, data: BufferData| {
        let width = data.width as u32;
        let height = data.height as u32;
        let stride = data.stride as usize;
        let offset = data.offset as usize;
        let pixel_data = unsafe { std::slice::from_raw_parts(ptr, len) };
        let row_bytes = width as usize * 4;
        let mut bgra = if stride == row_bytes
            && offset == 0
            && pixel_data.len() >= row_bytes * height as usize
        {
            pixel_data[..row_bytes * height as usize].to_vec()
        } else {
            let mut packed = Vec::with_capacity(row_bytes * height as usize);
            for row in 0..height as usize {
                let row_start = offset + row * stride;
                let row_end = row_start + row_bytes;
                if row_end <= pixel_data.len() {
                    packed.extend_from_slice(&pixel_data[row_start..row_end]);
                }
            }
            packed
        };
        // XRGB8888: the alpha byte is undefined (clients typically leave it
        // as 0).  Force opaque so that captures and any alpha-aware path
        // treat pixels correctly.
        if data.format == wl_shm::Format::Xrgb8888 || data.format == wl_shm::Format::Xbgr8888 {
            for px in bgra.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
        // ABGR/XBGR are [R,G,B,A] in memory — store as PixelData::Rgba.
        if matches!(
            data.format,
            wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888
        ) {
            result = Some((width, height, PixelData::Rgba(Arc::new(bgra))));
        } else {
            result = Some((width, height, PixelData::Bgra(Arc::new(bgra))));
        }
    });
    result
}

fn elapsed_ms() -> u32 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u32
}

impl CompositorHandler for Compositor {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientData>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Update RendererSurfaceState so Smithay's render element pipeline
        // can produce elements from imported textures.
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);

        // If this is a subsurface, walk up to the toplevel root.
        let (root_surface, key, surface_id) = {
            let key = surface.id().protocol_id() as u64;
            if let Some(info) = self.surfaces.get(&key) {
                (surface.clone(), key, info.surface_id)
            } else {
                // Not a toplevel — walk up to find the parent toplevel.
                let mut current = surface.clone();
                while let Some(parent) = get_parent(&current) {
                    current = parent;
                }
                let parent_key = current.id().protocol_id() as u64;
                match self.surfaces.get(&parent_key) {
                    Some(info) => (current, parent_key, info.surface_id),
                    None => return, // orphan subsurface
                }
            }
        };

        let mut committed_pixels: Option<(u32, u32, PixelData)> = None;
        let mut new_title = String::new();
        let mut new_app_id = String::new();

        let have_pending = self.pending_commits.contains_key(&surface_id);
        let warned_unreadable = self.surfaces.get(&key).is_none_or(|i| i.unreadable_warned);

        if !have_pending {
            // Use the xdg_toplevel geometry as the render region — this
            // is the content rect EXCLUDING CSD shadows.  Clients like
            // Chromium draw shadows as part of the surface buffer, but
            // the geometry tells us exactly where the real window is.
            let geometry = self.surfaces.get(&key).map(|i| i.window.geometry());

            // Try GPU renderer (handles SHM + DMA-BUF + subsurfaces).
            if let Some(renderer) = &mut self.gpu_renderer
                && let Some((w, h, bgra)) =
                    renderer.render_surface(&root_surface, geometry.as_ref())
            {
                committed_pixels = Some((w, h, PixelData::Bgra(Arc::new(bgra))));
            }

            // GPU unavailable or failed — fall back to manual SHM readback
            // for the root surface only (no subsurface compositing).
            if committed_pixels.is_none() {
                with_states(&root_surface, |states| {
                    let mut guard = states.cached_state.get::<SurfaceAttributes>();
                    let attrs = guard.current();
                    if let Some(compositor::BufferAssignment::NewBuffer(buffer)) =
                        attrs.buffer.as_ref()
                        && let Some((w, h, pd)) = read_shm_buffer(buffer)
                    {
                        committed_pixels = Some((w, h, pd));
                    }
                });
            }

            if committed_pixels.is_none() && !warned_unreadable {
                eprintln!(
                    "compositor: commit with no readable buffer — suppressing further warnings for this surface"
                );
            }
        }

        // Release all buffers in the tree so clients can reuse them.
        with_surface_tree_downward(
            &root_surface,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |_, states, _| {
                let mut guard = states.cached_state.get::<SurfaceAttributes>();
                let attrs = guard.current();
                if let Some(compositor::BufferAssignment::NewBuffer(buffer)) = attrs.buffer.as_ref()
                {
                    buffer.release();
                }
            },
            |_, _, _| true,
        );

        // Read title/app_id from the toplevel.
        with_states(&root_surface, |states| {
            if let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() {
                let lock = data.lock().unwrap();
                new_title = lock.title.clone().unwrap_or_default();
                new_app_id = lock.app_id.clone().unwrap_or_default();
            }
        });

        // Mark surface as warned if we hit an unreadable buffer.
        if committed_pixels.is_none()
            && !have_pending
            && !warned_unreadable
            && let Some(info) = self.surfaces.get_mut(&key)
        {
            info.unreadable_warned = true;
        }

        if let Some(info) = self.surfaces.get_mut(&key)
            && new_title != info.last_title
        {
            info.last_title = new_title.clone();
            let _ = self.event_tx.send(CompositorEvent::SurfaceTitle {
                surface_id,
                title: new_title,
            });
        }

        if let Some(info) = self.surfaces.get_mut(&key)
            && new_app_id != info.last_app_id
        {
            info.last_app_id = new_app_id.clone();
            let _ = self.event_tx.send(CompositorEvent::SurfaceAppId {
                surface_id,
                app_id: new_app_id,
            });
        }

        if let Some((width, height, pixels)) = committed_pixels {
            let info = self.surfaces.get_mut(&key).unwrap();
            if width != info.last_width || height != info.last_height {
                info.last_width = width;
                info.last_height = height;
                let _ = self.event_tx.send(CompositorEvent::SurfaceResized {
                    surface_id,
                    width: width as u16,
                    height: height as u16,
                });
            }

            if !pixels.is_empty() {
                self.pending_commits
                    .insert(surface_id, (width, height, pixels));
            }
        }

        // Frame callbacks are fired in flush_pending_commits() — once per
        // dispatch cycle — rather than on every commit to avoid hot loops.
    }
}

impl BufferHandler for Compositor {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Compositor {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for Compositor {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        if self.verbose {
            eprintln!("[compositor] new_toplevel");
        }
        let window = Window::new_wayland_window(surface.clone());
        let wl_surface = surface.wl_surface().clone();
        let key = wl_surface.id().protocol_id() as u64;
        let surface_id = self.allocate_surface_id();

        self.space.map_element(window.clone(), (0, 0), false);
        self.space.refresh();

        let info = SurfaceInfo {
            surface_id,
            window,
            last_width: 0,
            last_height: 0,
            last_title: String::new(),
            last_app_id: String::new(),
            unreadable_warned: false,
        };
        self.surfaces.insert(key, info);
        self.surface_lookup.insert(surface_id, key);

        surface.with_pending_state(|state| {
            state.states.set(
                smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated,
            );
        });
        surface.send_configure();

        let _ = self.event_tx.send(CompositorEvent::SurfaceCreated {
            surface_id,
            title: String::new(),
            app_id: String::new(),
            parent_id: 0,
            width: 0,
            height: 0,
        });
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        let key = wl_surface.id().protocol_id() as u64;
        self.surface_pixel_cache.remove(&key);
        if let Some(info) = self.surfaces.remove(&key) {
            self.surface_lookup.remove(&info.surface_id);
            self.space.unmap_elem(&info.window);
            let _ = self.event_tx.send(CompositorEvent::SurfaceDestroyed {
                surface_id: info.surface_id,
            });
        }
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

impl OutputHandler for Compositor {}

impl SeatHandler for Compositor {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        if let Some(surface) = focused {
            let key = surface.id().protocol_id() as u64;
            if let Some(info) = self.surfaces.get(&key) {
                self.focused_surface_id = info.surface_id;
            }
        }
        let client = focused.and_then(|s| self.display_handle.get_client(s.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
    }
}

impl SelectionHandler for Compositor {
    type SelectionUserData = Arc<Vec<u8>>;

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        seat: Seat<Self>,
    ) {
        if ty != SelectionTarget::Clipboard {
            return;
        }
        let Some(source) = source else { return };
        let mime_types = source.mime_types();

        // Pick the best text mime type the source offers.
        let preferred = [
            "text/plain;charset=utf-8",
            "text/plain",
            "UTF8_STRING",
            "TEXT",
            "STRING",
        ];
        let Some(mime) = preferred
            .iter()
            .map(|m| m.to_string())
            .find(|m| mime_types.contains(m))
        else {
            return;
        };

        // Create a socketpair: the write end goes to the Wayland client,
        // we read clipboard data from the read end on a helper thread.
        let (mut read_stream, write_stream) = match std::os::unix::net::UnixStream::pair() {
            Ok(pair) => pair,
            Err(_) => return,
        };
        let write_fd: OwnedFd = write_stream.into();

        if request_data_device_client_selection::<Self>(&seat, mime.clone(), write_fd).is_err() {
            return;
        }

        let event_tx = self.event_tx.clone();
        let event_notify = self.event_notify.clone();
        let surface_id = self.focused_surface_id;
        std::thread::Builder::new()
            .name("clipboard-read".into())
            .spawn(move || {
                use std::io::Read;
                const MAX_CLIPBOARD_SIZE: usize = 16 * 1024 * 1024; // 16 MiB
                let _ = read_stream.set_read_timeout(Some(std::time::Duration::from_secs(1)));
                let mut data = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    match read_stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            data.extend_from_slice(&buf[..n]);
                            if data.len() > MAX_CLIPBOARD_SIZE {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                data.truncate(MAX_CLIPBOARD_SIZE);
                if !data.is_empty() {
                    let _ = event_tx.send(CompositorEvent::ClipboardContent {
                        surface_id,
                        mime_type: mime,
                        data,
                    });
                    (event_notify)();
                }
            })
            .expect("failed to spawn clipboard-read thread");
    }

    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        _mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        user_data: &Self::SelectionUserData,
    ) {
        // Write the compositor-owned clipboard data to the requesting client's fd.
        let data = user_data.clone();
        std::thread::Builder::new()
            .name("clipboard-write".into())
            .spawn(move || {
                use std::io::Write;
                let mut file = std::fs::File::from(fd);
                let _ = file.write_all(&data);
            })
            .expect("failed to spawn clipboard-write thread");
    }
}

impl DataDeviceHandler for Compositor {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for Compositor {}
impl ServerDndGrabHandler for Compositor {}

impl XdgDecorationHandler for Compositor {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }
}

impl DmabufHandler for Compositor {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        let _ = notifier.successful::<Compositor>();
    }
}

impl DrmSyncobjHandler for Compositor {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.syncobj_state.as_mut()
    }
}

fn with_dmabuf_plane_bytes<T>(
    dmabuf: &Dmabuf,
    plane_idx: usize,
    f: impl FnOnce(&[u8]) -> Option<T>,
) -> Option<T> {
    let _ = dmabuf.sync_plane(
        plane_idx,
        smithay::backend::allocator::dmabuf::DmabufSyncFlags::START
            | smithay::backend::allocator::dmabuf::DmabufSyncFlags::READ,
    );
    struct PlaneSyncGuard<'a> {
        dmabuf: &'a Dmabuf,
        plane_idx: usize,
    }

    impl Drop for PlaneSyncGuard<'_> {
        fn drop(&mut self) {
            let _ = self.dmabuf.sync_plane(
                self.plane_idx,
                smithay::backend::allocator::dmabuf::DmabufSyncFlags::END
                    | smithay::backend::allocator::dmabuf::DmabufSyncFlags::READ,
            );
        }
    }

    let _sync_guard = PlaneSyncGuard { dmabuf, plane_idx };
    let mapping = dmabuf.map_plane(plane_idx, DmabufMappingMode::READ).ok()?;
    let ptr = mapping.ptr() as *const u8;
    let len = mapping.length();
    let plane_data = unsafe { std::slice::from_raw_parts(ptr, len) };
    f(plane_data)
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

fn read_le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let raw = bytes.get(offset..end)?;
    Some(u16::from_le_bytes([raw[0], raw[1]]))
}

/// Direct mmap readback of a packed BGRA/RGBA DMA-BUF fd.
/// Used for anonymous heap fds (Vulkan wsi) where Smithay's map_plane fails.
/// Must be called BEFORE wl_buffer.release() to avoid a race with the client.
#[allow(dead_code)]
fn read_packed_dmabuf_fd(
    raw_fd: std::os::fd::RawFd,
    stride: usize,
    width: usize,
    height: usize,
    format: Fourcc,
) -> Option<PixelData> {
    let file_size = unsafe { libc::lseek(raw_fd, 0, libc::SEEK_END) };
    if file_size <= 0 {
        return None;
    }
    let map_len = file_size as usize;

    #[repr(C)]
    struct DmaBufSync {
        flags: u64,
    }
    const DMA_BUF_SYNC_READ: u64 = 1;
    const DMA_BUF_SYNC_START: u64 = 0;
    const DMA_BUF_SYNC_END: u64 = 4;
    const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x40086200;

    let sync_start = DmaBufSync {
        flags: DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ,
    };
    unsafe {
        libc::ioctl(raw_fd, DMA_BUF_IOCTL_SYNC as _, &sync_start);
    }

    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            map_len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            raw_fd,
            0,
        )
    };

    let result = if ptr == libc::MAP_FAILED {
        None
    } else {
        let plane_data = unsafe { std::slice::from_raw_parts(ptr as *const u8, map_len) };
        let r = read_packed_dmabuf(plane_data, stride, width, height, false, format);
        unsafe {
            libc::munmap(ptr, map_len);
        }
        r
    };

    let sync_end = DmaBufSync {
        flags: DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ,
    };
    unsafe {
        libc::ioctl(raw_fd, DMA_BUF_IOCTL_SYNC as _, &sync_end);
    }

    result
}

/// Read a packed 4-byte DMA-BUF into native pixel data.
///
/// ARGB8888 / XRGB8888 are BGRA in memory on little-endian — pass straight
/// through as `PixelData::Bgra` with zero per-pixel work.  ABGR8888 /
/// XBGR8888 are RGBA in memory — likewise zero swizzle.
fn read_packed_dmabuf(
    plane_data: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    y_inverted: bool,
    format: Fourcc,
) -> Option<PixelData> {
    let row_bytes = width * 4;
    let total = row_bytes * height;

    // Determine whether the raw bytes are BGRA or RGBA in memory, and
    // whether the alpha channel is real or must be forced to 0xFF.
    let (is_bgra, force_opaque) = match format {
        Fourcc::Argb8888 => (true, false),  // BGRA in memory, real alpha
        Fourcc::Xrgb8888 => (true, true),   // BGRX — alpha undefined
        Fourcc::Abgr8888 => (false, false), // RGBA in memory, real alpha
        Fourcc::Xbgr8888 => (false, true),  // RGBX — alpha undefined
        _ => return None,
    };

    // Fast path: contiguous, not y-inverted, no padding.
    if !y_inverted && stride == row_bytes && plane_data.len() >= total {
        let mut buf = plane_data[..total].to_vec();
        if force_opaque {
            // Stamp alpha = 255 on every 4th byte.
            for px in buf.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
        return Some(if is_bgra {
            PixelData::Bgra(Arc::new(buf))
        } else {
            PixelData::Rgba(Arc::new(buf))
        });
    }

    // Slow path: stride padding or y-inversion — pack rows.
    let mut buf = Vec::with_capacity(total);
    for row in 0..height {
        let src_row = if y_inverted { height - 1 - row } else { row };
        let row_start = src_row * stride;
        let row_end = row_start + row_bytes;
        if row_end > plane_data.len() {
            return None;
        }
        buf.extend_from_slice(&plane_data[row_start..row_end]);
    }
    if force_opaque {
        for px in buf.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }
    Some(if is_bgra {
        PixelData::Bgra(Arc::new(buf))
    } else {
        PixelData::Rgba(Arc::new(buf))
    })
}

#[cfg(test)]
fn read_nv12_dmabuf(
    y_plane: &[u8],
    y_stride: usize,
    uv_plane: &[u8],
    uv_stride: usize,
    width: usize,
    height: usize,
    y_inverted: bool,
) -> Option<Vec<u8>> {
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return None;
    }

    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let src_row = if y_inverted { height - 1 - row } else { row };
        let y_row_start = src_row * y_stride;
        let uv_row_start = (src_row / 2) * uv_stride;
        for col in 0..width {
            let y = y_plane[y_row_start + col];
            let uv_idx = uv_row_start + (col / 2) * 2;
            let u = uv_plane[uv_idx];
            let v = uv_plane[uv_idx + 1];
            let [r, g, b] = yuv420_to_rgb(y, u, v);
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    Some(rgba)
}

#[cfg(test)]
fn read_p010_dmabuf(
    y_plane: &[u8],
    y_stride: usize,
    uv_plane: &[u8],
    uv_stride: usize,
    width: usize,
    height: usize,
    y_inverted: bool,
) -> Option<Vec<u8>> {
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return None;
    }

    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let src_row = if y_inverted { height - 1 - row } else { row };
        let y_row_start = src_row * y_stride;
        let y_row_end = y_row_start + width * 2;
        if y_row_end > y_plane.len() {
            return None;
        }

        let uv_row_start = (src_row / 2) * uv_stride;
        let uv_row_end = uv_row_start + width * 2;
        if uv_row_end > uv_plane.len() {
            return None;
        }

        for col in 0..width {
            let y = (read_le_u16(y_plane, y_row_start + col * 2)? >> 8) as u8;
            let uv_idx = uv_row_start + (col / 2) * 4;
            let u = (read_le_u16(uv_plane, uv_idx)? >> 8) as u8;
            let v = (read_le_u16(uv_plane, uv_idx + 2)? >> 8) as u8;
            let [r, g, b] = yuv420_to_rgb(y, u, v);
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }

    Some(rgba)
}

/// Copy NV12 planes into a contiguous buffer without colorspace conversion.
/// Returns (data, y_stride, uv_stride) where data = Y rows ++ UV rows.
fn read_nv12_dmabuf_passthrough(
    y_plane: &[u8],
    y_stride: usize,
    uv_plane: &[u8],
    uv_stride: usize,
    width: usize,
    height: usize,
    y_inverted: bool,
) -> Option<(Vec<u8>, usize, usize)> {
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return None;
    }

    let uv_height = height / 2;
    // Use width as the output stride (pack rows tightly).
    let out_y_stride = width;
    // UV plane has interleaved U,V pairs: width bytes per row.
    let out_uv_stride = width;
    let mut data = vec![0u8; out_y_stride * height + out_uv_stride * uv_height];

    // Copy Y plane
    for row in 0..height {
        let src_row = if y_inverted { height - 1 - row } else { row };
        let src_start = src_row * y_stride;
        let src_end = src_start + width;
        if src_end > y_plane.len() {
            return None;
        }
        let dst_start = row * out_y_stride;
        data[dst_start..dst_start + width].copy_from_slice(&y_plane[src_start..src_end]);
    }

    // Copy UV plane
    let uv_dst_offset = out_y_stride * height;
    for row in 0..uv_height {
        let src_row = if y_inverted { uv_height - 1 - row } else { row };
        let src_start = src_row * uv_stride;
        let src_end = src_start + width; // width bytes of interleaved UV
        if src_end > uv_plane.len() {
            return None;
        }
        let dst_start = uv_dst_offset + row * out_uv_stride;
        data[dst_start..dst_start + width].copy_from_slice(&uv_plane[src_start..src_end]);
    }

    Some((data, out_y_stride, out_uv_stride))
}

/// Convert P010 (10-bit) DMA-BUF to 8-bit NV12 without going through RGBA.
/// Returns (data, y_stride, uv_stride).
fn read_p010_to_nv12(
    y_plane: &[u8],
    y_stride: usize,
    uv_plane: &[u8],
    uv_stride: usize,
    width: usize,
    height: usize,
    y_inverted: bool,
) -> Option<(Vec<u8>, usize, usize)> {
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return None;
    }

    let uv_height = height / 2;
    let out_y_stride = width;
    let out_uv_stride = width;
    let mut data = vec![0u8; out_y_stride * height + out_uv_stride * uv_height];

    // Convert Y plane: P010 stores 16-bit LE values, take high 8 bits
    for row in 0..height {
        let src_row = if y_inverted { height - 1 - row } else { row };
        let dst_start = row * out_y_stride;
        for col in 0..width {
            let src_offset = src_row * y_stride + col * 2;
            let val = read_le_u16(y_plane, src_offset)?;
            data[dst_start + col] = (val >> 8) as u8;
        }
    }

    // Convert UV plane: P010 stores 16-bit LE U,V pairs
    let uv_dst_offset = out_y_stride * height;
    for row in 0..uv_height {
        let src_row = if y_inverted { uv_height - 1 - row } else { row };
        let dst_start = uv_dst_offset + row * out_uv_stride;
        for col in 0..width / 2 {
            let src_offset = src_row * uv_stride + col * 4;
            let u = (read_le_u16(uv_plane, src_offset)? >> 8) as u8;
            let v = (read_le_u16(uv_plane, src_offset + 2)? >> 8) as u8;
            data[dst_start + col * 2] = u;
            data[dst_start + col * 2 + 1] = v;
        }
    }

    Some((data, out_y_stride, out_uv_stride))
}

/// Convert Smithay Fourcc (DrmFourcc enum) to raw DRM fourcc u32.
fn fourcc_to_drm(code: Fourcc) -> u32 {
    code as u32
}

fn read_dmabuf_pixels(dmabuf: &Dmabuf) -> Option<(u32, u32, PixelData)> {
    let size = dmabuf.size();
    let width = size.w as u32;
    let height = size.h as u32;
    if width == 0 || height == 0 {
        return None;
    }

    let format = dmabuf.format();

    // --- Path selection ---
    //
    // For packed BGRA/RGBA formats (ARGB8888, XRGB8888, ABGR8888, XBGR8888)
    // we take the zero-copy path: dup the DMA-BUF fd and pass it through as
    // PixelData::DmaBuf.  The encoder imports it directly into a VASurface via
    // VA-API VPP (BGRA→NV12 on the GPU) without any CPU mmap.  This is safe
    // because the buffer release happens after we've already submitted the fd
    // to the encoder's VPP pipeline — the kernel DMA-BUF object stays alive
    // as long as the fd is open.
    //
    // For NV12/P010 (hardware video decoder output) we CPU-mmap immediately
    // in the commit handler, before releasing the buffer, because these
    // typically come from the VA-API decoder's own surfaces and are already
    // CPU-accessible (system RAM).  The CPU path avoids a redundant GPU round-trip.
    //
    // Non-Linear modifiers are fine for the GPU zero-copy path (VA-API imports
    // by DRM fd regardless of modifier).  For the CPU path we require Linear.

    let y_inverted = dmabuf.y_inverted();

    // For packed BGRA/RGBA DMA-BUFs, choose between two paths:
    //
    // 1. Zero-copy GPU path (PixelData::DmaBuf): dup the fd and let the
    //    encoder import it into VA-API via PRIME_2.  Only safe for DRM
    //    device-backed fds (from GBM/KMS).  The encoder detects anonymous fds
    //    and rejects them.
    //
    // 2. Immediate CPU mmap (PixelData::Bgra): mmap NOW, before wl_buffer
    //    release, so we capture the frame before mpv overwrites it.  Required
    //    for anonymous /dmabuf heap fds (Vulkan wsi) where the content is
    //    CPU-accessible but may be reused immediately after release.
    //
    // Zero-copy GPU path: dup the DMA-BUF fd and pass it through.
    // The server imports it via VA-API VPP (BGRA→NV12 on the GPU)
    // without any CPU mmap.  The dup'd fd keeps the kernel DMA-BUF
    // object alive even after wl_buffer.release().
    //
    // Previously this path did an eager CPU mmap+copy as a fallback,
    // but that consumed 87% of CPU at 60fps (1.9MB memcpy per frame).
    if !y_inverted
        && matches!(
            format.code,
            Fourcc::Argb8888 | Fourcc::Xrgb8888 | Fourcc::Abgr8888 | Fourcc::Xbgr8888
        )
        && let Some(borrowed_fd) = dmabuf.handles().next()
        && let Ok(owned) = borrowed_fd.try_clone_to_owned()
    {
        let stride = dmabuf.strides().next().unwrap_or(width * 4);
        let offset = dmabuf.offsets().next().unwrap_or(0);
        return Some((
            width,
            height,
            PixelData::DmaBuf {
                fd: Arc::new(owned),
                fourcc: fourcc_to_drm(format.code),
                modifier: format.modifier.into(),
                stride,
                offset,
            },
        ));
    }

    // CPU readback path for NV12/P010 (and y_inverted BGRA as fallback).
    // Must happen before wl_buffer.release() since the client may reuse the
    // buffer immediately after release.
    let modifier_is_linear = matches!(format.modifier, Modifier::Linear);
    if !modifier_is_linear {
        return None;
    }

    let width_usize = width as usize;
    let height_usize = height as usize;
    let pixel_data = match format.code {
        Fourcc::Argb8888 | Fourcc::Xrgb8888 | Fourcc::Abgr8888 | Fourcc::Xbgr8888 => {
            // Fallback CPU path for y_inverted buffers.
            let stride = dmabuf.strides().next().unwrap_or(width * 4) as usize;
            with_dmabuf_plane_bytes(dmabuf, 0, |plane_data| {
                read_packed_dmabuf(
                    plane_data,
                    stride,
                    width_usize,
                    height_usize,
                    y_inverted,
                    format.code,
                )
            })?
        }
        Fourcc::Nv12 => {
            // Pass NV12 straight through — no YUV->RGBA conversion.
            let mut strides = dmabuf.strides();
            let y_stride = strides.next().unwrap_or(width) as usize;
            let uv_stride = strides.next().unwrap_or(width) as usize;
            let nv12 = with_dmabuf_plane_bytes(dmabuf, 0, |y_plane_data| {
                with_dmabuf_plane_bytes(dmabuf, 1, |uv_plane_data| {
                    read_nv12_dmabuf_passthrough(
                        y_plane_data,
                        y_stride,
                        uv_plane_data,
                        uv_stride,
                        width_usize,
                        height_usize,
                        y_inverted,
                    )
                })
            })?;
            PixelData::Nv12 {
                data: Arc::new(nv12.0),
                y_stride: nv12.1,
                uv_stride: nv12.2,
            }
        }
        Fourcc::P010 => {
            // P010 requires conversion to 8-bit NV12 since our encoders don't
            // accept 10-bit input.  Convert to NV12 directly (not RGBA).
            let mut strides = dmabuf.strides();
            let y_stride = strides.next().unwrap_or(width * 2) as usize;
            let uv_stride = strides.next().unwrap_or(width * 2) as usize;
            let nv12 = with_dmabuf_plane_bytes(dmabuf, 0, |y_plane| {
                with_dmabuf_plane_bytes(dmabuf, 1, |uv_plane| {
                    read_p010_to_nv12(
                        y_plane,
                        y_stride,
                        uv_plane,
                        uv_stride,
                        width_usize,
                        height_usize,
                        y_inverted,
                    )
                })
            })?;
            PixelData::Nv12 {
                data: Arc::new(nv12.0),
                y_stride: nv12.1,
                uv_stride: nv12.2,
            }
        }
        _ => return None,
    };

    Some((width, height, pixel_data))
}

impl PrimarySelectionHandler for Compositor {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

impl XdgActivationHandler for Compositor {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation_state
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        _token_data: XdgActivationTokenData,
        _surface: WlSurface,
    ) {
    }
}

impl FractionalScaleHandler for Compositor {
    fn new_fractional_scale(&mut self, _surface: WlSurface) {}
}

impl XdgToplevelIconHandler for Compositor {}
impl TabletSeatHandler for Compositor {}

delegate_compositor!(Compositor);
delegate_cursor_shape!(Compositor);
delegate_shm!(Compositor);
delegate_xdg_shell!(Compositor);
delegate_seat!(Compositor);
delegate_data_device!(Compositor);
delegate_primary_selection!(Compositor);
delegate_output!(Compositor);
delegate_dmabuf!(Compositor);
smithay::delegate_drm_syncobj!(Compositor);
delegate_fractional_scale!(Compositor);
delegate_viewporter!(Compositor);
delegate_xdg_activation!(Compositor);
delegate_xdg_decoration!(Compositor);
delegate_xdg_toplevel_icon!(Compositor);
delegate_text_input_manager!(Compositor);

pub struct CompositorHandle {
    pub event_rx: mpsc::Receiver<CompositorEvent>,
    pub command_tx: mpsc::Sender<CompositorCommand>,
    pub socket_name: String,
    pub thread: std::thread::JoinHandle<()>,
    pub shutdown: Arc<AtomicBool>,
    loop_signal: LoopSignal,
}

impl CompositorHandle {
    /// Wake the compositor event loop immediately so it processes
    /// pending commands without waiting for the idle timeout.
    pub fn wake(&self) {
        self.loop_signal.wakeup();
    }
}

pub fn spawn_compositor(
    verbose: bool,
    event_notify: Arc<dyn Fn() + Send + Sync>,
    gpu_device: &str,
) -> CompositorHandle {
    let gpu_device = gpu_device.to_string();
    let (event_tx, event_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let (socket_tx, socket_rx) = mpsc::sync_channel(1);
    let (signal_tx, signal_rx) = mpsc::sync_channel::<LoopSignal>(1);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|p| {
            // Verify the directory is actually writable by the current user
            // before using it. A stale or inaccessible XDG_RUNTIME_DIR (e.g.
            // in containers, after su/sudo, or in CI) causes PermissionDenied
            // when smithay tries to bind the Wayland socket. A probe write is
            // more reliable than inspecting mode bits, which don't account for
            // the effective uid.
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
                    event_notify,
                    shutdown_clone,
                    verbose,
                    gpu_device,
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

    CompositorHandle {
        event_rx,
        command_tx,
        socket_name,
        thread,
        shutdown,
        loop_signal,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_compositor(
    event_tx: mpsc::Sender<CompositorEvent>,
    command_rx: mpsc::Receiver<CompositorCommand>,
    socket_tx: mpsc::SyncSender<String>,
    signal_tx: mpsc::SyncSender<LoopSignal>,
    event_notify: Arc<dyn Fn() + Send + Sync>,
    shutdown: Arc<AtomicBool>,
    verbose: bool,
    gpu_device: String,
) {
    let mut event_loop: EventLoop<Compositor> =
        EventLoop::try_new().expect("failed to create event loop");
    let display: Display<Compositor> = Display::new().expect("failed to create display");
    let dh = display.handle();

    let compositor_state = CompositorState::new::<Compositor>(&dh);
    let xdg_shell_state = XdgShellState::new::<Compositor>(&dh);
    let shm_state = ShmState::new::<Compositor>(&dh, vec![]);
    let data_device_state = DataDeviceState::new::<Compositor>(&dh);
    let viewporter_state = ViewporterState::new::<Compositor>(&dh);
    let xdg_decoration_state = XdgDecorationState::new::<Compositor>(&dh);
    let primary_selection_state = PrimarySelectionState::new::<Compositor>(&dh);
    let activation_state = XdgActivationState::new::<Compositor>(&dh);
    FractionalScaleManagerState::new::<Compositor>(&dh);
    CursorShapeManagerState::new::<Compositor>(&dh);
    // Disabled: smithay 0.7 has a bug in ShmBufferUserData::remove_destruction_hook
    // (uses != instead of ==) that causes a protocol error when clients destroy icon
    // buffers, killing Chromium-based browsers.
    TextInputManagerState::new::<Compositor>(&dh);

    let mut dmabuf_state = DmabufState::new();
    // DMA-BUF format advertisement.
    //
    // Advertise packed BGRA/RGBA and YUV formats with both Linear and Invalid
    // (implicit) modifiers.  Modifier::Invalid tells clients they may use any
    // modifier (driver-chosen), which is how Vulkan/EGL allocates on AMD/radv.
    // The VA-API VPP zero-copy path imports buffers by DRM fd regardless of
    // modifier, so tiled GPU allocations work fine.
    // NV12/P010 use the CPU readback path and must be Linear.
    // Only advertise Linear modifier.  This forces clients to allocate
    // CPU-mappable buffers, ensuring the mmap readback path and VA-API
    // PRIME import both work.  Clients still render on the GPU — they
    // just use a linear memory layout instead of GPU-tiled.
    // Advertise packed BGRA/RGBA and YUV formats with both Linear and
    // Invalid (implicit) modifiers.  Invalid tells clients they may use
    // any modifier (driver-chosen tiling), which is how GPU compositors
    // like Chromium allocate surfaces.  The VA-API VPP path imports
    // buffers by DRM fd regardless of modifier.
    let dmabuf_formats = {
        use Fourcc::*;
        let packed = [Argb8888, Xrgb8888, Abgr8888, Xbgr8888];
        let yuv = [Fourcc::Nv12, Fourcc::P010];
        let mut fmts = Vec::new();
        for code in packed {
            fmts.push(DmabufFormat {
                code,
                modifier: Modifier::Linear,
            });
            fmts.push(DmabufFormat {
                code,
                modifier: Modifier::Invalid,
            });
        }
        for code in yuv {
            fmts.push(DmabufFormat {
                code,
                modifier: Modifier::Linear,
            });
        }
        fmts
    };
    // Build DMA-BUF feedback with the render node's dev_t so clients
    // allocate GEM-backed DMA-BUFs on the right GPU (instead of anonymous
    // /dmabuf: heap fds that EGL can't import).
    let dmabuf_global = match std::fs::metadata(&gpu_device).ok().and_then(|m| {
        use std::os::unix::fs::MetadataExt;
        let dev = m.rdev();
        let formats = dmabuf_formats.clone();
        let feedback = DmabufFeedbackBuilder::new(dev, formats).build().ok()?;
        Some(dmabuf_state.create_global_with_default_feedback::<Compositor>(&dh, &feedback))
    }) {
        Some(global) => {
            if verbose {
                eprintln!("[compositor] DMA-BUF feedback enabled ({gpu_device})");
            }
            global
        }
        None => {
            if verbose {
                eprintln!("[compositor] DMA-BUF feedback unavailable, using basic global");
            }
            dmabuf_state.create_global::<Compositor>(&dh, dmabuf_formats)
        }
    };

    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "headless");
    let keymap = include_str!("../data/us-qwerty.xkb").to_string();
    seat.add_keyboard_from_keymap_string(keymap, 200, 25)
        .expect("failed to add keyboard from embedded keymap");
    seat.add_pointer();

    let output = Output::new(
        "headless-0".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Virtual".into(),
            model: "Headless".into(),
        },
    );
    let mode = Mode {
        size: (1920, 1080).into(),
        refresh: 60_000,
    };
    output.create_global::<Compositor>(&dh);
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(smithay::output::Scale::Integer(1)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    let mut space = Space::default();
    space.map_output(&output, (0, 0));

    let listening_socket = ListeningSocketSource::new_auto().unwrap_or_else(|e| {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "(unset)".into());
        panic!(
            "failed to create wayland socket in XDG_RUNTIME_DIR={dir}: {e}\n\
             hint: ensure the directory exists and is writable by the current user"
        );
    });
    let socket_name = listening_socket
        .socket_name()
        .to_string_lossy()
        .into_owned();
    socket_tx.send(socket_name).unwrap();

    let handle = event_loop.handle();

    handle
        .insert_source(listening_socket, |client_stream, _, state| {
            if let Err(e) = state.display_handle.insert_client(
                client_stream,
                Arc::new(ClientData {
                    compositor_state: CompositorClientState::default(),
                }),
            ) && verbose
            {
                eprintln!("[compositor] insert_client error: {e}");
            }
        })
        .expect("failed to insert listening socket");

    let loop_signal = event_loop.get_signal();

    let mut compositor = Compositor {
        display_handle: dh.clone(),
        compositor_state,
        xdg_shell_state,
        shm_state,
        seat_state,
        data_device_state,
        viewporter_state,
        xdg_decoration_state,
        dmabuf_state,
        dmabuf_global,
        syncobj_state: {
            // Enable explicit sync (linux-drm-syncobj-v1) if the GPU supports it.
            // This replaces implicit DMA-BUF fencing which hangs on AMD.

            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&gpu_device)
                .ok()
                .and_then(|f| {
                    use std::os::fd::OwnedFd;
                    let owned: OwnedFd = f.into();
                    let dev_fd = smithay::utils::DeviceFd::from(owned);
                    let drm_fd = DrmDeviceFd::new(dev_fd);
                    if supports_syncobj_eventfd(&drm_fd) {
                        let state = DrmSyncobjState::new::<Compositor>(&dh, drm_fd);
                        eprintln!("[compositor] explicit sync (drm-syncobj) enabled");
                        Some(state)
                    } else {
                        eprintln!("[compositor] explicit sync not supported by GPU");
                        None
                    }
                })
        },
        primary_selection_state,
        activation_state,
        seat,
        output,
        space,
        surfaces: HashMap::new(),
        surface_lookup: HashMap::new(),
        next_surface_id: 1,
        event_tx,
        event_notify,
        loop_signal: loop_signal.clone(),
        verbose,
        focused_surface_id: 0,
        pending_commits: HashMap::new(),
        surface_pixel_cache: HashMap::new(),
        gpu_renderer: super::render::SurfaceRenderer::try_new(&gpu_device),
    };

    // Send the loop signal back so the server can wake us.
    let _ = signal_tx.send(loop_signal.clone());

    let display_source = Generic::new(display, Interest::READ, calloop::Mode::Level);
    handle
        .insert_source(display_source, |_, display, state| {
            let d = unsafe { display.get_mut() };
            if let Err(e) = d.dispatch_clients(state)
                && verbose
            {
                eprintln!("[compositor] dispatch_clients error: {e}");
            }
            if let Err(e) = d.flush_clients()
                && verbose
            {
                eprintln!("[compositor] flush_clients error: {e}");
            }
            Ok(PostAction::Continue)
        })
        .expect("failed to insert display source");

    if verbose {
        eprintln!("[compositor] entering event loop");
    }
    while !shutdown.load(Ordering::Relaxed) {
        while let Ok(cmd) = command_rx.try_recv() {
            match cmd {
                CompositorCommand::Shutdown => {
                    shutdown.store(true, Ordering::Relaxed);
                    return;
                }
                other => compositor.handle_command(other),
            }
        }

        // No rate limit — the loop wakes instantly on Wayland client
        // traffic (fd readable) or server commands (loop_signal.wakeup()).
        // The 1s ceiling is only a liveness fallback for shutdown polling.
        if let Err(e) =
            event_loop.dispatch(Some(std::time::Duration::from_secs(1)), &mut compositor)
            && verbose
        {
            eprintln!("[compositor] event loop error: {e}");
        }

        // Flush coalesced commits — only the latest per surface is sent.
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

#[cfg(test)]
mod tests {
    use super::{Fourcc, read_nv12_dmabuf, read_p010_dmabuf, read_packed_dmabuf};

    /// Test-only wrapper that returns RGBA like the old `read_packed_rgba_dmabuf`.
    fn read_packed_rgba_dmabuf(
        plane_data: &[u8],
        stride: usize,
        width: usize,
        height: usize,
        y_inverted: bool,
        format: Fourcc,
    ) -> Option<Vec<u8>> {
        let pd = read_packed_dmabuf(plane_data, stride, width, height, y_inverted, format)?;
        Some(pd.to_rgba(width as u32, height as u32))
    }

    #[test]
    fn xrgb_dmabuf_forces_opaque_alpha() {
        let pixels = [
            0x10, 0x20, 0x30, 0x00, //
            0x40, 0x50, 0x60, 0x7f,
        ];

        let rgba = read_packed_rgba_dmabuf(&pixels, 8, 2, 1, false, Fourcc::Xrgb8888).unwrap();

        assert_eq!(rgba, vec![0x30, 0x20, 0x10, 0xff, 0x60, 0x50, 0x40, 0xff]);
    }

    #[test]
    fn nv12_black_decodes_to_opaque_black() {
        let y_plane = [16, 16, 16, 16];
        let uv_plane = [128, 128];

        let rgba = read_nv12_dmabuf(&y_plane, 2, &uv_plane, 2, 2, 2, false).unwrap();

        assert_eq!(rgba, [0, 0, 0, 255].repeat(4));
    }

    #[test]
    fn p010_white_decodes_to_opaque_white() {
        let y_plane = [0x00, 0xeb, 0x00, 0xeb, 0x00, 0xeb, 0x00, 0xeb];
        let uv_plane = [0x00, 0x80, 0x00, 0x80];

        let rgba = read_p010_dmabuf(&y_plane, 4, &uv_plane, 4, 2, 2, false).unwrap();

        assert_eq!(rgba, [255, 255, 255, 255].repeat(4));
    }
}
