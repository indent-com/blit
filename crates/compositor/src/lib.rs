#[cfg(unix)]
mod imp;
#[cfg(unix)]
pub use imp::*;

#[cfg(not(unix))]
mod stub {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    pub enum CompositorEvent {}

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
