//! A compositor-dismissed popup must disappear without another client commit.
//!
//! Brave leaves an otherwise idle page behind its context menu.  If
//! `popup_done` only tells the client to tear the popup down, the last encoded
//! root frame keeps containing the menu until some unrelated page repaint
//! happens hundreds of milliseconds later.  The xdg-shell contract says the
//! compositor unmaps the popup at the same time it sends `popup_done`, so that
//! topology change itself has to produce a new root composite.

#![cfg(target_os = "linux")]

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use blit_compositor::{CompositorCommand, CompositorEvent, spawn_compositor};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    popup_done: bool,
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
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => {
                state.compositor =
                    Some(registry.bind::<wl_compositor::WlCompositor, _, _>(name, 4, qh, ()));
            }
            "wl_shm" => {
                state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
            }
            "wl_seat" => {
                state.seat =
                    Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(9), qh, ()));
            }
            "xdg_wm_base" => {
                state.wm_base = Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(
                    name,
                    version.min(6),
                    qh,
                    (),
                ));
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

impl Dispatch<xdg_popup::XdgPopup, ()> for App {
    fn event(
        state: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, xdg_popup::Event::PopupDone) {
            state.popup_done = true;
        }
    }
}

delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_seat::WlSeat);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore xdg_positioner::XdgPositioner);
delegate_noop!(App: ignore xdg_toplevel::XdgToplevel);

#[test]
fn popup_done_publishes_the_uncovered_toplevel_without_another_commit() {
    let handle = spawn_compositor(false, Arc::new(|| {}), "");
    let stream = UnixStream::connect(&handle.socket_name).expect("connect to compositor socket");
    let conn = Connection::from_socket(stream).expect("wayland connection");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut app = App::default();
    queue.roundtrip(&mut app).expect("registry roundtrip");
    let compositor = app.compositor.clone().expect("wl_compositor advertised");
    let wm_base = app.wm_base.clone().expect("xdg_wm_base advertised");
    let seat = app.seat.clone().expect("wl_seat advertised");

    const ROOT_W: i32 = 160;
    const ROOT_H: i32 = 120;
    const POPUP_W: i32 = 48;
    const POPUP_H: i32 = 40;
    let root_bytes = ROOT_W * ROOT_H * 4;
    let popup_bytes = POPUP_W * POPUP_H * 4;
    let pool_bytes = root_bytes + popup_bytes;
    let raw_fd = unsafe { libc::memfd_create(c"popup-dismiss".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw_fd >= 0, "memfd_create failed");
    assert_eq!(unsafe { libc::ftruncate(raw_fd, pool_bytes.into()) }, 0);
    let backing = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let pool = app.shm.as_ref().expect("wl_shm advertised").create_pool(
        backing.as_fd(),
        pool_bytes,
        &qh,
        (),
    );

    let root_buffer = pool.create_buffer(
        0,
        ROOT_W,
        ROOT_H,
        ROOT_W * 4,
        wl_shm::Format::Xrgb8888,
        &qh,
        (),
    );
    let popup_buffer = pool.create_buffer(
        root_bytes,
        POPUP_W,
        POPUP_H,
        POPUP_W * 4,
        wl_shm::Format::Xrgb8888,
        &qh,
        (),
    );

    let root = compositor.create_surface(&qh, ());
    let root_xdg = wm_base.get_xdg_surface(&root, &qh, ());
    let _toplevel = root_xdg.get_toplevel(&qh, ());
    root.commit();
    queue.roundtrip(&mut app).expect("root configure roundtrip");
    root.attach(Some(&root_buffer), 0, 0);
    root.damage_buffer(0, 0, ROOT_W, ROOT_H);
    root.commit();
    queue.roundtrip(&mut app).expect("root buffer roundtrip");

    let surface_id = recv_surface_created(&handle);
    // Rendering can be unavailable on a machine with no Vulkan device.  The
    // protocol half still runs there, but there is no frame stream whose
    // invalidation this test can observe.
    if !recv_surface_commit(&handle, surface_id, Duration::from_secs(2)) {
        handle.stop();
        return;
    }

    let popup_wl = compositor.create_surface(&qh, ());
    let popup_xdg = wm_base.get_xdg_surface(&popup_wl, &qh, ());
    let positioner = wm_base.create_positioner(&qh, ());
    positioner.set_size(POPUP_W, POPUP_H);
    positioner.set_anchor_rect(12, 12, 1, 1);
    let popup = popup_xdg.get_popup(Some(&root_xdg), &positioner, &qh, ());
    popup.grab(&seat, 1);
    popup_wl.commit();
    queue
        .roundtrip(&mut app)
        .expect("popup configure roundtrip");
    popup_wl.attach(Some(&popup_buffer), 0, 0);
    popup_wl.damage_buffer(0, 0, POPUP_W, POPUP_H);
    popup_wl.commit();
    queue.roundtrip(&mut app).expect("popup buffer roundtrip");
    assert!(
        recv_surface_commit(&handle, surface_id, Duration::from_secs(2)),
        "mapping the popup never produced a root frame"
    );

    // Let every result from the popup's mapping submit retire, then discard
    // it.  From here on the client intentionally sends no buffer commit, so
    // only compositor-side popup invalidation can produce another frame.
    drain_events(&handle, Duration::from_millis(100));
    handle
        .command_tx
        .send(CompositorCommand::PointerMotion {
            surface_id,
            x: 130.0,
            y: 100.0,
            time_ms: 0,
        })
        .expect("send outside motion");
    handle
        .command_tx
        .send(CompositorCommand::PointerButton {
            surface_id,
            button: 0x110,
            pressed: true,
            time_ms: 0,
        })
        .expect("send outside press");
    handle.wake();
    queue.roundtrip(&mut app).expect("dismiss roundtrip");

    assert!(app.popup_done, "outside press did not dismiss the popup");
    assert!(
        recv_surface_commit(&handle, surface_id, Duration::from_secs(1)),
        "popup_done left the menu in the last published frame until a future client commit"
    );

    // Keep the protocol objects alive through the assertion above.
    let _keep_alive = (popup, popup_xdg, popup_wl, positioner, pool, backing);
    handle.stop();
}

fn recv_surface_created(handle: &blit_compositor::CompositorHandle) -> u16 {
    loop {
        match handle.event_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(CompositorEvent::SurfaceCreated { surface_id, .. }) => return surface_id,
            Ok(_) => {}
            Err(err) => panic!("compositor never announced the toplevel: {err}"),
        }
    }
}

fn recv_surface_commit(
    handle: &blit_compositor::CompositorHandle,
    surface_id: u16,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match handle
            .event_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(CompositorEvent::SurfaceCommit {
                surface_id: committed,
                ..
            }) if committed == surface_id => return true,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return false,
            Err(RecvTimeoutError::Disconnected) => {
                panic!("compositor event channel disconnected")
            }
        }
    }
}

fn drain_events(handle: &blit_compositor::CompositorHandle, quiet_for: Duration) {
    loop {
        match handle.event_rx.recv_timeout(quiet_for) {
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return,
            Err(RecvTimeoutError::Disconnected) => {
                panic!("compositor event channel disconnected")
            }
        }
    }
}
