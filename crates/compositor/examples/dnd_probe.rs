//! Live drag-and-drop probe: a minimal Wayland client that connects to a
//! running blit server's compositor, maps an xdg toplevel, binds
//! wl_data_device, and logs to stdout every drag event it sees — offers with
//! their MIME types, enter/motion/leave/drop with coordinates, action
//! negotiation events, and the bytes it reads when it `receive()`s an
//! offered mime after a drop.
//!
//! It behaves like a cooperative drop target: `accept`s the first offered
//! mime on enter, and after a drop `receive()`s every advertised mime,
//! logging byte counts and a printable preview, then `finish()`es the offer.
//!
//! Usage: dnd_probe [socket-path]
//! (defaults to $WAYLAND_DISPLAY, resolved against $XDG_RUNTIME_DIR when
//! relative)

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};

use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

macro_rules! log {
    ($($arg:tt)*) => {{
        println!($($arg)*);
        let _ = std::io::stdout().flush();
    }};
}

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    ddm: Option<wl_data_device_manager::WlDataDeviceManager>,
    shm: Option<wl_shm::WlShm>,
    configured: bool,
    /// Live offers by object id, with the mimes advertised so far.
    offers: HashMap<String, (wl_data_offer::WlDataOffer, Vec<String>)>,
    /// The offer named by the current drag's `enter`.
    drag_offer: Option<wl_data_offer::WlDataOffer>,
}

fn offer_key(offer: &wl_data_offer::WlDataOffer) -> String {
    format!("{:?}", offer.id())
}

