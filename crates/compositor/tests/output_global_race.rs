//! A client may bind an output it learned about just as the compositor
//! withdraws it.  `global_remove` and `wl_registry.bind` travel in opposite
//! directions, so freeing the global immediately turns that valid race into a
//! fatal protocol error.

#![cfg(target_os = "linux")]

use std::os::unix::net::UnixStream;
use std::sync::Arc;

use blit_compositor::spawn_compositor;
use wayland_client::protocol::{wl_compositor, wl_output, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    outputs: Vec<(u32, u32)>,
    removed_outputs: Vec<u32>,
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
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" => {
                    state.compositor =
                        Some(registry.bind::<wl_compositor::WlCompositor, _, _>(name, 4, qh, ()));
                }
                "xdg_wm_base" => {
                    state.wm_base =
                        Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(name, 5, qh, ()));
                }
                "wl_output" => state.outputs.push((name, version)),
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => state.removed_outputs.push(name),
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
fn output_removal_does_not_reject_an_in_flight_bind() {
    let handle = spawn_compositor(false, Arc::new(|| {}), "");
    let stream = UnixStream::connect(&handle.socket_name).expect("connect to compositor socket");
    let conn = Connection::from_socket(stream).expect("wayland connection");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());

    let mut app = App::default();
    queue.roundtrip(&mut app).expect("registry roundtrip");
    assert_eq!(app.outputs.len(), 1, "client starts with one output");

    let compositor = app.compositor.clone().expect("wl_compositor advertised");
    let wm_base = app.wm_base.clone().expect("xdg_wm_base advertised");

    let first_surface = compositor.create_surface(&qh, ());
    let first_xdg = wm_base.get_xdg_surface(&first_surface, &qh, ());
    let first_toplevel = first_xdg.get_toplevel(&qh, ());
    queue.roundtrip(&mut app).expect("first toplevel roundtrip");
    assert_eq!(
        app.outputs.len(),
        1,
        "first toplevel claims the initial output"
    );

    let second_surface = compositor.create_surface(&qh, ());
    let second_xdg = wm_base.get_xdg_surface(&second_surface, &qh, ());
    let second_toplevel = second_xdg.get_toplevel(&qh, ());
    queue
        .roundtrip(&mut app)
        .expect("second toplevel roundtrip");
    assert_eq!(
        app.outputs.len(),
        2,
        "second toplevel publishes another output"
    );
    let (removed_name, removed_version) = app.outputs[1];

    // Queue the output's destruction and a bind based on its earlier registry
    // advertisement without reading the intervening global_remove.  The
    // server sees the destroy first; an immediate remove_global() then kills
    // this otherwise-valid client when it processes the bind next.
    second_toplevel.destroy();
    let _late_output =
        registry.bind::<wl_output::WlOutput, _, _>(removed_name, removed_version.min(4), &qh, ());
    queue
        .roundtrip(&mut app)
        .expect("late output bind must survive global removal");
    assert!(
        app.removed_outputs.contains(&removed_name),
        "the output was not withdrawn"
    );

    second_xdg.destroy();
    second_surface.destroy();
    first_toplevel.destroy();
    first_xdg.destroy();
    first_surface.destroy();
    handle.stop();
}
