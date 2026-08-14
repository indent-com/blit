//! A file dragged from the user's desktop has to reach the app as a real
//! Wayland drag session, not as a paste in disguise.
//!
//! The drag has no Wayland client behind it, so the compositor drives the
//! `wl_data_device` session itself with a null source: enter, motion, drop,
//! and an offer that answers `receive` from the DROP-time payload.  Nothing
//! on the compositor side distinguishes "session never entered" from
//! "entered and quietly dropped the data", so these tests pin what the app
//! sees at each step, including the cancel path.

#![cfg(target_os = "linux")]

use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::{
    wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_registry, wl_seat,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};

use blit_compositor::{
    CompositorCommand, CompositorEvent, CompositorHandle, spawn_compositor_without_renderer,
};

use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// A `wl_data_device` drag event, reduced to what these tests care about.
#[derive(Debug, PartialEq, Clone)]
enum Drag {
    Enter { x: f64, y: f64 },
    Motion { x: f64, y: f64 },
    Leave,
    Drop,
}

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    ddm: Option<wl_data_device_manager::WlDataDeviceManager>,
    seat: Option<wl_seat::WlSeat>,
    events: Vec<Drag>,
    /// MIME types advertised on the current offer, in arrival order.
    offered: Vec<String>,
    /// `source_actions` events on the current offer, as raw bits.
    source_actions: Vec<u32>,
    /// `action` events on the current offer, as raw bits.
    actions: Vec<u32>,
    /// The offer named by the current drag's `enter`.
    drag_offer: Option<wl_data_offer::WlDataOffer>,
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
            "wl_seat" => {
                state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, 7, qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { .. } => {
                state.offered.clear();
                state.source_actions.clear();
                state.actions.clear();
            }
            wl_data_device::Event::Enter { x, y, id, .. } => {
                state.drag_offer = id;
                state.events.push(Drag::Enter { x, y });
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                state.events.push(Drag::Motion { x, y });
            }
            wl_data_device::Event::Leave => state.events.push(Drag::Leave),
            wl_data_device::Event::Drop => state.events.push(Drag::Drop),
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
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_client::WEnum;
        match event {
            wl_data_offer::Event::Offer { mime_type } => state.offered.push(mime_type),
            wl_data_offer::Event::SourceActions { source_actions } => {
                let bits = match source_actions {
                    WEnum::Value(a) => a.bits(),
                    WEnum::Unknown(b) => b,
                };
                state.source_actions.push(bits);
            }
            wl_data_offer::Event::Action { dnd_action } => {
                let bits = match dnd_action {
                    WEnum::Value(a) => a.bits(),
                    WEnum::Unknown(b) => b,
                };
                state.actions.push(bits);
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
        _: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        _: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_seat::WlSeat);
delegate_noop!(App: ignore wl_data_device_manager::WlDataDeviceManager);

struct Fixture {
    app: App,
    queue: EventQueue<App>,
    conn: Connection,
    surface_id: u16,
    _surface: wl_surface::WlSurface,
    _xdg_surface: xdg_surface::XdgSurface,
    _device: wl_data_device::WlDataDevice,
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
        let ddm = app.ddm.clone().expect("wl_data_device_manager advertised");
        let seat = app.seat.clone().expect("wl_seat advertised");

        // The data device has to exist before the drag enters: the offer is
        // handed to whichever device is bound at that moment.
        let device = ddm.get_data_device(&seat, &qh, ());

        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let _toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        queue.roundtrip(&mut app).expect("map roundtrip");

        // The compositor names the surface in an event; the drag commands
        // are aimed by surface id.
        let surface_id = loop {
            match handle.event_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(CompositorEvent::SurfaceCreated { surface_id, .. }) => break surface_id,
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => panic!("no SurfaceCreated within 5s"),
                Err(e) => panic!("compositor event channel closed: {e}"),
            }
        };

        Self {
            app,
            queue,
            conn,
            surface_id,
            _surface: surface,
            _xdg_surface: xdg_surface,
            _device: device,
            handle: Some(handle),
        }
    }

    fn send(&mut self, cmd: CompositorCommand) {
        let handle = self.handle.as_ref().expect("compositor running");
        handle.command_tx.send(cmd).expect("send command");
        handle.wake();
        // The command is handled on the compositor's own thread.
        std::thread::sleep(Duration::from_millis(50));
        self.queue.roundtrip(&mut self.app).expect("roundtrip");
    }

    /// Ask the current drag offer for one MIME type and return the reader.
    /// The compositor may not answer until the drop lands: a receive issued
    /// at enter (Chromium's eager fetch) is parked and written when it does.
    fn begin_receive(&mut self, mime_type: &str) -> UnixStream {
        let offer = self
            .app
            .drag_offer
            .clone()
            .expect("a drag offer was entered");
        self.begin_receive_from(&offer, mime_type)
    }

    /// Ask a particular offer for one MIME type.  Old offers can remain
    /// client-side after a re-enter, so tests need to address them directly.
    fn begin_receive_from(
        &mut self,
        offer: &wl_data_offer::WlDataOffer,
        mime_type: &str,
    ) -> UnixStream {
        let (reader, writer) = UnixStream::pair().expect("pipe");
        offer.receive(mime_type.to_string(), writer.as_fd());
        // Flush the request (and the fd with it) before dropping our end,
        // or the compositor gets a socket that is already closed.
        self.conn.flush().expect("flush");
        drop(writer);
        reader
    }

    /// Ask the current drag offer for one MIME type and read what comes back.
    fn receive(&mut self, mime_type: &str) -> Vec<u8> {
        let reader = self.begin_receive(mime_type);
        std::thread::sleep(Duration::from_millis(50));
        read_all(&reader)
    }
}

