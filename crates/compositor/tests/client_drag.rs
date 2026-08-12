//! A drag a Wayland app starts itself has to behave like a real drag:
//! enter/motion/leave/drop on whatever app is under the pointer, the
//! transfer spliced through to the source, and `dnd_cancelled` when the
//! drop lands nowhere.
//!
//! The compositor owns the implicit grab from `start_drag` to the button
//! release — pointer input never reaches `wl_pointer` during it.  These
//! tests drive that grab with the same `PointerMotion`/`PointerButton`
//! commands the browser sends, with ordinary Wayland clients as source
//! and target, and pin what each side sees.

#![cfg(target_os = "linux")]

use std::io::Read;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use wayland_client::protocol::{
    wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
    wl_registry, wl_seat, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};

use blit_compositor::{CompositorCommand, CompositorEvent, CompositorHandle, spawn_compositor};

use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// What the source writes when its `send` event arrives.
const PAYLOAD: &[u8] = b"payload from the source";
const CHROMIUM_IMAGE_MIME: &str =
    "application/octet-stream;name=\"screenshot_2026-08-07_at_8.32.34___pm.png\"";
const CHROMIUM_CUSTOM_MIME: &str = "chromium/x-web-custom-data";
const PNG_PAYLOAD: &[u8] = b"\x89PNG\r\n\x1a\nimage bytes from chromium";
const CHROMIUM_CUSTOM_PAYLOAD: &[u8] = b"chromium custom drag metadata";
/// BTN_LEFT, as `CompositorCommand::PointerButton` expects (evdev).
const BTN_LEFT: u32 = 0x110;

/// A `wl_data_device` drag event on the target side.
#[derive(Debug, PartialEq, Clone)]
enum Drag {
    Enter { x: f64, y: f64 },
    Motion { x: f64, y: f64 },
    Leave,
    Drop,
}

