#[cfg(unix)]
mod imp;
#[cfg(unix)]
pub use imp::*;

#[cfg(not(unix))]
mod stub {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

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
            pixels: Vec<u8>,
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
        },
        SurfaceFocus {
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
        Shutdown,
    }

    pub struct CompositorHandle {
        pub event_rx: mpsc::Receiver<CompositorEvent>,
        pub command_tx: mpsc::Sender<CompositorCommand>,
        pub socket_name: String,
        pub thread: std::thread::JoinHandle<()>,
        pub shutdown: Arc<AtomicBool>,
    }

    pub fn spawn_compositor() -> CompositorHandle {
        unimplemented!("compositor is only supported on Unix")
    }
}

#[cfg(not(unix))]
pub use stub::*;