/// A mime the drop did not carry gets the fd closed empty, so bound the
/// read rather than waiting on an EOF that might never come.
fn read_all(reader: &UnixStream) -> Vec<u8> {
    reader
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let mut buf = Vec::new();
    let _ = (&*reader).read_to_end(&mut buf);
    buf
}

const URI_LIST: &[u8] = b"file:///tmp/blit_drag_1_2/a%20b.png\r\n";
const PNG_BYTES: &[u8] = &[0x89, b'P', b'N', b'G'];

#[test]
fn a_drop_enters_motions_leaves_and_serves_the_uri_list() {
    let mut fx = Fixture::new();
    let surface_id = fx.surface_id;

    fx.send(CompositorCommand::DragEnter {
        surface_id,
        x: 10.0,
        y: 20.0,
        mimes: vec![
            "text/uri-list".to_string(),
            "application/octet-stream".to_string(),
        ],
        planned_uri_list: None,
    });
    assert_eq!(fx.app.events, vec![Drag::Enter { x: 10.0, y: 20.0 }]);
    assert_eq!(
        fx.app.offered,
        vec![
            "text/uri-list".to_string(),
            "application/octet-stream".to_string(),
        ],
        "the ENTER mime list reaches the app unchanged"
    );
    assert!(fx.app.drag_offer.is_some(), "enter carried no offer");
    // Chromium takes its negotiated operation from these events; without
    // them it refuses the drop no matter what else happens.
    assert_eq!(
        fx.app.source_actions,
        vec![1],
        "the offer announces source_actions(Copy) at enter"
    );

    // The client accepts the mime it wants and declares its actions; the
    // compositor answers with the negotiated one.
    let offer = fx.app.drag_offer.clone().unwrap();
    offer.accept(0, Some("text/uri-list".to_string()));
    offer.set_actions(DndAction::Copy, DndAction::Copy);
    fx.conn.flush().expect("flush");
    fx.queue
        .roundtrip(&mut fx.app)
        .expect("set_actions roundtrip");
    assert_eq!(fx.app.actions, vec![1], "set_actions answered action(Copy)");

    fx.send(CompositorCommand::DragMotion {
        surface_id,
        x: 30.0,
        y: 40.0,
    });
    assert_eq!(
        fx.app.events.last(),
        Some(&Drag::Motion { x: 30.0, y: 40.0 })
    );

    // Chromium's eager fetch: a receive issued at enter is not answered
    // empty — it is parked and written the moment the drop lands.
    let early = fx.begin_receive("text/uri-list");
    let early_unknown = fx.begin_receive("image/png");

    fx.send(CompositorCommand::DragDrop {
        surface_id,
        x: 30.0,
        y: 40.0,
        offers: vec![("text/uri-list".to_string(), URI_LIST.to_vec())],
    });
    assert_eq!(
        &fx.app.events[fx.app.events.len() - 2..],
        &[Drag::Drop, Drag::Leave],
        "a successful drop must also clear the destination drag focus"
    );

    // The parked receive now yields the staged bytes; the parked one for a
    // mime the drop did not carry closed empty.
    assert_eq!(read_all(&early), URI_LIST);
    assert_eq!(read_all(&early_unknown), b"");

    // After the drop the offer serves the staged payload — byte for byte —
    // and nothing for a mime the drop did not carry.
    assert_eq!(fx.receive("text/uri-list"), URI_LIST);
    assert_eq!(fx.receive("image/png"), b"");

    // finish + destroy end the session from the client side; both must be
    // accepted without a protocol error.
    offer.finish();
    offer.destroy();
    fx.conn.flush().expect("flush");
    fx.queue.roundtrip(&mut fx.app).expect("finish roundtrip");
}