/// A `wl_data_source` event on the source side.
#[derive(Debug, PartialEq, Clone)]
enum Src {
    Target(Option<String>),
    Send(String),
    Cancelled,
    DropPerformed,
    Finished,
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
    /// The offer named by the current drag's `enter`.
    drag_offer: Option<wl_data_offer::WlDataOffer>,
    src_events: Vec<Src>,
    offer_source_actions: Option<wl_data_device_manager::DndAction>,
    offer_action: Option<wl_data_device_manager::DndAction>,
    source_action: Option<wl_data_device_manager::DndAction>,
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
                state.offer_source_actions = None;
                state.offer_action = None;
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
        match event {
            wl_data_offer::Event::Offer { mime_type } => state.offered.push(mime_type),
            wl_data_offer::Event::SourceActions { source_actions } => {
                state.offer_source_actions = source_actions.into_result().ok();
            }
            wl_data_offer::Event::Action { dnd_action } => {
                state.offer_action = dnd_action.into_result().ok();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Target { mime_type } => {
                state.src_events.push(Src::Target(mime_type));
            }
            // The transfer: write the payload and close, like a real source.
            wl_data_source::Event::Send { mime_type, fd } => {
                let mut f = std::fs::File::from(fd);
                let payload = match mime_type.as_str() {
                    CHROMIUM_IMAGE_MIME => PNG_PAYLOAD,
                    CHROMIUM_CUSTOM_MIME => CHROMIUM_CUSTOM_PAYLOAD,
                    _ => PAYLOAD,
                };
                let _ = f.write_all(payload);
                state.src_events.push(Src::Send(mime_type));
            }
            wl_data_source::Event::Cancelled => state.src_events.push(Src::Cancelled),
            wl_data_source::Event::DndDropPerformed => state.src_events.push(Src::DropPerformed),
            wl_data_source::Event::DndFinished => state.src_events.push(Src::Finished),
            wl_data_source::Event::Action { dnd_action } => {
                state.source_action = dnd_action.into_result().ok();
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
        Self {
            handle: Some(handle),
        }
    }

    fn socket(&self) -> &str {
        &self
            .handle
            .as_ref()
            .expect("compositor running")
            .socket_name
    }

    /// Send a compositor command — the same commands the server forwards
    /// from the browser — and give the compositor thread a moment.
    fn send(&self, cmd: CompositorCommand) {
        let handle = self.handle.as_ref().expect("compositor running");
        handle.command_tx.send(cmd).expect("send command");
        handle.wake();
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One Wayland client connection: a data device (target side and the
/// handle `start_drag` is called on) plus any toplevels it maps.
struct TestClient {
    app: App,
    queue: EventQueue<App>,
    conn: Connection,
    device: wl_data_device::WlDataDevice,
    /// Kept alive so the server-side surfaces outlive the test.
    toplevels: Vec<(
        wl_surface::WlSurface,
        xdg_surface::XdgSurface,
        xdg_toplevel::XdgToplevel,
    )>,
}

impl TestClient {
    fn connect(fx: &Fixture) -> Self {
        let stream = UnixStream::connect(fx.socket()).expect("connect to compositor socket");
        let conn = Connection::from_socket(stream).expect("wayland connection");
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());

        let mut app = App::default();
        queue.roundtrip(&mut app).expect("registry roundtrip");
        let ddm = app.ddm.clone().expect("wl_data_device_manager advertised");
        let seat = app.seat.clone().expect("wl_seat advertised");
        let device = ddm.get_data_device(&seat, &qh, ());
        Self {
            app,
            queue,
            conn,
            device,
            toplevels: Vec::new(),
        }
    }

    /// Map a toplevel and return the compositor's surface id for it.
    fn new_toplevel(&mut self, fx: &Fixture) -> u16 {
        let qh = self.queue.handle();
        let compositor = self.app.compositor.clone().expect("wl_compositor bound");
        let wm_base = self.app.wm_base.clone().expect("xdg_wm_base bound");
        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        self.queue.roundtrip(&mut self.app).expect("map roundtrip");
        self.toplevels.push((surface, xdg_surface, toplevel));

        let handle = fx.handle.as_ref().expect("compositor running");
        loop {
            match handle.event_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(CompositorEvent::SurfaceCreated { surface_id, .. }) => return surface_id,
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => panic!("no SurfaceCreated within 5s"),
                Err(e) => panic!("compositor event channel closed: {e}"),
            }
        }
    }

    fn roundtrip(&mut self) {
        self.conn.flush().expect("flush");
        self.queue.roundtrip(&mut self.app).expect("roundtrip");
    }

    /// Offer `mimes` on a fresh data source and start the drag from this
    /// client's (first) toplevel surface.
    fn start_drag(&mut self, mimes: &[&str]) -> wl_data_source::WlDataSource {
        let qh = self.queue.handle();
        let ddm = self.app.ddm.clone().expect("ddm bound");
        let source = ddm.create_data_source(&qh, ());
        for mime in mimes {
            source.offer(mime.to_string());
        }
        source.set_actions(wl_data_device_manager::DndAction::Copy);
        let surface = self.toplevels[0].0.clone();
        self.device.start_drag(Some(&source), &surface, None, 0);
        // Deliver offer + start_drag before the caller drives the pointer.
        self.roundtrip();
        source
    }

    fn start_drag_with_actions(
        &mut self,
        mimes: &[&str],
        actions: wl_data_device_manager::DndAction,
    ) -> wl_data_source::WlDataSource {
        let qh = self.queue.handle();
        let ddm = self.app.ddm.clone().expect("ddm bound");
        let source = ddm.create_data_source(&qh, ());
        for mime in mimes {
            source.offer(mime.to_string());
        }
        // The protocol requires this before start_drag.  The compositor has
        // to retain it on the source until the drag session is constructed.
        source.set_actions(actions);
        let surface = self.toplevels[0].0.clone();
        self.device.start_drag(Some(&source), &surface, None, 0);
        self.roundtrip();
        source
    }
}

/// The target asks the current drag offer for `mime`; the source answers
/// when it next dispatches events.
fn target_receive(tgt: &mut TestClient, mime: &str) -> UnixStream {
    let offer = tgt
        .app
        .drag_offer
        .clone()
        .expect("a drag offer was entered");
    let (reader, writer) = UnixStream::pair().expect("pipe");
    offer.receive(mime.to_string(), writer.as_fd());
    tgt.conn.flush().expect("flush");
    drop(writer);
    reader
}

fn read_all(reader: &UnixStream) -> Vec<u8> {
    reader
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let mut buf = Vec::new();
    let _ = (&*reader).read_to_end(&mut buf);
    buf
}

#[test]
fn a_client_drag_delivers_enter_motion_drop_and_the_sources_bytes() {
    let fx = Fixture::new();
    let mut src = TestClient::connect(&fx);
    let _src_sid = src.new_toplevel(&fx);
    let mut tgt = TestClient::connect(&fx);
    let tgt_sid = tgt.new_toplevel(&fx);

    let _source = src.start_drag(&["text/plain", "application/octet-stream"]);

    // The pointer (still held by the browser) crosses onto the target.
    fx.send(CompositorCommand::PointerMotion {
        surface_id: tgt_sid,
        x: 10.0,
        y: 20.0,
        time_ms: 0,
    });
    tgt.roundtrip();
    assert_eq!(tgt.app.events, vec![Drag::Enter { x: 10.0, y: 20.0 }]);
    assert_eq!(
        tgt.app.offered,
        vec![
            "text/plain".to_string(),
            "application/octet-stream".to_string(),
        ],
        "the target sees the source's mime list, not the compositor's"
    );
    let offer = tgt.app.drag_offer.clone().expect("enter carried an offer");
    assert_eq!(
        tgt.app.offer_source_actions,
        Some(wl_data_device_manager::DndAction::Copy),
        "source_actions precedes destination action negotiation"
    );
    assert_eq!(tgt.app.offer_action, None, "action waits for set_actions");
    offer.set_actions(
        wl_data_device_manager::DndAction::Copy,
        wl_data_device_manager::DndAction::Copy,
    );
    offer.accept(0, Some("text/plain".to_string()));
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(
        tgt.app.offer_action,
        Some(wl_data_device_manager::DndAction::Copy)
    );
    assert_eq!(
        src.app.source_action,
        Some(wl_data_device_manager::DndAction::Copy)
    );
    assert_eq!(
        src.app.src_events,
        vec![Src::Target(Some("text/plain".to_string()))],
        "the destination's accepted MIME is feedback to the source"
    );

    fx.send(CompositorCommand::PointerMotion {
        surface_id: tgt_sid,
        x: 30.0,
        y: 40.0,
        time_ms: 0,
    });
    tgt.roundtrip();
    assert_eq!(
        tgt.app.events.last(),
        Some(&Drag::Motion { x: 30.0, y: 40.0 })
    );

    // The button goes up: drop on the target, drop_performed at the source,
    // and no wl_pointer.button anywhere (the grab swallows it).
    fx.send(CompositorCommand::PointerButton {
        surface_id: tgt_sid,
        button: BTN_LEFT,
        pressed: false,
        time_ms: 0,
    });
    tgt.roundtrip();
    assert_eq!(tgt.app.events.last(), Some(&Drag::Drop));
    src.roundtrip();
    assert_eq!(
        src.app.src_events,
        vec![
            Src::Target(Some("text/plain".to_string())),
            Src::DropPerformed,
        ]
    );

    // The transfer itself is spliced through to the source.
    let reader = target_receive(&mut tgt, "text/plain");
    std::thread::sleep(Duration::from_millis(50));
    src.roundtrip(); // handles Send, writes the payload
    assert_eq!(read_all(&reader), PAYLOAD);
    assert_eq!(
        src.app.src_events,
        vec![
            Src::Target(Some("text/plain".to_string())),
            Src::DropPerformed,
            Src::Send("text/plain".to_string()),
        ]
    );

    // finish completes the drag at the source.
    offer.finish();
    offer.destroy();
    tgt.roundtrip();
    std::thread::sleep(Duration::from_millis(50));
    src.roundtrip();
    assert_eq!(
        src.app.src_events,
        vec![
            Src::Target(Some("text/plain".to_string())),
            Src::DropPerformed,
            Src::Send("text/plain".to_string()),
            Src::Finished,
        ]
    );
}

#[test]
fn source_actions_declared_before_start_drag_are_preserved() {
    use wl_data_device_manager::DndAction;

    let fx = Fixture::new();
    let mut src = TestClient::connect(&fx);
    let _src_sid = src.new_toplevel(&fx);
    let mut tgt = TestClient::connect(&fx);
    let tgt_sid = tgt.new_toplevel(&fx);

    let _source = src.start_drag_with_actions(&["text/plain"], DndAction::Move);
    fx.send(CompositorCommand::PointerMotion {
        surface_id: tgt_sid,
        x: 10.0,
        y: 20.0,
        time_ms: 0,
    });
    tgt.roundtrip();
    src.roundtrip();

    assert_eq!(tgt.app.offer_source_actions, Some(DndAction::Move));
    assert_eq!(tgt.app.offer_action, None, "action waits for set_actions");
    let offer = tgt.app.drag_offer.clone().expect("enter carried an offer");
    offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Move);
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(tgt.app.offer_action, Some(DndAction::Move));
    assert_eq!(src.app.source_action, Some(DndAction::Move));
}

#[test]
fn unsupported_destination_preference_falls_back_to_a_common_action() {
    use wl_data_device_manager::DndAction;

    let fx = Fixture::new();
    let mut src = TestClient::connect(&fx);
    let _src_sid = src.new_toplevel(&fx);
    let mut tgt = TestClient::connect(&fx);
    let tgt_sid = tgt.new_toplevel(&fx);

    let _source = src.start_drag(&["text/plain"]);
    fx.send(CompositorCommand::PointerMotion {
        surface_id: tgt_sid,
        x: 10.0,
        y: 20.0,
        time_ms: 0,
    });
    tgt.roundtrip();

    let offer = tgt.app.drag_offer.clone().expect("enter carried an offer");
    offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Move);
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(
        tgt.app.offer_action,
        Some(DndAction::Copy),
        "the source only offers Copy, so Move preference cannot win"
    );
    assert_eq!(src.app.source_action, Some(DndAction::Copy));
}

#[test]
fn explicit_source_action_none_is_not_rewritten_to_copy() {
    use wl_data_device_manager::DndAction;

    let fx = Fixture::new();
    let mut src = TestClient::connect(&fx);
    let _src_sid = src.new_toplevel(&fx);
    let mut tgt = TestClient::connect(&fx);
    let tgt_sid = tgt.new_toplevel(&fx);

    let _source = src.start_drag_with_actions(&["text/plain"], DndAction::empty());
    fx.send(CompositorCommand::PointerMotion {
        surface_id: tgt_sid,
        x: 10.0,
        y: 20.0,
        time_ms: 0,
    });
    tgt.roundtrip();
    assert_eq!(tgt.app.offer_source_actions, Some(DndAction::empty()));

    let offer = tgt.app.drag_offer.clone().expect("enter carried an offer");
    offer.set_actions(DndAction::Copy, DndAction::empty());
    offer.accept(0, Some("text/plain".to_string()));
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(tgt.app.offer_action, Some(DndAction::empty()));
    assert_eq!(src.app.source_action, Some(DndAction::empty()));

    fx.send(CompositorCommand::PointerButton {
        surface_id: tgt_sid,
        button: BTN_LEFT,
        pressed: false,
        time_ms: 0,
    });
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(tgt.app.events.last(), Some(&Drag::Leave));
    assert_eq!(src.app.src_events.last(), Some(&Src::Cancelled));
}

#[test]
fn chromium_image_drag_can_fetch_custom_metadata_and_the_named_image_stream() {
    use wl_data_device_manager::DndAction;

    let fx = Fixture::new();
    let mut src = TestClient::connect(&fx);
    let _src_sid = src.new_toplevel(&fx);
    let mut tgt = TestClient::connect(&fx);
    let tgt_sid = tgt.new_toplevel(&fx);

    let mimes = [
        "text/x-moz-url",
        "text/html",
        "text/plain;charset=utf-8",
        "text/plain",
        CHROMIUM_IMAGE_MIME,
        CHROMIUM_CUSTOM_MIME,
    ];
    let _source = src.start_drag(&mimes);

    fx.send(CompositorCommand::PointerMotion {
        surface_id: tgt_sid,
        x: 12.0,
        y: 34.0,
        time_ms: 0,
    });
    tgt.roundtrip();
    assert_eq!(tgt.app.events, vec![Drag::Enter { x: 12.0, y: 34.0 }]);
    assert_eq!(
        tgt.app.offered,
        mimes.map(str::to_string),
        "Chromium's parameterized image MIME must survive the offer unchanged"
    );
    assert_eq!(tgt.app.offer_source_actions, Some(DndAction::Copy));
    assert_eq!(tgt.app.offer_action, None, "action waits for set_actions");

    let offer = tgt.app.drag_offer.clone().expect("enter carried an offer");
    offer.set_actions(DndAction::Copy, DndAction::Copy);
    // Chromium targets can accept their metadata type while fetching the
    // separately advertised named octet stream that contains the image.
    offer.accept(0, Some(CHROMIUM_CUSTOM_MIME.to_string()));
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(tgt.app.offer_action, Some(DndAction::Copy));
    assert_eq!(src.app.source_action, Some(DndAction::Copy));
    assert_eq!(
        src.app.src_events,
        vec![Src::Target(Some(CHROMIUM_CUSTOM_MIME.to_string()))]
    );

    // Chromium is allowed to request multiple offered types before drop.
    // Each receive must retain its own MIME and fd all the way to the source.
    let custom_reader = target_receive(&mut tgt, CHROMIUM_CUSTOM_MIME);
    let image_reader = target_receive(&mut tgt, CHROMIUM_IMAGE_MIME);
    std::thread::sleep(Duration::from_millis(50));
    src.roundtrip();
    assert_eq!(read_all(&custom_reader), CHROMIUM_CUSTOM_PAYLOAD);
    assert_eq!(read_all(&image_reader), PNG_PAYLOAD);
    assert_eq!(
        src.app.src_events,
        vec![
            Src::Target(Some(CHROMIUM_CUSTOM_MIME.to_string())),
            Src::Send(CHROMIUM_CUSTOM_MIME.to_string()),
            Src::Send(CHROMIUM_IMAGE_MIME.to_string()),
        ]
    );

    fx.send(CompositorCommand::PointerButton {
        surface_id: tgt_sid,
        button: BTN_LEFT,
        pressed: false,
        time_ms: 0,
    });
    tgt.roundtrip();
    assert_eq!(tgt.app.events.last(), Some(&Drag::Drop));
    src.roundtrip();
    assert_eq!(src.app.src_events.last(), Some(&Src::DropPerformed));

    offer.finish();
    offer.destroy();
    tgt.roundtrip();
    std::thread::sleep(Duration::from_millis(50));
    src.roundtrip();
    assert_eq!(src.app.src_events.last(), Some(&Src::Finished));
}

#[test]
fn a_drop_nowhere_cancels_at_the_source() {
    let fx = Fixture::new();
    let mut src = TestClient::connect(&fx);
    let _src_sid = src.new_toplevel(&fx);
    let mut tgt = TestClient::connect(&fx);
    let _tgt_sid = tgt.new_toplevel(&fx);

    let _source = src.start_drag(&["text/plain"]);

    // Motion over no surface at all (an id nothing is mapped to), then the
    // release.
    fx.send(CompositorCommand::PointerMotion {
        surface_id: 65000,
        x: 5.0,
        y: 5.0,
        time_ms: 0,
    });
    fx.send(CompositorCommand::PointerButton {
        surface_id: 65000,
        button: BTN_LEFT,
        pressed: false,
        time_ms: 0,
    });
    src.roundtrip();
    tgt.roundtrip();
    assert_eq!(src.app.src_events, vec![Src::Cancelled]);
    assert!(
        tgt.app.events.is_empty(),
        "a cancelled drag never entered anywhere"
    );
}

#[test]
fn crossing_between_surfaces_leaves_the_first_and_enters_the_second() {
    use wl_data_device_manager::DndAction;

    let fx = Fixture::new();
    let mut src = TestClient::connect(&fx);
    let _src_sid = src.new_toplevel(&fx);
    let mut tgt = TestClient::connect(&fx);
    let first_sid = tgt.new_toplevel(&fx);
    let second_sid = tgt.new_toplevel(&fx);

    let _source = src.start_drag(&["text/plain"]);

    fx.send(CompositorCommand::PointerMotion {
        surface_id: first_sid,
        x: 1.0,
        y: 1.0,
        time_ms: 0,
    });
    tgt.roundtrip();
    assert_eq!(tgt.app.events, vec![Drag::Enter { x: 1.0, y: 1.0 }]);

    fx.send(CompositorCommand::PointerMotion {
        surface_id: second_sid,
        x: 2.0,
        y: 2.0,
        time_ms: 0,
    });
    tgt.roundtrip();
    assert_eq!(
        tgt.app.events,
        vec![
            Drag::Enter { x: 1.0, y: 1.0 },
            Drag::Leave,
            Drag::Enter { x: 2.0, y: 2.0 },
        ],
        "leave precedes the new enter"
    );
    // The new enter advertised the source's mimes on a fresh offer.
    assert_eq!(tgt.app.offered, vec!["text/plain".to_string()]);

    let offer = tgt
        .app
        .drag_offer
        .clone()
        .expect("second enter carried an offer");
    offer.set_actions(DndAction::Copy, DndAction::Copy);
    offer.accept(0, Some("text/plain".to_string()));
    tgt.roundtrip();
    src.roundtrip();

    // Releasing over the second surface drops there.
    fx.send(CompositorCommand::PointerButton {
        surface_id: second_sid,
        button: BTN_LEFT,
        pressed: false,
        time_ms: 0,
    });
    tgt.roundtrip();
    assert_eq!(tgt.app.events.last(), Some(&Drag::Drop));
    src.roundtrip();
    assert_eq!(
        src.app.src_events,
        vec![
            Src::Target(None),
            Src::Target(Some("text/plain".to_string())),
            Src::DropPerformed,
        ]
    );
}

#[test]
fn a_drag_onto_the_same_app_still_works() {
    use wl_data_device_manager::DndAction;

    let fx = Fixture::new();
    let mut app = TestClient::connect(&fx);
    let sid = app.new_toplevel(&fx);

    let _source = app.start_drag(&["text/plain"]);

    fx.send(CompositorCommand::PointerMotion {
        surface_id: sid,
        x: 8.0,
        y: 8.0,
        time_ms: 0,
    });
    app.roundtrip();
    assert_eq!(app.app.events, vec![Drag::Enter { x: 8.0, y: 8.0 }]);

    let offer = app.app.drag_offer.clone().expect("enter carried an offer");
    offer.set_actions(DndAction::Copy, DndAction::Copy);
    offer.accept(0, Some("text/plain".to_string()));
    app.roundtrip();

    fx.send(CompositorCommand::PointerButton {
        surface_id: sid,
        button: BTN_LEFT,
        pressed: false,
        time_ms: 0,
    });
    app.roundtrip();
    assert_eq!(
        app.app.events,
        vec![Drag::Enter { x: 8.0, y: 8.0 }, Drag::Drop]
    );
    assert_eq!(
        app.app.src_events,
        vec![
            Src::Target(Some("text/plain".to_string())),
            Src::DropPerformed,
        ]
    );

    let reader = target_receive(&mut app, "text/plain");
    std::thread::sleep(Duration::from_millis(50));
    app.roundtrip(); // handles Send, writes the payload
    assert_eq!(read_all(&reader), PAYLOAD);
}

#[test]
fn release_without_an_accepted_mime_cancels_instead_of_dropping() {
    use wl_data_device_manager::DndAction;

    let fx = Fixture::new();
    let mut src = TestClient::connect(&fx);
    let _src_sid = src.new_toplevel(&fx);
    let mut tgt = TestClient::connect(&fx);
    let tgt_sid = tgt.new_toplevel(&fx);

    let _source = src.start_drag(&["text/plain"]);
    fx.send(CompositorCommand::PointerMotion {
        surface_id: tgt_sid,
        x: 3.0,
        y: 5.0,
        time_ms: 0,
    });
    tgt.roundtrip();
    let offer = tgt.app.drag_offer.clone().expect("enter carried an offer");
    offer.set_actions(DndAction::Copy, DndAction::Copy);
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(tgt.app.offer_action, Some(DndAction::Copy));

    fx.send(CompositorCommand::PointerButton {
        surface_id: tgt_sid,
        button: BTN_LEFT,
        pressed: false,
        time_ms: 0,
    });
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(
        tgt.app.events,
        vec![Drag::Enter { x: 3.0, y: 5.0 }, Drag::Leave]
    );
    assert_eq!(
        src.app.src_events,
        vec![Src::Target(None), Src::Cancelled],
        "an action alone is not a valid v3 drop"
    );
    assert!(!src.app.src_events.contains(&Src::DropPerformed));
}

#[test]
fn release_with_none_action_cancels_instead_of_dropping() {
    use wl_data_device_manager::DndAction;

    let fx = Fixture::new();
    let mut src = TestClient::connect(&fx);
    let _src_sid = src.new_toplevel(&fx);
    let mut tgt = TestClient::connect(&fx);
    let tgt_sid = tgt.new_toplevel(&fx);

    let _source = src.start_drag(&["text/plain"]);
    fx.send(CompositorCommand::PointerMotion {
        surface_id: tgt_sid,
        x: 7.0,
        y: 9.0,
        time_ms: 0,
    });
    tgt.roundtrip();
    let offer = tgt.app.drag_offer.clone().expect("enter carried an offer");
    // The source offers only Copy, so a Move-only destination has no
    // intersection and must receive action(NONE).
    offer.set_actions(DndAction::Move, DndAction::empty());
    offer.accept(0, Some("text/plain".to_string()));
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(tgt.app.offer_action, Some(DndAction::empty()));
    assert_eq!(src.app.source_action, Some(DndAction::empty()));

    fx.send(CompositorCommand::PointerButton {
        surface_id: tgt_sid,
        button: BTN_LEFT,
        pressed: false,
        time_ms: 0,
    });
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(
        tgt.app.events,
        vec![Drag::Enter { x: 7.0, y: 9.0 }, Drag::Leave]
    );
    assert_eq!(
        src.app.src_events,
        vec![
            Src::Target(Some("text/plain".to_string())),
            Src::Target(None),
            Src::Cancelled,
        ],
        "MIME acceptance cannot turn action(NONE) into a drop"
    );
    assert!(!src.app.src_events.contains(&Src::DropPerformed));
}

#[test]
fn none_action_can_be_renegotiated_on_the_same_entered_offer() {
    use wl_data_device_manager::DndAction;

    let fx = Fixture::new();
    let mut src = TestClient::connect(&fx);
    let _src_sid = src.new_toplevel(&fx);
    let mut tgt = TestClient::connect(&fx);
    let tgt_sid = tgt.new_toplevel(&fx);

    let _source = src.start_drag(&["text/plain"]);
    fx.send(CompositorCommand::PointerMotion {
        surface_id: tgt_sid,
        x: 11.0,
        y: 13.0,
        time_ms: 0,
    });
    tgt.roundtrip();
    let offer = tgt.app.drag_offer.clone().expect("enter carried an offer");
    let offer_id = offer.id();

    offer.set_actions(DndAction::Move, DndAction::empty());
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(tgt.app.offer_action, Some(DndAction::empty()));
    assert_eq!(src.app.source_action, Some(DndAction::empty()));
    assert_eq!(tgt.app.events, vec![Drag::Enter { x: 11.0, y: 13.0 }]);
    assert_eq!(
        tgt.app.drag_offer.as_ref().map(Proxy::id),
        Some(offer_id.clone()),
        "action(NONE) keeps pointer focus and the current offer alive"
    );

    offer.set_actions(DndAction::Copy, DndAction::Copy);
    offer.accept(0, Some("text/plain".to_string()));
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(tgt.app.offer_action, Some(DndAction::Copy));
    assert_eq!(src.app.source_action, Some(DndAction::Copy));
    assert_eq!(tgt.app.events, vec![Drag::Enter { x: 11.0, y: 13.0 }]);
    assert_eq!(tgt.app.drag_offer.as_ref().map(Proxy::id), Some(offer_id));

    fx.send(CompositorCommand::PointerButton {
        surface_id: tgt_sid,
        button: BTN_LEFT,
        pressed: false,
        time_ms: 0,
    });
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(tgt.app.events.last(), Some(&Drag::Drop));
    assert_eq!(src.app.src_events.last(), Some(&Src::DropPerformed));
}

#[test]
fn destroying_the_active_offer_leaves_and_cancels_the_drag() {
    let fx = Fixture::new();
    let mut src = TestClient::connect(&fx);
    let _src_sid = src.new_toplevel(&fx);
    let mut tgt = TestClient::connect(&fx);
    let tgt_sid = tgt.new_toplevel(&fx);

    let _source = src.start_drag(&["text/plain"]);
    fx.send(CompositorCommand::PointerMotion {
        surface_id: tgt_sid,
        x: 2.0,
        y: 3.0,
        time_ms: 0,
    });
    tgt.roundtrip();
    let offer = tgt.app.drag_offer.clone().expect("enter carried an offer");

    offer.destroy();
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(
        tgt.app.events,
        vec![Drag::Enter { x: 2.0, y: 3.0 }, Drag::Leave]
    );
    assert_eq!(src.app.src_events, vec![Src::Target(None), Src::Cancelled]);
}

#[test]
fn destroying_the_target_surface_cannot_drop_on_stale_acceptance() {
    use wl_data_device_manager::DndAction;

    let fx = Fixture::new();
    let mut src = TestClient::connect(&fx);
    let _src_sid = src.new_toplevel(&fx);
    let mut tgt = TestClient::connect(&fx);
    let tgt_sid = tgt.new_toplevel(&fx);

    let _source = src.start_drag(&["text/plain"]);
    fx.send(CompositorCommand::PointerMotion {
        surface_id: tgt_sid,
        x: 4.0,
        y: 6.0,
        time_ms: 0,
    });
    tgt.roundtrip();
    let offer = tgt.app.drag_offer.clone().expect("enter carried an offer");
    offer.set_actions(DndAction::Copy, DndAction::Copy);
    offer.accept(0, Some("text/plain".to_string()));
    tgt.roundtrip();
    src.roundtrip();

    let (surface, xdg_surface, toplevel) = tgt.toplevels.pop().expect("target toplevel");
    toplevel.destroy();
    xdg_surface.destroy();
    surface.destroy();
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(
        tgt.app.events,
        vec![Drag::Enter { x: 4.0, y: 6.0 }, Drag::Leave]
    );

    fx.send(CompositorCommand::PointerButton {
        surface_id: tgt_sid,
        button: BTN_LEFT,
        pressed: false,
        time_ms: 0,
    });
    tgt.roundtrip();
    src.roundtrip();
    assert!(!tgt.app.events.contains(&Drag::Drop));
    assert_eq!(src.app.src_events.last(), Some(&Src::Cancelled));
    assert!(!src.app.src_events.contains(&Src::DropPerformed));
}

#[test]
fn destroying_the_source_mid_drag_leaves_the_target_and_ends_the_session() {
    let fx = Fixture::new();
    let mut src = TestClient::connect(&fx);
    let _src_sid = src.new_toplevel(&fx);
    let mut tgt = TestClient::connect(&fx);
    let tgt_sid = tgt.new_toplevel(&fx);

    let source = src.start_drag(&["text/plain"]);
    fx.send(CompositorCommand::PointerMotion {
        surface_id: tgt_sid,
        x: 4.0,
        y: 4.0,
        time_ms: 0,
    });
    tgt.roundtrip();
    assert_eq!(tgt.app.events, vec![Drag::Enter { x: 4.0, y: 4.0 }]);

    // The source goes away mid-drag: the target hears the leave, and the
    // source hears nothing (it is gone).
    source.destroy();
    src.roundtrip();
    std::thread::sleep(Duration::from_millis(50));
    tgt.roundtrip();
    assert_eq!(
        tgt.app.events,
        vec![Drag::Enter { x: 4.0, y: 4.0 }, Drag::Leave]
    );
    assert!(
        src.app.src_events.is_empty(),
        "no cancelled for a dead source"
    );

    // The release afterwards is not a drop: no session is left to complete.
    fx.send(CompositorCommand::PointerButton {
        surface_id: tgt_sid,
        button: BTN_LEFT,
        pressed: false,
        time_ms: 0,
    });
    tgt.roundtrip();
    src.roundtrip();
    assert_eq!(
        tgt.app.events,
        vec![Drag::Enter { x: 4.0, y: 4.0 }, Drag::Leave]
    );
    assert!(src.app.src_events.is_empty());
}
