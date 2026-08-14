#![cfg(target_os = "linux")]

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use blit_compositor::{CompositorEvent, spawn_compositor_without_renderer};
use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::xdg::foreign::zv2::client::{zxdg_exported_v2, zxdg_exporter_v2};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    exporter: Option<zxdg_exporter_v2::ZxdgExporterV2>,
    handles: Vec<String>,
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
                    Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(name, 5, qh, ()));
            }
            "zxdg_exporter_v2" => {
                state.exporter =
                    Some(registry.bind::<zxdg_exporter_v2::ZxdgExporterV2, _, _>(name, 1, qh, ()));
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
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
        }
    }
}

impl Dispatch<zxdg_exported_v2::ZxdgExportedV2, ()> for App {
    fn event(
        state: &mut Self,
        _: &zxdg_exported_v2::ZxdgExportedV2,
        event: zxdg_exported_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_exported_v2::Event::Handle { handle } = event {
            state.handles.push(handle);
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore xdg_toplevel::XdgToplevel);
delegate_noop!(App: ignore zxdg_exporter_v2::ZxdgExporterV2);

#[test]
fn exported_parent_handles_resolve_only_while_the_toplevel_and_export_live() {
    let mut compositor = Some(spawn_compositor_without_renderer(false, Arc::new(|| {})));
    let stream = UnixStream::connect(&compositor.as_ref().unwrap().socket_name)
        .expect("connect to compositor socket");
    let connection = Connection::from_socket(stream).expect("wayland connection");
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    connection.display().get_registry(&qh, ());
    let mut app = App::default();
    queue.roundtrip(&mut app).expect("registry roundtrip");

    let wl_compositor = app.compositor.clone().expect("wl_compositor advertised");
    let wm_base = app.wm_base.clone().expect("xdg_wm_base advertised");
    let exporter = app
        .exporter
        .clone()
        .expect("xdg-foreign exporter advertised");
    let surface = wl_compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    surface.commit();
    queue.roundtrip(&mut app).expect("map toplevel");
    let surface_id = loop {
        match compositor
            .as_mut()
            .unwrap()
            .event_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("surface creation event")
        {
            CompositorEvent::SurfaceCreated { surface_id, .. } => break surface_id,
            _ => continue,
        }
    };

    let first = exporter.export_toplevel(&surface, &qh, ());
    queue.roundtrip(&mut app).expect("receive first handle");
    let first_handle = app.handles.pop().expect("first exported handle");
    assert_eq!(first_handle.len(), 32);
    assert!(first_handle.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(
        compositor
            .as_ref()
            .unwrap()
            .resolve_foreign_parent(&format!("wayland:{first_handle}")),
        Some(surface_id)
    );
    assert_eq!(
        compositor
            .as_ref()
            .unwrap()
            .resolve_foreign_parent(&first_handle),
        None
    );
    first.destroy();
    queue.roundtrip(&mut app).expect("destroy first export");
    assert_eq!(
        compositor
            .as_ref()
            .unwrap()
            .resolve_foreign_parent(&format!("wayland:{first_handle}")),
        None
    );

    let _second = exporter.export_toplevel(&surface, &qh, ());
    queue.roundtrip(&mut app).expect("receive second handle");
    let second_handle = app.handles.pop().expect("second exported handle");
    assert_ne!(first_handle, second_handle);
    toplevel.destroy();
    queue.roundtrip(&mut app).expect("destroy toplevel role");
    assert_eq!(
        compositor
            .as_ref()
            .unwrap()
            .resolve_foreign_parent(&format!("wayland:{second_handle}")),
        None
    );

    compositor.take().unwrap().stop();
}