#[test]
fn a_planned_uri_list_is_served_during_hover() {
    let mut fx = Fixture::new();
    let surface_id = fx.surface_id;

    fx.send(CompositorCommand::DragEnter {
        surface_id,
        x: 10.0,
        y: 20.0,
        mimes: vec!["text/uri-list".to_string()],
        planned_uri_list: Some(URI_LIST.to_vec()),
    });

    // No DROP has happened.  Chromium's enter-time fetch must nevertheless
    // finish now so it can deliver dragenter to the destination page.
    let early = fx.begin_receive("text/uri-list");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(read_all(&early), URI_LIST);

    fx.send(CompositorCommand::DragCancel);
}

#[test]
fn a_cancel_before_the_drop_closes_parked_receives_empty() {
    let mut fx = Fixture::new();
    let surface_id = fx.surface_id;

    fx.send(CompositorCommand::DragEnter {
        surface_id,
        x: 10.0,
        y: 20.0,
        mimes: vec!["text/uri-list".to_string()],
        planned_uri_list: None,
    });
    let early = fx.begin_receive("text/uri-list");

    fx.send(CompositorCommand::DragCancel);
    assert_eq!(fx.app.events.last(), Some(&Drag::Leave));
    // The session ended without a drop: the parked fd closes, an empty read.
    assert_eq!(read_all(&early), b"");
}

#[test]
fn octet_stream_falls_back_to_a_single_dropped_items_bytes() {
    let mut fx = Fixture::new();
    let surface_id = fx.surface_id;

    fx.send(CompositorCommand::DragEnter {
        surface_id,
        x: 10.0,
        y: 20.0,
        mimes: vec![
            "text/uri-list".to_string(),
            "application/octet-stream".to_string(),
        ],
        planned_uri_list: None,
    });
    fx.send(CompositorCommand::DragDrop {
        surface_id,
        x: 10.0,
        y: 20.0,
        // What the server stages for one named item: the uri-list plus the
        // item's own mime with its bytes.
        offers: vec![
            ("text/uri-list".to_string(), URI_LIST.to_vec()),
            ("image/png".to_string(), PNG_BYTES.to_vec()),
        ],
    });
    // The ENTER advertised application/octet-stream; the item's bytes are
    // the only sane thing to serve under it.
    assert_eq!(fx.receive("application/octet-stream"), PNG_BYTES);
    // A uri-list-only drop has no candidate: octet-stream stays empty.
    fx.send(CompositorCommand::DragEnter {
        surface_id,
        x: 10.0,
        y: 20.0,
        mimes: vec![
            "text/uri-list".to_string(),
            "application/octet-stream".to_string(),
        ],
        planned_uri_list: None,
    });
    fx.send(CompositorCommand::DragDrop {
        surface_id,
        x: 10.0,
        y: 20.0,
        offers: vec![("text/uri-list".to_string(), URI_LIST.to_vec())],
    });
    assert_eq!(fx.receive("application/octet-stream"), b"");
}

