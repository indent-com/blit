//! `set_min_size` / `set_max_size` say what the client can actually draw.
//!
//! We cannot resize a pane to suit a client -- the pane is the viewer's
//! layout -- but we can stop quoting a size it is going to refuse, and we owe
//! it the protocol errors xdg-shell specifies for a nonsense pair.  Both
//! halves are double-buffered, so the pair is judged at commit, not as each
//! arrives.

#![cfg(target_os = "linux")]

use std::os::unix::net::UnixStream;
use std::sync::Arc;

use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};

use blit_compositor::{CompositorHandle, spawn_compositor};

use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// xdg_toplevel.error.invalid_size
const INVALID_SIZE: u32 = 2;

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    /// Every configure the server sent, as (width, height).
    configures: Vec<(i32, i32)>,
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
        if let xdg_toplevel::Event::Configure { width, height, .. } = event {
            state.configures.push((width, height));
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);

struct Fixture {
    app: App,
    queue: EventQueue<App>,
    conn: Connection,
    surface: wl_surface::WlSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    _xdg_surface: xdg_surface::XdgSurface,
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

        Self {
            app,
            queue,
            conn,
            surface,
            toplevel,
            _xdg_surface: xdg_surface,
            handle: Some(handle),
        }
    }

    /// Commit, then let the server react.  Returns the protocol error code if
    /// the connection was killed instead.
    fn commit(&mut self) -> Option<u32> {
        self.surface.commit();
        self.round()
    }

    fn round(&mut self) -> Option<u32> {
        match self.queue.roundtrip(&mut self.app) {
            Ok(_) => None,
            Err(_) => self.conn.protocol_error().map(|e| e.code),
        }
    }

    /// The size in the most recent configure, provoking one first.
    fn latest_configured_size(&mut self) -> (i32, i32) {
        // set_maximized is declined with a restatement of the current
        // configure, which is a cheap way to ask "what size do you think I am?"
        self.toplevel.set_maximized();
        assert_eq!(self.round(), None, "unexpected protocol error");
        *self
            .app
            .configures
            .last()
            .expect("the server always answers set_maximized with a configure")
    }
}

#[test]
fn a_maximum_below_the_pane_is_what_we_ask_for() {
    let mut fixture = Fixture::new();
    let (pane_w, pane_h) = fixture.latest_configured_size();
    assert!(
        pane_w > 800 && pane_h > 600,
        "test assumes a pane larger than the cap it is about to set, got {pane_w}x{pane_h}"
    );

    fixture.toplevel.set_max_size(800, 600);
    assert_eq!(fixture.commit(), None, "a plain maximum is not an error");

    assert_eq!(
        fixture.latest_configured_size(),
        (800, 600),
        "we kept asking for the full pane after the client said it caps out \
         lower; it will refuse and render small anyway"
    );
}

#[test]
fn a_minimum_above_the_pane_is_what_we_ask_for() {
    let mut fixture = Fixture::new();
    fixture.toplevel.set_min_size(3000, 2000);
    assert_eq!(fixture.commit(), None, "a plain minimum is not an error");

    assert_eq!(
        fixture.latest_configured_size(),
        (3000, 2000),
        "the client cannot draw itself smaller than this, so asking for less \
         only produces a surface neither side agrees on"
    );
}

#[test]
fn hints_only_take_effect_on_commit() {
    let mut fixture = Fixture::new();
    let before = fixture.latest_configured_size();

    fixture.toplevel.set_max_size(640, 480);
    assert_eq!(fixture.round(), None, "unexpected protocol error");

    assert_eq!(
        fixture.latest_configured_size(),
        before,
        "the maximum was applied without a commit; xdg-shell double-buffers it"
    );
}

#[test]
fn a_minimum_raised_past_the_old_maximum_in_one_commit_is_fine() {
    let mut fixture = Fixture::new();
    fixture.toplevel.set_max_size(800, 600);
    assert_eq!(fixture.commit(), None, "a plain maximum is not an error");

    // Both halves move together.  Judged as they arrive, the min would briefly
    // exceed the still-committed max of 800x600 and the client would be killed
    // for a sequence the protocol allows.
    fixture.toplevel.set_min_size(1000, 800);
    fixture.toplevel.set_max_size(1200, 900);
    assert_eq!(
        fixture.commit(),
        None,
        "killed a client for raising its minimum and maximum in one commit"
    );

    assert_eq!(fixture.latest_configured_size(), (1200, 900));
}

#[test]
fn a_minimum_above_the_maximum_is_refused() {
    let mut fixture = Fixture::new();
    fixture.toplevel.set_min_size(1000, 1000);
    fixture.toplevel.set_max_size(800, 800);
    assert_eq!(
        fixture.commit(),
        Some(INVALID_SIZE),
        "a minimum above the maximum is an invalid_size error"
    );
}

#[test]
fn a_negative_hint_is_refused() {
    let mut fixture = Fixture::new();
    fixture.toplevel.set_max_size(-1, 600);
    assert_eq!(
        fixture.round(),
        Some(INVALID_SIZE),
        "a negative maximum is an invalid_size error"
    );
}

#[test]
fn a_negative_minimum_is_refused() {
    let mut fixture = Fixture::new();
    fixture.toplevel.set_min_size(100, -5);
    assert_eq!(
        fixture.round(),
        Some(INVALID_SIZE),
        "a negative minimum is an invalid_size error"
    );
}

#[test]
fn zero_means_no_opinion() {
    let mut fixture = Fixture::new();
    let pane = fixture.latest_configured_size();

    fixture.toplevel.set_max_size(800, 600);
    assert_eq!(fixture.commit(), None);
    assert_eq!(fixture.latest_configured_size(), (800, 600));

    // Zero withdraws the cap rather than asking for a zero-sized window.
    fixture.toplevel.set_max_size(0, 0);
    assert_eq!(fixture.commit(), None);
    assert_eq!(
        fixture.latest_configured_size(),
        pane,
        "zero should withdraw the maximum, not clamp the pane to nothing"
    );
}
