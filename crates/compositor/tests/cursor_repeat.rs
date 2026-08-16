//! A cursor surface commits far more often than its artwork changes.
//!
//! Xwayland re-attaches its cursor on pointer enter and on every update it
//! was throttling behind a frame callback, so the same image is committed
//! again and again. Announcing it every time is not merely wasted bandwidth:
//! a viewer that rebuilds an object URL per announcement revokes the one it
//! is drawing from, and the cursor blinks at whatever rate the commits
//! arrive.

#![cfg(target_os = "linux")]

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use blit_compositor::{CompositorEvent, spawn_compositor_without_renderer};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
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
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor =
                        Some(registry.bind::<wl_compositor::WlCompositor, _, _>(name, 4, qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, 5, qh, ()));
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_seat::WlSeat);
delegate_noop!(App: ignore wl_pointer::WlPointer);

/// Commit the same cursor artwork repeatedly; only the first may be
/// announced.
#[test]
fn repeating_identical_cursor_artwork_is_announced_once() {
    let handle = spawn_compositor_without_renderer(false, Arc::new(|| {}));
    let stream = UnixStream::connect(&handle.socket_name).expect("connect to compositor socket");
    let conn = Connection::from_socket(stream).expect("wayland connection");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut app = App::default();
    queue.roundtrip(&mut app).expect("registry roundtrip");
    let compositor = app.compositor.clone().expect("wl_compositor");
    let shm = app.shm.clone().expect("wl_shm");
    let seat = app.seat.clone().expect("wl_seat");
    let pointer = seat.get_pointer(&qh, ());

    // A 4x4 opaque cursor in a shm pool.
    let (w, h) = (4i32, 4i32);
    let stride = w * 4;
    let size = stride * h;
    // An unlinked file under the runtime dir: the compositor only needs a
    // readable fd of the right length, and this avoids a dev-dependency.
    let path = std::env::temp_dir().join(format!("blit-cursor-test-{}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("shm backing file");
    std::fs::remove_file(&path).ok();
    file.set_len(size as u64).expect("size shm file");
    let pool = shm.create_pool(std::os::fd::AsFd::as_fd(&file), size, &qh, ());
    let buffer = pool.create_buffer(0, w, h, stride, wl_shm::Format::Argb8888, &qh, ());

    let cursor = compositor.create_surface(&qh, ());
    pointer.set_cursor(0, Some(&cursor), 1, 1);
    // Five commits of byte-identical artwork.
    for _ in 0..5 {
        cursor.attach(Some(&buffer), 0, 0);
        cursor.damage(0, 0, w, h);
        cursor.commit();
        queue.roundtrip(&mut app).expect("cursor commit roundtrip");
    }

    let mut announcements = 0usize;
    while let Ok(event) = handle.event_rx.recv_timeout(Duration::from_millis(300)) {
        if matches!(event, CompositorEvent::SurfaceCursor { .. }) {
            announcements += 1;
        }
    }
    assert_eq!(
        announcements, 1,
        "identical cursor artwork was announced {announcements} times; a viewer that \
         rebuilds its object URL per announcement blinks once per repeat",
    );

    handle.stop();
}
