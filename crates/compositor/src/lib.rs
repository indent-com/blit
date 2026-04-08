#[cfg(target_os = "linux")]
mod imp;
#[cfg(target_os = "linux")]
mod render;
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
    }

    #[derive(Clone)]
    pub struct PixelLayer {
        pub x: i32,
        pub y: i32,
        pub width: u32,
        pub height: u32,
        pub pixels: PixelData,
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
                PixelData::Nv12 { .. } | PixelData::DmaBuf { .. } => Vec::new(),
            }
        }

        pub fn is_empty(&self) -> bool {
            match self {
                PixelData::Bgra(v) | PixelData::Rgba(v) => v.is_empty(),
                PixelData::Nv12 { data, .. } => data.is_empty(),
                PixelData::DmaBuf { .. } => false,
            }
        }

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
        ReleaseKeys {
            keycodes: Vec<u32>,
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
        Shutdown,
    }

    pub struct CompositorHandle {
        pub event_rx: mpsc::Receiver<CompositorEvent>,
        pub command_tx: mpsc::Sender<CompositorCommand>,
        pub socket_name: String,
        pub thread: std::thread::JoinHandle<()>,
        pub shutdown: Arc<AtomicBool>,
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
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub use stub::*;