#[test]
fn a_cancelled_drag_sends_leave_and_a_second_enter_retargets() {
    let mut fx = Fixture::new();
    let surface_id = fx.surface_id;

    fx.send(CompositorCommand::DragEnter {
        surface_id,
        x: 5.0,
        y: 5.0,
        mimes: vec!["text/plain".to_string()],
        planned_uri_list: None,
    });
    assert_eq!(fx.app.events, vec![Drag::Enter { x: 5.0, y: 5.0 }]);

    fx.send(CompositorCommand::DragCancel);
    assert_eq!(fx.app.events.last(), Some(&Drag::Leave));

    // The session is gone: a fresh ENTER starts a new one rather than
    // tripping over the corpse of the cancelled one.
    fx.send(CompositorCommand::DragEnter {
        surface_id,
        x: 6.0,
        y: 6.0,
        mimes: vec!["text/uri-list".to_string()],
        planned_uri_list: None,
    });
    assert_eq!(
        fx.app.events,
        vec![
            Drag::Enter { x: 5.0, y: 5.0 },
            Drag::Leave,
            Drag::Enter { x: 6.0, y: 6.0 },
        ]
    );
    assert_eq!(fx.app.offered, vec!["text/uri-list".to_string()]);

    // DragLeave also ends with a leave event and no dangling session.
    fx.send(CompositorCommand::DragLeave);
    assert_eq!(fx.app.events.last(), Some(&Drag::Leave));
}

#[test]
fn a_reenter_survives_the_old_offers_late_destroy() {
    let mut fx = Fixture::new();
    let surface_id = fx.surface_id;

    fx.send(CompositorCommand::DragEnter {
        surface_id,
        x: 5.0,
        y: 5.0,
        mimes: vec!["text/uri-list".to_string()],
        planned_uri_list: None,
    });
    let first = fx.app.drag_offer.clone().expect("first offer");
    // Chromium's eager fetch, parked on the first session.
    let early = fx.begin_receive("text/uri-list");

    fx.send(CompositorCommand::DragLeave);
    assert_eq!(fx.app.events.last(), Some(&Drag::Leave));
    // The leave ended the first session: its parked receive closed empty.
    assert_eq!(read_all(&early), b"");

    // Re-enter the SAME surface: a fresh session with a fresh offer.
    fx.send(CompositorCommand::DragEnter {
        surface_id,
        x: 6.0,
        y: 6.0,
        mimes: vec!["text/uri-list".to_string()],
        planned_uri_list: Some(URI_LIST.to_vec()),
    });
    assert_eq!(
        fx.app.events,
        vec![
            Drag::Enter { x: 5.0, y: 5.0 },
            Drag::Leave,
            Drag::Enter { x: 6.0, y: 6.0 },
        ]
    );
    let second = fx.app.drag_offer.clone().expect("second offer");
    assert_ne!(first.id(), second.id(), "a re-enter needs a fresh offer");

    // A late receive on the first offer must close empty, not leak the
    // replacement session's planned URI list.
    let late = fx.begin_receive_from(&first, "text/uri-list");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(read_all(&late), b"");

    // The race the session has to survive: the client destroys the FIRST
    // offer only now, after the second enter installed the new session.
    // Touching the new session here would make the drop below vanish.
    first.destroy();
    fx.conn.flush().expect("flush");
    fx.queue.roundtrip(&mut fx.app).expect("destroy roundtrip");

    fx.send(CompositorCommand::DragDrop {
        surface_id,
        x: 6.0,
        y: 6.0,
        offers: vec![("text/uri-list".to_string(), URI_LIST.to_vec())],
    });
    assert_eq!(
        &fx.app.events[fx.app.events.len() - 2..],
        &[Drag::Drop, Drag::Leave],
        "the drop and terminal leave must survive the old offer's late destroy"
    );
    assert_eq!(fx.receive("text/uri-list"), URI_LIST);

    second.finish();
    second.destroy();
    fx.conn.flush().expect("flush");
    fx.queue.roundtrip(&mut fx.app).expect("finish roundtrip");
}
