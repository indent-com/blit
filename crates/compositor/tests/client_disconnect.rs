//! A closed client connection must unmap its toplevel before a stale request
//! backlog is dispatched.

#![cfg(target_os = "linux")]

use std::net::Shutdown;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use blit_compositor::{CompositorEvent, spawn_compositor_without_renderer};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    configured: bool,
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
            "wl_shm" => {
                state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
            }
            "xdg_wm_base" => {
                state.wm_base =
                    Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(name, 5, qh, ()));
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
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore xdg_toplevel::XdgToplevel);

#[test]
fn handed_off_connection_outlives_the_original_process() {
    let handle = spawn_compositor_without_renderer(false, Arc::new(|| {}));

    let status = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--ignored",
            "--exact",
            "client_process_hands_connection_to_descendant",
        ])
        .env("BLIT_TEST_WAYLAND_SOCKET", &handle.socket_name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run Wayland client helper");
    assert!(status.success(), "Wayland client helper failed: {status}");

    let surface_id = recv_surface_created(&handle);
    let must_remain_until = Instant::now() + Duration::from_millis(150);
    loop {
        let remaining = must_remain_until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match handle.event_rx.recv_timeout(remaining) {
            Ok(CompositorEvent::SurfaceDestroyed {
                surface_id: destroyed,
            }) if destroyed == surface_id => {
                panic!("surface {surface_id} died with the process that handed off its socket")
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => {
                panic!("compositor event channel disconnected")
            }
        }
    }

    recv_surface_destroyed(&handle, surface_id, Duration::from_secs(2));
    handle.stop();
}

#[test]
fn closed_client_drops_queued_commits_before_destroying_surface() {
    let handle = spawn_compositor_without_renderer(false, Arc::new(|| {}));

    let stream = UnixStream::connect(&handle.socket_name).expect("connect to compositor socket");
    let disconnect = stream.try_clone().expect("clone Wayland socket");
    let conn = Connection::from_socket(stream).expect("wayland connection");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut app = App::default();
    queue.roundtrip(&mut app).expect("registry roundtrip");
    let surface = app
        .compositor
        .as_ref()
        .expect("wl_compositor advertised")
        .create_surface(&qh, ());
    let xdg_surface = app
        .wm_base
        .as_ref()
        .expect("xdg_wm_base advertised")
        .get_xdg_surface(&surface, &qh, ());
    let _toplevel = xdg_surface.get_toplevel(&qh, ());
    surface.commit();
    queue.roundtrip(&mut app).expect("map roundtrip");
    assert!(app.configured, "surface was not configured");

    // Use a real buffer so each stale commit is expensive enough to expose
    // the delay reliably. The backing pages may remain sparse; their content
    // is immaterial to disconnect cleanup.
    const WIDTH: i32 = 1024;
    const HEIGHT: i32 = 1024;
    let size = (WIDTH * HEIGHT * 4) as usize;
    let raw_fd = unsafe { libc::memfd_create(c"client-disconnect".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw_fd >= 0, "memfd_create failed");
    assert_eq!(unsafe { libc::ftruncate(raw_fd, size as libc::off_t) }, 0);
    let backing = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let pool = app.shm.as_ref().expect("wl_shm advertised").create_pool(
        backing.as_fd(),
        size as i32,
        &qh,
        (),
    );
    let buffer = pool.create_buffer(
        0,
        WIDTH,
        HEIGHT,
        WIDTH * 4,
        wl_shm::Format::Xrgb8888,
        &qh,
        (),
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, WIDTH, HEIGHT);
    surface.commit();
    queue.roundtrip(&mut app).expect("buffer roundtrip");

    let surface_id = recv_surface_created(&handle);

    // Fill the socket with work that is obsolete as soon as its peer closes.
    // wayland-backend normally drains this entire readable backlog before it
    // reads EOF, which used to postpone SurfaceDestroyed for seconds.
    for _ in 0..10_000 {
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, WIDTH, HEIGHT);
        surface.commit();
    }
    let _ = conn.flush();

    let disconnected_at = Instant::now();
    disconnect
        .shutdown(Shutdown::Write)
        .expect("close Wayland connection writer");

    loop {
        match handle.event_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(CompositorEvent::SurfaceDestroyed {
                surface_id: destroyed,
            }) if destroyed == surface_id => {
                assert!(
                    disconnected_at.elapsed() < Duration::from_millis(250),
                    "disconnect cleanup waited {:?}",
                    disconnected_at.elapsed(),
                );
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("surface {surface_id} was not destroyed promptly: {err}"),
        }
    }

    handle.stop();
}

fn recv_surface_created(handle: &blit_compositor::CompositorHandle) -> u16 {
    loop {
        match handle.event_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(CompositorEvent::SurfaceCreated { surface_id, .. }) => return surface_id,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => panic!("no SurfaceCreated within 5s"),
            Err(RecvTimeoutError::Disconnected) => panic!("compositor event channel disconnected"),
        }
    }
}

fn recv_surface_destroyed(
    handle: &blit_compositor::CompositorHandle,
    surface_id: u16,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        match handle
            .event_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(CompositorEvent::SurfaceDestroyed {
                surface_id: destroyed,
            }) if destroyed == surface_id => return,
            Ok(_) => {}
            Err(err) => panic!("surface {surface_id} was not destroyed: {err}"),
        }
    }
}

/// Connect in one process and keep the same connection alive in a descendant.
#[test]
#[ignore]
fn client_process_hands_connection_to_descendant() {
    let Ok(socket) = std::env::var("BLIT_TEST_WAYLAND_SOCKET") else {
        return;
    };

    let stream = UnixStream::connect(socket).expect("connect to compositor socket");
    let conn = Connection::from_socket(stream).expect("wayland connection");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut app = App::default();
    queue.roundtrip(&mut app).expect("registry roundtrip");
    let surface = app
        .compositor
        .as_ref()
        .expect("wl_compositor advertised")
        .create_surface(&qh, ());
    let xdg_surface = app
        .wm_base
        .as_ref()
        .expect("xdg_wm_base advertised")
        .get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    surface.commit();
    queue.roundtrip(&mut app).expect("map roundtrip");

    let child = unsafe { libc::fork() };
    assert!(
        child >= 0,
        "fork failed: {}",
        std::io::Error::last_os_error()
    );
    if child == 0 {
        std::thread::sleep(Duration::from_millis(600));
        unsafe { libc::_exit(0) };
    }

    // Exit without destructors, as a killed connector would. The descendant
    // still owns the socket and is therefore the live Wayland client.
    let _keep_alive = (toplevel, xdg_surface, surface, queue, conn);
    unsafe { libc::_exit(0) };
}
