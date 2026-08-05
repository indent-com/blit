//! A toplevel that asks to change state must get an answer.
//!
//! Panes are permanently activated and maximized, so every one of these
//! requests is declined -- but silence is not a way to decline.  xdg-shell
//! requires a configure in reply to the maximize/fullscreen pair, and
//! Chromium-based clients (every Electron app) flip themselves to minimized
//! the instant they send set_minimized, then stop drawing until a configure
//! carrying `activated` tells them otherwise.

#![cfg(target_os = "linux")]

use std::os::unix::net::UnixStream;
use std::sync::Arc;

use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};

use blit_compositor::{CompositorHandle, spawn_compositor};

// xdg_shell is not among wayland-client's bundled protocols, so talk to it
// through the generated bindings the compositor crate already pulls in.
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    /// Every configure the server sent, as (width, height, states).
    configures: Vec<(i32, i32, Vec<u32>)>,
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
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        _: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Configure {
            width,
            height,
            states,
        } = event
        {
            let states = states
                .chunks_exact(4)
                .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            state.configures.push((width, height, states));
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);

/// A real client on a real compositor, with one mapped toplevel.
struct Fixture {
    app: App,
    queue: EventQueue<App>,
    toplevel: xdg_toplevel::XdgToplevel,
    // Dropping any of these would tear down the objects under test.
    _surface: wl_surface::WlSurface,
    _xdg_surface: xdg_surface::XdgSurface,
    _conn: Connection,
    // Leave the compositor thread running and let process exit reap it, the
    // way the server does.  Tripping `handle.shutdown` instead unwinds
    // `run_compositor` and segfaults in renderer teardown -- a path nothing
    // in the tree actually takes.
    _handle: CompositorHandle,
}

impl Fixture {
    fn new() -> Self {
        let handle = spawn_compositor(false, Arc::new(|| {}), "");

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

        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        queue.roundtrip(&mut app).expect("map roundtrip");

        let fixture = Self {
            app,
            queue,
            toplevel,
            _surface: surface,
            _xdg_surface: xdg_surface,
            _conn: conn,
            _handle: handle,
        };
        assert!(
            fixture.states_since(0).contains(&State::Activated),
            "expected an initial activated configure, got {:?}",
            fixture.app.configures
        );
        fixture
    }

    /// How many configures have arrived so far.
    fn mark(&self) -> usize {
        self.app.configures.len()
    }

    /// States carried by every configure that arrived after `mark`.
    fn states_since(&self, mark: usize) -> Vec<State> {
        self.app.configures[mark..]
            .iter()
            .flat_map(|(_, _, states)| states.iter().copied())
            .filter_map(|s| match s {
                1 => Some(State::Maximized),
                2 => Some(State::Fullscreen),
                3 => Some(State::Resizing),
                4 => Some(State::Activated),
                _ => None,
            })
            .collect()
    }

    /// Send whatever `f` asks for, then let the server answer.
    fn request(&mut self, f: impl FnOnce(&xdg_toplevel::XdgToplevel)) -> usize {
        let mark = self.mark();
        f(&self.toplevel);
        self.queue.roundtrip(&mut self.app).expect("roundtrip");
        mark
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum State {
    Maximized,
    Fullscreen,
    Resizing,
    Activated,
}

#[test]
fn set_minimized_is_declined_with_an_activated_configure() {
    let mut fixture = Fixture::new();
    let mark = fixture.request(|tl| tl.set_minimized());

    // Without `activated` in a configure, a Chromium client stays parked in
    // the minimized state it assigned itself and never paints again.
    assert!(
        fixture.states_since(mark).contains(&State::Activated),
        "set_minimized drew no activated configure; the client is left \
         believing it is minimized. configures: {:?}",
        fixture.app.configures
    );
}

#[test]
fn maximize_and_fullscreen_requests_are_always_answered() {
    let mut fixture = Fixture::new();

    // xdg-shell: each of these "will respond by emitting a configure event".
    type Send = fn(&xdg_toplevel::XdgToplevel);
    let requests: [(&str, Send); 4] = [
        ("set_maximized", |tl| tl.set_maximized()),
        ("unset_maximized", |tl| tl.unset_maximized()),
        ("set_fullscreen", |tl| tl.set_fullscreen(None)),
        ("unset_fullscreen", |tl| tl.unset_fullscreen()),
    ];
    for (name, send) in requests {
        let mark = fixture.request(send);
        let states = fixture.states_since(mark);
        assert!(
            !states.is_empty(),
            "{name} drew no configure at all; the client waits forever"
        );
        // The pane never actually changes shape, so the answer is always the
        // same: still activated, still maximized, never fullscreen.
        assert!(
            states.contains(&State::Activated) && states.contains(&State::Maximized),
            "{name} answered with {states:?}, expected activated + maximized"
        );
        assert!(
            !states.contains(&State::Fullscreen),
            "{name} answered with fullscreen, which panes never enter"
        );
    }
}