/// Ask the offer for `mime`'s bytes, with a 2s timeout.
fn receive_and_log(offer: &wl_data_offer::WlDataOffer, conn: &Connection, mime: &str) {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        log!("RECEIVE {mime} pipe-failed");
        return;
    }
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    offer.receive(mime.to_string(), write_fd.as_fd());
    let _ = conn.flush();
    drop(write_fd); // our copy; EOF arrives when the compositor closes its end
    let mut pfd = libc::pollfd {
        fd: read_fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pfd, 1, 2000) };
    if ready <= 0 {
        log!("RECEIVE {mime} TIMEOUT");
        return;
    }
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = unsafe {
            libc::read(
                read_fd.as_raw_fd(),
                tmp.as_mut_ptr() as *mut libc::c_void,
                tmp.len(),
            )
        };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n as usize]);
        if buf.len() > 16 * 1024 * 1024 {
            break;
        }
    }
    let preview: String = buf
        .iter()
        .take(96)
        .map(|b| {
            if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '.'
            }
        })
        .collect();
    log!("RECEIVE {mime} {} bytes [{preview}]", buf.len());
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
            "wl_seat" => {
                state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, 7, qh, ()));
            }
            "wl_data_device_manager" => {
                state.ddm = Some(
                    registry.bind::<wl_data_device_manager::WlDataDeviceManager, _, _>(
                        name,
                        3,
                        qh,
                        (),
                    ),
                );
            }
            "wl_shm" => {
                state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            log!("SEAT-CAPS {capabilities:?}");
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        conn: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                let key = offer_key(&id);
                state.offers.insert(key.clone(), (id, Vec::new()));
                log!("OFFER-NEW {key}");
            }
            wl_data_device::Event::Enter {
                serial, x, y, id, ..
            } => {
                log!("DND-ENTER serial={serial} x={x:.1} y={y:.1}");
                if let Some(ref offer) = id {
                    state.drag_offer = Some(offer.clone());
                    let key = offer_key(offer);
                    let mimes = state
                        .offers
                        .get(&key)
                        .map(|(_, m)| m.clone())
                        .unwrap_or_default();
                    log!("DND-ENTER offer={key} mimes={mimes:?}");
                    // Behave like a cooperative target: accept the mime we
                    // would want (uri-list first, then anything).
                    let want = mimes
                        .iter()
                        .find(|m| m.as_str() == "text/uri-list")
                        .or(mimes.first())
                        .cloned();
                    offer.accept(serial, want);
                } else {
                    log!("DND-ENTER offer=NONE");
                }
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                log!("DND-MOTION x={x:.1} y={y:.1}");
            }
            wl_data_device::Event::Leave => {
                log!("DND-LEAVE");
                state.drag_offer = None;
            }
            wl_data_device::Event::Drop => {
                log!("DND-DROP");
                if let Some(ref offer) = state.drag_offer {
                    let key = offer_key(offer);
                    let mimes = state
                        .offers
                        .get(&key)
                        .map(|(_, m)| m.clone())
                        .unwrap_or_default();
                    for mime in &mimes {
                        receive_and_log(offer, conn, mime);
                    }
                    offer.finish();
                    let _ = conn.flush();
                    log!("DND-FINISH sent");
                } else {
                    log!("DND-DROP with no live offer");
                }
            }
            wl_data_device::Event::Selection { id } => {
                match id {
                    Some(offer) => log!("SELECTION offer={}", offer_key(&offer)),
                    None => log!("SELECTION none"),
                }
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(App, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for App {
    fn event(
        state: &mut Self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let key = offer_key(offer);
        match event {
            wl_data_offer::Event::Offer { mime_type } => {
                log!("OFFER-MIME {key} {mime_type}");
                if let Some((_, mimes)) = state.offers.get_mut(&key) {
                    mimes.push(mime_type);
                }
            }
            wl_data_offer::Event::SourceActions { source_actions } => {
                log!("OFFER-SOURCE-ACTIONS {key} {source_actions:?}");
            }
            wl_data_offer::Event::Action { dnd_action } => {
                log!("OFFER-ACTION {key} {dnd_action:?}");
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

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        _: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            log!("TOPLEVEL-CLOSE");
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_data_device_manager::WlDataDeviceManager);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);

fn socket_path() -> String {
    let name = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("WAYLAND_DISPLAY").ok())
        .expect("usage: dnd_probe <socket-path> (or set WAYLAND_DISPLAY)");
    if name.starts_with('/') {
        name
    } else {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        format!("{dir}/{name}")
    }
}

fn main() {
    let path = socket_path();
    let stream = UnixStream::connect(&path).expect("connect to compositor socket");
    let conn = Connection::from_socket(stream).expect("wayland connection");
    let mut queue: EventQueue<App> = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut app = App::default();
    queue.roundtrip(&mut app).expect("registry roundtrip");

    let compositor = app.compositor.clone().expect("wl_compositor advertised");
    let wm_base = app.wm_base.clone().expect("xdg_wm_base advertised");
    let seat = app.seat.clone().expect("wl_seat advertised");
    let ddm = app.ddm.clone().expect("wl_data_device_manager advertised");
    let shm = app.shm.clone().expect("wl_shm advertised");

    // The data device has to exist before the drag enters: the offer is
    // handed to whichever device is bound at that moment.
    let _device = ddm.get_data_device(&seat, &qh, ());

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_app_id("dnd-probe".into());
    toplevel.set_title("dnd-probe".into());
    surface.commit();
    queue.roundtrip(&mut app).expect("map roundtrip");

    // xdg_shell forbids attaching a buffer before the first configure.
    for _ in 0..50 {
        if app.configured {
            break;
        }
        queue.roundtrip(&mut app).expect("configure roundtrip");
    }
    if !app.configured {
        log!("WARN never configured; attaching buffer anyway");
    }

    // A real opaque buffer, so the compositor treats this as a live toplevel
    // with content.
    const W: i32 = 200;
    const H: i32 = 200;
    let size = (W * H * 4) as usize;
    let fd = unsafe { libc::memfd_create(c"dnd-probe".as_ptr(), 0) };
    assert!(fd >= 0, "memfd_create failed");
    assert_eq!(unsafe { libc::ftruncate(fd, size as libc::off_t) }, 0);
    let pixels = vec![0xFFu8; size];
    assert_eq!(
        unsafe { libc::write(fd, pixels.as_ptr() as *const libc::c_void, size) } as usize,
        size
    );
    let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let pool = shm.create_pool(owned_fd.as_fd(), size as i32, &qh, ());
    std::mem::forget(owned_fd); // keep the backing fd alive for the pool
    let buffer = pool.create_buffer(0, W, H, W * 4, wl_shm::Format::Xrgb8888, &qh, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, W, H);
    surface.commit();
    queue.roundtrip(&mut app).expect("buffer roundtrip");
    log!("MAPPED {W}x{H}");
    log!("READY");

    loop {
        let _ = conn.flush();
        if let Some(guard) = queue.prepare_read() {
            let _ = guard.read();
        }
        queue.dispatch_pending(&mut app).expect("dispatch");
    }
}
