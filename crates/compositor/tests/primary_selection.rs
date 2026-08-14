//! Middle-click paste is between two Wayland clients or it is nothing.
//!
//! The clipboard gets away without compositor-side plumbing: a selection is
//! read out to the browser as text and comes back as an external offer when
//! the user pastes, so the round trip carries it from one app to another.
//! PRIMARY has no such detour -- the web platform exposes no primary
//! selection, so there is nothing to bounce off.  Unless the compositor
//! hands the source to the other client itself, a middle click lands on a
//! selection that exists and cannot be reached, which from the app's side is
//! indistinguishable from no selection at all.
//!
//! These tests run two independent connections, because one client is
//! exactly the case that cannot fail: the owner does not need to be told
//! about its own selection.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1 as pdm, zwp_primary_selection_device_v1 as pdev,
    zwp_primary_selection_offer_v1 as poffer, zwp_primary_selection_source_v1 as psrc,
};

use blit_compositor::{CompositorHandle, spawn_compositor_without_renderer};

const TEXT: &str = "selected in one app";
const MIME: &str = "text/plain;charset=utf-8";

#[derive(Default)]
struct App {
    mgr: Option<pdm::ZwpPrimarySelectionDeviceManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    /// MIME types advertised on the most recent offer, in arrival order.
    offered: Vec<String>,
    /// The offer the compositor last named as the selection, and whether it
    /// has ever named one.  `None` after an explicit clear, which is a
    /// different thing from never having been told.
    selection: Option<poffer::ZwpPrimarySelectionOfferV1>,
    selection_events: usize,
    /// What this client serves when asked for its own source.
    serves: Option<Vec<u8>>,
    /// Set when the compositor tells us our source was displaced.
    cancelled: bool,
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
            "zwp_primary_selection_device_manager_v1" => {
                state.mgr = Some(
                    registry.bind::<pdm::ZwpPrimarySelectionDeviceManagerV1, _, _>(name, 1, qh, ()),
                );
            }
            "wl_seat" => {
                state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, 7, qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<pdev::ZwpPrimarySelectionDeviceV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &pdev::ZwpPrimarySelectionDeviceV1,
        event: pdev::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // A fresh offer supersedes whatever was advertised before it.
            pdev::Event::DataOffer { .. } => state.offered.clear(),
            pdev::Event::Selection { id } => {
                state.selection = id;
                state.selection_events += 1;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(App, pdev::ZwpPrimarySelectionDeviceV1, [
        pdev::EVT_DATA_OFFER_OPCODE => (poffer::ZwpPrimarySelectionOfferV1, ()),
    ]);
}

impl Dispatch<poffer::ZwpPrimarySelectionOfferV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &poffer::ZwpPrimarySelectionOfferV1,
        event: poffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let poffer::Event::Offer { mime_type } = event {
            state.offered.push(mime_type);
        }
    }
}

impl Dispatch<psrc::ZwpPrimarySelectionSourceV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &psrc::ZwpPrimarySelectionSourceV1,
        event: psrc::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // The owner writes the bytes itself and closes its end; the
            // compositor is a matchmaker here, never a buffer.
            psrc::Event::Send { fd, .. } => {
                if let Some(ref bytes) = state.serves {
                    let mut f = std::fs::File::from(fd);
                    let _ = f.write_all(bytes);
                }
            }
            psrc::Event::Cancelled => state.cancelled = true,
            _ => {}
        }
    }
}

delegate_noop!(App: ignore wl_seat::WlSeat);
delegate_noop!(App: ignore pdm::ZwpPrimarySelectionDeviceManagerV1);

/// One client: its own connection, queue and primary-selection device.
struct Client {
    app: App,
    queue: EventQueue<App>,
    conn: Connection,
    qh: QueueHandle<App>,
    mgr: pdm::ZwpPrimarySelectionDeviceManagerV1,
    seat: wl_seat::WlSeat,
    device: Option<pdev::ZwpPrimarySelectionDeviceV1>,
}

impl Client {
    /// Connect and bind, without yet asking for a device.
    fn connect(socket: &str) -> Self {
        let stream = UnixStream::connect(socket).expect("connect to compositor socket");
        let conn = Connection::from_socket(stream).expect("wayland connection");
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());

        let mut app = App::default();
        queue.roundtrip(&mut app).expect("registry roundtrip");
        let mgr = app
            .mgr
            .clone()
            .expect("zwp_primary_selection_device_manager_v1 advertised");
        let seat = app.seat.clone().expect("wl_seat advertised");

