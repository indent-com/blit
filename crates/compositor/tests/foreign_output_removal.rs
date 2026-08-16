//! Output globals are owner-filtered: `can_view` hands a `wl_output` only to
//! the client whose toplevel it describes.  Withdrawing one must stay just as
//! private -- a client that never saw the global must never be told it went
//! away.
//!
//! It is not a cosmetic leak.  `wl_registry.global_remove` names a global a
//! client has no entry for, and a client that keeps a map keyed by registry
//! name has nothing to remove.  xwayland-satellite unwraps exactly that lookup
//! (`src/server/mod.rs`, `globals_map.remove(&global).unwrap()`), so one
//! foreign toplevel closing panics the bridge and takes every X11 application
//! in the session with it.

#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use blit_compositor::spawn_compositor_without_renderer;
use wayland_client::protocol::{wl_compositor, wl_output, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    /// Every global name this client was ever told about, of any interface.
    seen: HashSet<u32>,
    removed: Vec<u32>,
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
        match event {
            wl_registry::Event::Global {
                name, interface, ..
            } => {
                state.seen.insert(name);
                match interface.as_str() {
                    "wl_compositor" => {
                        state.compositor = Some(
                            registry.bind::<wl_compositor::WlCompositor, _, _>(name, 4, qh, ()),
                        );
                    }
                    "xdg_wm_base" => {
                        state.wm_base =
                            Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(name, 5, qh, ()));
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => state.removed.push(name),
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

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_output::WlOutput);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore xdg_toplevel::XdgToplevel);

#[test]
fn withdrawing_an_output_stays_private_to_its_owner() {
    let handle = spawn_compositor_without_renderer(false, Arc::new(|| {}));

    let connect = || {
        let stream =
            UnixStream::connect(&handle.socket_name).expect("connect to compositor socket");
        let conn = Connection::from_socket(stream).expect("wayland connection");
        let queue = conn.new_event_queue();
        (conn, queue)
    };

    // The owner: it opens a second toplevel, so a second output global exists.
    let (_owner_conn, mut owner_queue) = connect();
    let owner_qh = owner_queue.handle();
    let _owner_registry = _owner_conn.display().get_registry(&owner_qh, ());
    let mut owner = App::default();
    owner_queue.roundtrip(&mut owner).expect("owner registry");

    // The observer stands in for the xwayland-satellite bridge: its own
    // connection, its own registry, no interest in anyone else's toplevels.
    let (_obs_conn, mut obs_queue) = connect();
    let obs_qh = obs_queue.handle();
    let _obs_registry = _obs_conn.display().get_registry(&obs_qh, ());
    let mut observer = App::default();
    obs_queue
        .roundtrip(&mut observer)
        .expect("observer registry");

    let compositor = owner.compositor.clone().expect("wl_compositor advertised");
    let wm_base = owner.wm_base.clone().expect("xdg_wm_base advertised");

    let first_surface = compositor.create_surface(&owner_qh, ());
    let first_xdg = wm_base.get_xdg_surface(&first_surface, &owner_qh, ());
    let first_toplevel = first_xdg.get_toplevel(&owner_qh, ());
    owner_queue.roundtrip(&mut owner).expect("first toplevel");

    let second_surface = compositor.create_surface(&owner_qh, ());
    let second_xdg = wm_base.get_xdg_surface(&second_surface, &owner_qh, ());
    let second_toplevel = second_xdg.get_toplevel(&owner_qh, ());
    owner_queue.roundtrip(&mut owner).expect("second toplevel");

    // The observer must not have learned of the owner's extra output at all.
    obs_queue
        .roundtrip(&mut observer)
        .expect("observer settles");
    let seen_before = observer.seen.clone();

    // Closing it retires the owner's second output slot.
    second_toplevel.destroy();
    owner_queue.roundtrip(&mut owner).expect("owner settles");
    obs_queue
        .roundtrip(&mut observer)
        .expect("observer settles after the foreign toplevel closes");

    assert!(
        observer.seen == seen_before,
        "the observer should learn of no new globals from a foreign toplevel"
    );
    let unknown: Vec<u32> = observer
        .removed
        .iter()
        .copied()
        .filter(|name| !seen_before.contains(name))
        .collect();
    assert!(
        unknown.is_empty(),
        "observer was told to remove global(s) {unknown:?} it was never told about; \
         it only ever saw {seen_before:?}"
    );

    second_xdg.destroy();
    second_surface.destroy();
    first_toplevel.destroy();
    first_xdg.destroy();
    first_surface.destroy();
    handle.stop();
}
