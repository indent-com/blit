//! A browser paste has to reach the app as the type it actually is.
//!
//! The selection the browser hands us is one MIME type and a blob of bytes,
//! and the only thing telling an app what those bytes are is the type we
//! advertise.  Text used to be the only thing that arrived, so the data
//! device answered `text/plain` unconditionally -- which for an image is
//! not a lenient alias, it is a lie the client cannot detect.  These tests
//! pin what the app sees, because nothing on the compositor side
//! distinguishes the two cases.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use wayland_client::protocol::{
    wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_registry, wl_seat,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};

use blit_compositor::{CompositorCommand, CompositorEvent, CompositorHandle, spawn_compositor};

#[derive(Default)]
struct App {
    ddm: Option<wl_data_device_manager::WlDataDeviceManager>,
    seat: Option<wl_seat::WlSeat>,
    /// MIME types advertised on the most recent offer, in arrival order.
    offered: Vec<String>,
    /// The offer the compositor last named as the selection.
    selection: Option<wl_data_offer::WlDataOffer>,
    source_sends: Vec<String>,
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
            // A fresh offer supersedes whatever was advertised before it.
            wl_data_device::Event::DataOffer { .. } => state.offered.clear(),
            wl_data_device::Event::Selection { id } => state.selection = id,
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
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.offered.push(mime_type);
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
        if let wl_data_source::Event::Send { mime_type, fd } = event {
            let mut file = std::fs::File::from(fd);
            file.write_all(PNG).expect("write clipboard image");
            state.source_sends.push(mime_type);
        }
    }
}

delegate_noop!(App: ignore wl_seat::WlSeat);
delegate_noop!(App: ignore wl_data_device_manager::WlDataDeviceManager);

struct Fixture {
    app: App,
    queue: EventQueue<App>,
    conn: Connection,
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
        let stream =
            UnixStream::connect(&handle.socket_name).expect("connect to compositor socket");
        let conn = Connection::from_socket(stream).expect("wayland connection");
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());

        let mut app = App::default();
        queue.roundtrip(&mut app).expect("registry roundtrip");
        let ddm = app.ddm.clone().expect("wl_data_device_manager advertised");
        let seat = app.seat.clone().expect("wl_seat advertised");
        // The device has to exist before the selection is set: the offer is
        // pushed to whichever devices are connected at that moment.
        let _device = ddm.get_data_device(&seat, &qh, ());
        queue.roundtrip(&mut app).expect("device roundtrip");

        Self {
            app,
            queue,
            conn,
            handle: Some(handle),
        }
    }

    /// Put an external (browser/CLI) selection on the clipboard and settle.
    fn offer(&mut self, mime_type: &str, data: &[u8]) {
        let handle = self.handle.as_ref().expect("compositor running");
        handle
            .command_tx
            .send(CompositorCommand::ClipboardOffer {
                mime_type: mime_type.to_string(),
                data: data.to_vec(),
            })
            .expect("send command");
        handle.wake();
        // The command is handled on the compositor's own thread.
        std::thread::sleep(Duration::from_millis(50));
        self.queue.roundtrip(&mut self.app).expect("roundtrip");
    }

    /// Ask the current selection for one MIME type and read what comes back.
    fn receive(&mut self, mime_type: &str) -> Vec<u8> {
        let offer = self.app.selection.clone().expect("a selection was offered");
        let (mut reader, writer) = UnixStream::pair().expect("pipe");
        offer.receive(mime_type.to_string(), writer.as_fd());
        // Flush the request (and the fd with it) before dropping our end,
        // or the compositor gets a socket that is already closed.
        self.conn.flush().expect("flush");
        drop(writer);
        std::thread::sleep(Duration::from_millis(50));
        // A type we were not offered gets the fd closed empty, so bound the
        // read rather than waiting on an EOF that a broken compositor might
        // never send.
        reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        buf
    }

    /// Drain compositor events, asserting the channel stayed alive.
    fn drain_events(&mut self) -> Vec<CompositorEvent> {
        let handle = self.handle.as_ref().expect("compositor running");
        let mut out = Vec::new();
        loop {
            match handle.event_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(ev) => out.push(ev),
                Err(RecvTimeoutError::Timeout) => break,
                Err(e) => panic!("compositor event channel closed: {e}"),
            }
        }
        out
    }
}

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xde, 0xad];

#[test]
fn an_image_selection_is_advertised_as_the_image_type_alone() {
    let mut fx = Fixture::new();
    fx.offer("image/png", PNG);
    assert_eq!(fx.app.offered, vec!["image/png".to_string()]);
    assert!(fx.app.selection.is_some(), "no selection was announced");
}

