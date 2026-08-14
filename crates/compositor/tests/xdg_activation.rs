//! xdg_activation_v1 lets a client ask for its own window to come forward --
//! an Electron app reacting to a notification click is the canonical case.
//!
//! Pane arrangement belongs to the frontend, not the compositor, so the
//! compositor cannot honour the request itself.  What it must not do is drop
//! it on the floor: the request goes out as `SurfaceActivated` so the server
//! can tell every viewer to raise and focus the pane, which is how the
//! frontend answers with C2S_SURFACE_FOCUS.

#![cfg(target_os = "linux")]

use std::os::unix::net::UnixStream;
use std::sync::Arc;

use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};

use blit_compositor::{CompositorEvent, CompositorHandle, spawn_compositor_without_renderer};

use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    activation: Option<xdg_activation_v1::XdgActivationV1>,
    /// The token string the compositor answered our commit with.
    token: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name, interface, ..
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => {
                state.compositor =
                    Some(registry.bind::<wl_compositor::WlCompositor, _, _>(name, 4, qh, ()));
            }
            "xdg_wm_base" => {
                state.wm_base =
                    Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(name, 1, qh, ()));
            }
            "xdg_activation_v1" => {
                state.activation = Some(registry.bind::<xdg_activation_v1::XdgActivationV1, _, _>(
                    name,
                    1,
                    qh,
                    (),
                ));
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &xdg_activation_token_v1::XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.token = Some(token);
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore xdg_wm_base::XdgWmBase);
delegate_noop!(App: ignore xdg_surface::XdgSurface);
delegate_noop!(App: ignore xdg_toplevel::XdgToplevel);
delegate_noop!(App: ignore xdg_activation_v1::XdgActivationV1);

struct Fixture {
    app: App,
    queue: EventQueue<App>,
    surface: wl_surface::WlSurface,
    handle: Option<CompositorHandle>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.stop();
        }
    }
}

impl Fixture {
    fn new() -> Self {
        let handle = spawn_compositor_without_renderer(false, Arc::new(|| {}));
        let stream =
            UnixStream::connect(&handle.socket_name).expect("connect to compositor socket");
        let conn = Connection::from_socket(stream).expect("wayland connection");
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());

        let mut app = App::default();
        queue.roundtrip(&mut app).expect("registry roundtrip");
        let compositor = app.compositor.clone().expect("wl_compositor advertised");
        let wm_base = app.wm_base.clone().expect("xdg_wm_base advertised");
        assert!(
            app.activation.is_some(),
            "xdg_activation_v1 must be advertised"
        );

        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        queue.roundtrip(&mut app).expect("map roundtrip");

        Self {
            app,
            queue,
            surface,
            handle: Some(handle),
        }
    }

    /// Drain compositor events until one matching `want` arrives.
    fn wait_event(&mut self, mut want: impl FnMut(&CompositorEvent) -> bool, what: &str) {
        let handle = self.handle.as_ref().expect("compositor running");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match handle
                .event_rx
                .recv_timeout(std::time::Duration::from_millis(200))
            {
                Ok(event) if want(&event) => return,
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        panic!("compositor never sent {what}");
    }
}

#[test]
fn activate_request_is_forwarded_as_an_event() {
    let mut fx = Fixture::new();

    let mut created_id = None;
    fx.wait_event(
        |e| match e {
            CompositorEvent::SurfaceCreated { surface_id, .. } => {
                created_id = Some(*surface_id);
                true
            }
            _ => false,
        },
        "SurfaceCreated",
    );
    let created_id = created_id.expect("captured by the matcher");

    let activation = fx.app.activation.clone().expect("activation global");
    let token = activation.get_activation_token(&fx.queue.handle(), ());
    token.commit();
    fx.queue.roundtrip(&mut fx.app).expect("token roundtrip");
    let token = fx.app.token.clone().expect("compositor issued a token");

    activation.activate(token, &fx.surface);
    fx.queue.roundtrip(&mut fx.app).expect("activate roundtrip");

    fx.wait_event(
        |e| matches!(e, CompositorEvent::SurfaceActivated { surface_id } if *surface_id == created_id),
        "SurfaceActivated for our toplevel",
    );
}
