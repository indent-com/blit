// Compiled everywhere: the server derives its announced codec strings from
// this same table on every platform.
pub mod av1_level;
#[cfg(target_os = "linux")]
mod drm_syncobj;
#[cfg(target_os = "linux")]
mod imp;
#[cfg(target_os = "linux")]
mod input_region;
// Compiled everywhere so its tests run on any host; only `imp` consumes it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod pointer_focus;
#[cfg(target_os = "linux")]
mod positioner;
#[cfg(target_os = "linux")]
mod render;
#[cfg(target_os = "linux")]
mod touch_pacer;
#[cfg(target_os = "linux")]
mod vulkan_encode;
#[cfg(target_os = "linux")]
mod vulkan_render;
#[cfg(target_os = "linux")]
pub use imp::*;

#[cfg(not(target_os = "linux"))]
mod stub {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    pub mod drm_fourcc {
        pub const ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
        pub const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
        pub const ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
        pub const XBGR8888: u32 = u32::from_le_bytes(*b"XB24");
        pub const NV12: u32 = u32::from_le_bytes(*b"NV12");
    }

    /// Placeholder for `std::os::fd::OwnedFd` on non-Unix platforms.
    #[derive(Debug)]
    pub struct OwnedFd(());

    #[derive(Clone)]
    pub enum PixelData {
        Bgra(Arc<Vec<u8>>),
        Rgba(Arc<Vec<u8>>),
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
            y_invert: bool,
        },
        Nv12DmaBuf {
            fd: Arc<OwnedFd>,
            stride: u32,
            uv_offset: u32,
            width: u32,
            height: u32,
            sync_fd: Option<Arc<OwnedFd>>,
        },
        Nv12OpaqueFd {
            fd: Arc<OwnedFd>,
            buf_id: u64,
            stride: u32,
            uv_offset: u32,
            width: u32,
            height: u32,
            is_444: bool,
            sync_fd: Option<Arc<OwnedFd>>,
        },
        VaSurface {
            surface_id: u32,
            va_display: usize,
            _fd: Arc<OwnedFd>,
        },
    }

    /// A bitstream a compositor-resident encoder produced for exactly one
    /// client.  Owned per `(surface_id, client_id)`, never shared.
    pub struct EncodedFrame {
        pub surface_id: u16,
        pub client_id: u64,
        pub width: u32,
        pub height: u32,
        pub data: Arc<Vec<u8>>,
        pub is_keyframe: bool,
        pub codec_flag: u8,
    }

    impl PixelData {
        pub fn to_rgba(&self, _width: u32, _height: u32) -> Vec<u8> {
            match self {
                PixelData::Rgba(data) => data.as_ref().clone(),
                PixelData::Bgra(data) => {
                    let mut rgba = Vec::with_capacity(data.len());
                    for px in data.chunks_exact(4) {
                        rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                    }
                    rgba
                }
                _ => Vec::new(),
            }
        }

        pub fn is_empty(&self) -> bool {
            match self {
                PixelData::Bgra(v) | PixelData::Rgba(v) => v.is_empty(),
                PixelData::Nv12 { data, .. } => data.is_empty(),
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
            /// The cursor surface's `buffer_scale`.  `width / scale` is the
            /// cursor's logical size — the same space the hotspot is in.
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
        SurfaceCommit {
            surface_id: u16,
            width: u32,
            height: u32,
            pixels: PixelData,
            timestamp_ms: u32,
            timestamp_sub_us: u16,
            encoder_skip: bool,
        },
        SurfaceEncoded {
            frame: EncodedFrame,
            timestamp_ms: u32,
            timestamp_sub_us: u16,
        },
        VulkanEncoderUnavailable {
            surface_id: u16,
            client_id: u64,
            /// Whether a session was created before encoding failed.  The
            /// server uses the requested profile it tracks for this pair to
            /// retry 4:4:4 refusals at 4:2:0 before changing backends.
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
        /// The stamped identity of a toplevel's application, sent once at
        /// creation for surfaces that arrived on a per-app socket.
        SurfaceOrigin {
            surface_id: u16,
            sandbox_engine: String,
            app_id: String,
            instance_id: String,
        },
        /// The client asked for one of its toplevels to be activated
        /// (xdg_activation_v1) — e.g. an Electron app reacting to a
        /// notification click.  Pane focus belongs to the frontend, so the
        /// request is forwarded, not acted on here.
        SurfaceActivated {
            surface_id: u16,
        },
        SurfaceTextInput {
            surface_id: u16,
            enabled: bool,
            requested: bool,
            hint: u32,
            purpose: u32,
            cursor_rect: Option<(i32, i32, i32, i32)>,
        },
        SurfaceResized {
            surface_id: u16,
            width: u16,
            height: u16,
            logical_width: u16,
            logical_height: u16,
        },
        ClipboardContent {
            mime_type: String,
            data: Vec<u8>,
        },
        ClipboardOwner {
            wayland: bool,
        },
        SurfaceCursor {
            surface_id: u16,
            cursor: CursorImage,
        },
        /// The compositor retired a direct-touch sequence on its own — the
        /// contact's target unmapped, or touch was disabled.  Without this the
        /// server would keep believing `owner_id` holds a live sequence and go
        /// on refusing every other viewer's contacts.
        TouchCancelled {
            owner_id: Option<u64>,
        },
    }

    /// Who a Wayland connection belongs to.
    ///
    /// Stamped by whoever created the socket the client arrived on, never
    /// asserted by the client itself — which is the whole point. `app_id` from
    /// `xdg_toplevel.set_app_id` is a free-form string an application says about
    /// itself, unverified and often wrong; `SO_PEERCRED` gives a pid that a
    /// zygote-forking or re-execing application immediately invalidates, and a
    /// passed connection fd means one socket need not mean one process at all.
    ///
    /// The fields mirror `wp_security_context_v1` so that protocol can be wired
    /// to this later, letting a third-party sandbox engine stamp its own
    /// sockets. Nothing here depends on that protocol existing.
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
        PointerAxis {
            surface_id: u16,
            dx: f64,
            dy: f64,
            v120_x: i16,
            v120_y: i16,
            source: Option<u8>,
            stop: bool,
            time_ms: u32,
        },
        SetTouchEnabled {
            enabled: bool,
        },
        Touch {
            owner_id: u64,
            surface_id: u16,
            phase: TouchPhase,
            /// The originating browser's `TouchEvent.timeStamp` in whole ms, in
            /// its own epoch.  Used only for the spacing between events.
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
        DragEnter {
            surface_id: u16,
            x: f64,
            y: f64,
            mimes: Vec<String>,
            planned_uri_list: Option<Vec<u8>>,
        },
        DragMotion {
            surface_id: u16,
            x: f64,
            y: f64,
        },
        DragLeave,
        DragDrop {
            surface_id: u16,
            x: f64,
            y: f64,
            offers: Vec<(String, Vec<u8>)>,
        },
        DragCancel,
        /// Take ownership of the primary selection on the browser's behalf.
        ///
        /// Unlike the clipboard, whose contents the compositor can fetch
        /// from the owning client on demand, PRIMARY has no web-side API to
        /// read from, so the bytes arrive up front and the compositor
        /// serves them itself.  Displaces any Wayland client that owned it.
        PrimaryOffer {
            mime_type: String,
            data: Vec<u8>,
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
        /// Composed text from the browser (e.g. IME or shifted characters
        /// that don't match the compositor's US-QWERTY keymap).  The compositor
        /// synthesises evdev key sequences for ASCII chars and uses
        /// zwp_text_input_v3 commit_string for non-ASCII.
        TextInput {
            text: String,
        },
        /// Text the user is still composing, for the app to show inline until
        /// it is committed or withdrawn.  Delivered via `zwp_text_input_v3`
        /// preedit_string; `cursor` is a byte offset into `text`, and an
        /// empty `text` withdraws the composition.
        Preedit {
            text: String,
            cursor: u16,
        },
        ReleaseKeys {
            keycodes: Vec<u32>,
        },
        Capture {
            surface_id: u16,
            /// Render scale in 120ths. 0 = current output scale.
            scale_120: u16,
            reply: mpsc::SyncSender<Option<(u32, u32, Vec<u8>)>>,
        },
        /// Fire pending wl_surface.frame callbacks for a surface so the
        /// client will paint and commit its next frame.  Send this when
        /// the server is ready to consume a new frame (streaming or capture).
        RequestFrame {
            surface_id: u16,
            presentation_at: std::time::Instant,
        },
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
        SetExternalOutputBuffers {
            surface_id: u32,
            target_w: u32,
            target_h: u32,
            native_w: u32,
            native_h: u32,
            buffers: Vec<ExternalOutputBuffer>,
        },
        RegisterDownscaleTarget {
            surface_id: u32,
            target_w: u32,
            target_h: u32,
            native_w: u32,
            native_h: u32,
            want_nv12_opaque: bool,
            want_cpu_pixels: bool,
            opaque_is_444: bool,
        },
        RestampTarget {
            surface_id: u32,
            target_w: u32,
            target_h: u32,
            native_w: u32,
            native_h: u32,
        },
        ClearDownscaleTarget {
            surface_id: u32,
            target_w: u32,
            target_h: u32,
        },
        SetXwaylandPid {
            pid: u32,
        },
        /// Adopt an already-bound listening socket whose clients are known to
        /// belong to `identity`.
        ///
        /// The caller binds the socket so the app can be spawned the instant
        /// this is sent — there is no window in which the socket is named but
        /// not yet listening. Ownership of the path stays with the caller,
        /// which unlinks it; the compositor only accepts on the fd.
        AddAppSocket {
            fd: OwnedFd,
            identity: AppIdentity,
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
        pub planes: Vec<ExternalOutputPlane>,
        pub nv12_fd: Option<Arc<OwnedFd>>,
        pub nv12_stride: u32,
        pub nv12_uv_offset: u32,
        pub nv12_modifier: u64,
        pub nv12_width: u32,
        pub nv12_height: u32,
    }

    pub struct CompositorHandle {
        pub event_rx: mpsc::Receiver<CompositorEvent>,
        pub command_tx: mpsc::Sender<CompositorCommand>,
        pub socket_name: String,
        /// Whether the compositor's Vulkan renderer supports Vulkan Video encode.
        pub vulkan_video_encode: bool,
        /// Whether the compositor's Vulkan renderer supports Vulkan Video AV1 encode.
        pub vulkan_video_encode_av1: bool,
        foreign_exports: Arc<std::sync::RwLock<std::collections::HashMap<String, u16>>>,
        thread: std::thread::JoinHandle<()>,
        #[allow(dead_code)]
        shutdown: Arc<AtomicBool>,
    }

    impl CompositorHandle {
        /// Wake the compositor event loop immediately.
        pub fn wake(&self) {}

        pub fn set_frame_interval(&self, _surface_id: u16, _interval: Option<std::time::Duration>) {
        }

        pub fn take_frame_clock_requests(&self) -> u32 {
            0
        }

        pub fn resolve_foreign_parent(&self, parent: &str) -> Option<u16> {
            let handle = parent.strip_prefix("wayland:")?;
            self.foreign_exports.read().ok()?.get(handle).copied()
        }

        /// Stop the compositor and wait for it to finish tearing down.
        pub fn stop(self) {
            let _ = self.thread.join();
        }
    }

    pub fn spawn_compositor(
        _verbose: bool,
        _event_notify: Arc<dyn Fn() + Send + Sync>,
        _gpu_device: &str,
    ) -> CompositorHandle {
        let (event_tx, event_rx) = mpsc::channel();
        let (command_tx, _command_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        // Drop the sender immediately so event_rx.recv() returns Err.
        drop(event_tx);
        CompositorHandle {
            event_rx,
            command_tx,
            socket_name: String::new(),
            thread: std::thread::spawn(|| {}),
            shutdown,
            vulkan_video_encode: false,
            vulkan_video_encode_av1: false,
            foreign_exports: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }

    #[doc(hidden)]
    pub fn spawn_compositor_without_renderer(
        verbose: bool,
        event_notify: Arc<dyn Fn() + Send + Sync>,
    ) -> CompositorHandle {
        spawn_compositor(verbose, event_notify, "")
    }
}

#[cfg(not(target_os = "linux"))]
pub use stub::*;