#[test]
fn an_image_selection_reads_back_byte_for_byte() {
    let mut fx = Fixture::new();
    fx.offer("image/png", PNG);
    assert_eq!(fx.receive("image/png"), PNG);
}

#[test]
fn an_image_selection_hands_a_text_request_nothing() {
    let mut fx = Fixture::new();
    fx.offer("image/png", PNG);
    // The text aliases belong to text.  Answering them with PNG bytes would
    // make a text editor paste binary it has no way to recognise.
    assert!(fx.receive("text/plain").is_empty());
    assert!(fx.receive("text/plain;charset=utf-8").is_empty());
    assert!(fx.receive("UTF8_STRING").is_empty());
}

#[test]
fn a_text_selection_still_answers_to_every_alias() {
    let mut fx = Fixture::new();
    fx.offer("text/plain;charset=utf-8", b"hello");
    assert_eq!(
        fx.app.offered,
        vec![
            "text/plain;charset=utf-8".to_string(),
            "text/plain".to_string(),
            "UTF8_STRING".to_string(),
        ]
    );
    assert_eq!(fx.receive("text/plain"), b"hello");
    assert_eq!(fx.receive("UTF8_STRING"), b"hello");
    assert_eq!(fx.receive("image/png"), b"");
}

#[test]
fn replacing_a_text_selection_with_an_image_withdraws_the_text_types() {
    let mut fx = Fixture::new();
    fx.offer("text/plain;charset=utf-8", b"hello");
    fx.offer("image/png", PNG);
    // A stale text alias here would let an app paste the *previous*
    // selection's bytes long after they were replaced.
    assert_eq!(fx.app.offered, vec!["image/png".to_string()]);
    assert_eq!(fx.receive("image/png"), PNG);
    assert!(fx.receive("text/plain").is_empty());
    fx.drain_events();
}

#[test]
fn a_wayland_clients_image_selection_splices_directly_to_another_client() {
    let mut fx = Fixture::new();

    // A second Wayland client is the clipboard owner (Slack); the fixture's
    // original client is the eventual paste target (Legcord).
    let socket = fx
        .handle
        .as_ref()
        .expect("compositor running")
        .socket_name
        .clone();
    let stream = UnixStream::connect(socket).expect("connect owner");
    let owner_conn = Connection::from_socket(stream).expect("owner connection");
    let mut owner_queue = owner_conn.new_event_queue();
    let owner_qh = owner_queue.handle();
    owner_conn.display().get_registry(&owner_qh, ());
    let mut owner = App::default();
    owner_queue
        .roundtrip(&mut owner)
        .expect("owner registry roundtrip");
    let ddm = owner.ddm.clone().expect("owner ddm");
    let seat = owner.seat.clone().expect("owner seat");
    let device = ddm.get_data_device(&seat, &owner_qh, ());
    let source = ddm.create_data_source(&owner_qh, ());
    source.offer("image/png".to_string());
    device.set_selection(Some(&source), 0);
    owner_queue
        .roundtrip(&mut owner)
        .expect("publish owner selection");

    std::thread::sleep(Duration::from_millis(50));
    assert!(
        fx.drain_events()
            .iter()
            .any(|event| matches!(event, CompositorEvent::ClipboardOwner { wayland: true })),
        "the web side must learn that browser paste may not replace this selection"
    );
    fx.queue
        .roundtrip(&mut fx.app)
        .expect("target selection roundtrip");
    assert_eq!(fx.app.offered, vec!["image/png".to_string()]);

    let offer = fx.app.selection.clone().expect("target selection offer");
    let (mut reader, writer) = UnixStream::pair().expect("transfer pipe");
    offer.receive("image/png".to_string(), writer.as_fd());
    fx.conn.flush().expect("flush target receive");
    drop(writer);
    std::thread::sleep(Duration::from_millis(50));
    owner_queue
        .roundtrip(&mut owner)
        .expect("owner handles send");

    reader
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).expect("read image");
    assert_eq!(bytes, PNG);
    assert_eq!(owner.source_sends, vec!["image/png".to_string()]);

    source.destroy();
    owner_queue
        .roundtrip(&mut owner)
        .expect("destroy owner selection");
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        fx.drain_events()
            .iter()
            .any(|event| matches!(event, CompositorEvent::ClipboardOwner { wayland: false })),
        "destroying the owner must re-enable browser clipboard import"
    );
}