        Self {
            app,
            queue,
            conn,
            qh,
            mgr,
            seat,
            device: None,
        }
    }

    fn get_device(&mut self) {
        let device = self.mgr.get_device(&self.seat, &self.qh, ());
        self.device = Some(device);
        self.settle();
    }

    /// Take ownership of the primary selection, serving `bytes` for `MIME`.
    fn own_selection(&mut self, bytes: &str) -> psrc::ZwpPrimarySelectionSourceV1 {
        self.app.serves = Some(bytes.as_bytes().to_vec());
        let source = self.mgr.create_source(&self.qh, ());
        source.offer(MIME.to_string());
        let device = self.device.as_ref().expect("device bound");
        device.set_selection(Some(&source), 1);
        self.settle();
        source
    }

    fn clear_selection(&mut self) {
        let device = self.device.as_ref().expect("device bound");
        device.set_selection(None, 2);
        self.settle();
    }

    /// Flush our requests, let the compositor's own thread act, read back.
    fn settle(&mut self) {
        self.conn.flush().expect("flush");
        std::thread::sleep(Duration::from_millis(50));
        self.queue.roundtrip(&mut self.app).expect("roundtrip");
    }
}

struct Fixture {
    handle: Option<CompositorHandle>,
    socket: String,
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
        let socket = handle.socket_name.clone();
        Self {
            handle: Some(handle),
            socket,
        }
    }

    fn client(&self) -> Client {
        Client::connect(&self.socket)
    }
}

/// Ask `paster`'s current offer for `MIME` and read what the *owner* writes.
///
/// The pipe has an end in each client, so both queues have to turn: the
/// reader's request has to reach the compositor, and the owner has to be
/// dispatched to serve it and drop its end before the read sees EOF.
fn paste(paster: &mut Client, owner: &mut Client) -> Vec<u8> {
    let offer = paster
        .app
        .selection
        .clone()
        .expect("a primary selection was offered");
    let (mut reader, writer) = UnixStream::pair().expect("pipe");
    offer.receive(MIME.to_string(), writer.as_fd());
    // Flush the request (and the fd with it) before dropping our end, or
    // the compositor gets a socket that is already closed.
    paster.conn.flush().expect("flush");
    drop(writer);
    std::thread::sleep(Duration::from_millis(50));
    owner.queue.roundtrip(&mut owner.app).expect("owner serves");

    // A type the owner will not serve leaves the fd closed empty, so bound
    // the read rather than waiting on an EOF that may never come.
    reader
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    buf
}

#[test]
fn a_selection_reaches_the_other_client() {
    let fx = Fixture::new();
    let mut owner = fx.client();
    let mut paster = fx.client();
    owner.get_device();
    paster.get_device();

    owner.own_selection(TEXT);
    paster.settle();

    assert_eq!(
        paster.app.offered,
        vec![MIME.to_string()],
        "the paster should be offered exactly what the owner advertised"
    );
    assert_eq!(paste(&mut paster, &mut owner), TEXT.as_bytes());
}

#[test]
fn a_client_that_binds_late_still_gets_the_selection() {
    let fx = Fixture::new();
    let mut owner = fx.client();
    owner.get_device();
    owner.own_selection(TEXT);

    // Nothing re-offers on focus, so a client starting after the copy would
    // otherwise never learn a selection exists.
    let mut paster = fx.client();
    paster.get_device();

    assert!(
        paster.app.selection.is_some(),
        "binding a device should surface the standing selection"
    );
    assert_eq!(paste(&mut paster, &mut owner), TEXT.as_bytes());
}

#[test]
fn a_new_owner_displaces_the_old_one() {
    let fx = Fixture::new();
    let mut first = fx.client();
    let mut second = fx.client();
    let mut paster = fx.client();
    first.get_device();
    second.get_device();
    paster.get_device();

    first.own_selection("first");
    second.own_selection("second");
    first.settle();
    paster.settle();

    assert!(
        first.app.cancelled,
        "the displaced owner should be told it no longer holds the selection"
    );
    assert_eq!(paste(&mut paster, &mut second), b"second");
}

#[test]
fn clearing_the_selection_withdraws_it() {
    let fx = Fixture::new();
    let mut owner = fx.client();
    let mut paster = fx.client();
    owner.get_device();
    paster.get_device();

    owner.own_selection(TEXT);
    paster.settle();
    assert!(paster.app.selection.is_some(), "selection was offered");

    owner.clear_selection();
    paster.settle();
    assert!(
        paster.app.selection.is_none(),
        "clearing should reach the other client, not leave it holding a dead offer"
    );
}

#[test]
fn a_dropped_source_withdraws_the_selection() {
    let fx = Fixture::new();
    let mut owner = fx.client();
    let mut paster = fx.client();
    owner.get_device();
    paster.get_device();

    let source = owner.own_selection(TEXT);
    paster.settle();
    assert!(paster.app.selection.is_some(), "selection was offered");

    // An app exiting takes its source with it; the offers pointing at it
    // are unbacked from that moment.
    source.destroy();
    owner.settle();
    paster.settle();
    assert!(
        paster.app.selection.is_none(),
        "a destroyed source should not leave a pasteable offer behind"
    );
}

#[test]
fn no_selection_means_no_offer() {
    let fx = Fixture::new();
    let mut paster = fx.client();
    paster.get_device();

    // The clear on bind is allowed, but it must not name an offer.
    assert!(paster.app.selection.is_none());
    assert!(paster.app.offered.is_empty());
}
