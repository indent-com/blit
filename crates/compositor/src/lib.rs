#[cfg(target_os = "linux")]
mod imp;
#[cfg(target_os = "linux")]
mod positioner;
#[cfg(target_os = "linux")]
mod render;
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
        },
        Nv12DmaBuf {
            fd: Arc<OwnedFd>,
            stride: u32,
            uv_offset: u32,
            width: u32,
            height: u32,
            sync_fd: Option<Arc<OwnedFd>>,
        },
        VaSurface {
            surface_id: u32,
            va_display: usize,
            _fd: Arc<OwnedFd>,
        },
        Encoded {
            data: Arc<Vec<u8>>,
            is_keyframe: bool,
            codec_flag: u8,
        },
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
                | PixelData::Nv12DmaBuf { .. } => false,
                PixelData::Encoded { data, .. } => data.is_empty(),
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
            surface_id: u16,
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
        },
        SetExternalOutputBuffers {
            surface_id: u32,
            buffers: Vec<ExternalOutputBuffer>,
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
    }

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
    }

    impl CompositorHandle {
        /// Wake the compositor event loop immediately.
        pub fn wake(&self) {}
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
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub use stub::*;
