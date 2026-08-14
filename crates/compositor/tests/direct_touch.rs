//! Direct multitouch, as the Wayland client actually receives it.
//!
//! The interesting behaviour is all in the events, which are invisible from the
//! compositor side: one `wl_touch.frame` per browser `TouchEvent`, an implicit
//! grab that keeps a moving contact on the surface it went down on, and — the
//! case that broke — a touch-started drag, which takes the seat over and so
//! must cancel the sequence rather than strand the contacts it swallows.

#![cfg(target_os = "linux")]

use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use wayland_client::protocol::{
    wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
    wl_registry, wl_seat, wl_surface, wl_touch,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use blit_compositor::{
    CompositorCommand, CompositorEvent, CompositorHandle, TouchPhase, TouchPoint,
    spawn_compositor_without_renderer,
};

/// A `wl_touch` event, reduced to what these tests pin.
#[derive(Debug, PartialEq, Clone)]
enum T {
    Down { id: i32, x: f64, y: f64 },
    Up { id: i32 },
    Motion { id: i32, x: f64, y: f64 },
    Frame,
    Cancel,
}

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    ddm: Option<wl_data_device_manager::WlDataDeviceManager>,
    seat: Option<wl_seat::WlSeat>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    events: Vec<T>,
    /// Serial of each `down`, keyed by the wayland contact id, so a drag can be
    /// authorised by the contact that started it.
    down_serials: Vec<(i32, u32)>,
    /// `time` of each motion, which is what an app differentiates to get a fling
    /// velocity.
    motion_times: Vec<u32>,
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
            "xdg_wm_base" => {
                state.wm_base =
                    Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(name, 1, qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_touch::WlTouch, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_touch::WlTouch,
        event: wl_touch::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_touch::Event::Down {
                serial, id, x, y, ..
            } => {
                state.down_serials.push((id, serial));
                state.events.push(T::Down { id, x, y });
            }
            wl_touch::Event::Up { id, .. } => state.events.push(T::Up { id }),
            wl_touch::Event::Motion { id, x, y, time } => {
                state.motion_times.push(time);
                state.events.push(T::Motion { id, x, y })
            }
            wl_touch::Event::Frame => state.events.push(T::Frame),
            wl_touch::Event::Cancel => state.events.push(T::Cancel),
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        _: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_seat::WlSeat);
delegate_noop!(App: ignore xdg_toplevel::XdgToplevel);
delegate_noop!(App: ignore wl_data_device_manager::WlDataDeviceManager);
delegate_noop!(App: ignore wl_data_device::WlDataDevice);
delegate_noop!(App: ignore wl_data_offer::WlDataOffer);
delegate_noop!(App: ignore wl_data_source::WlDataSource);

struct Fixture {
    app: App,
    queue: EventQueue<App>,
    surface_id: u16,
    seat: wl_seat::WlSeat,
    ddm: wl_data_device_manager::WlDataDeviceManager,
    _touch: wl_touch::WlTouch,
    surface: wl_surface::WlSurface,
    _xdg_surface: xdg_surface::XdgSurface,
    _conn: Connection,
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
        let ddm = app.ddm.clone().expect("wl_data_device_manager advertised");
        let seat = app.seat.clone().expect("wl_seat advertised");
        let wm_base = app.wm_base.clone().expect("xdg_wm_base advertised");

        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let _toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        queue.roundtrip(&mut app).expect("configure roundtrip");

        let surface_id = loop {
            match handle.event_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(CompositorEvent::SurfaceCreated { surface_id, .. }) => break surface_id,
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => panic!("no SurfaceCreated within 5s"),
                Err(e) => panic!("compositor event channel closed: {e}"),
            }
        };

        // The seat advertises Touch only while a viewer has direct mode on, so
        // the capability has to be turned on before `get_touch`.
        handle
            .command_tx
            .send(CompositorCommand::SetTouchEnabled { enabled: true })
            .expect("enable touch");
        handle.wake();
        std::thread::sleep(Duration::from_millis(50));
        queue.roundtrip(&mut app).expect("capabilities roundtrip");
        let touch = seat.get_touch(&qh, ());
        queue.roundtrip(&mut app).expect("get_touch roundtrip");

        let mut fixture = Self {
            app,
            queue,
            surface_id,
            seat,
            ddm,
            _touch: touch,
            surface,
            _xdg_surface: xdg_surface,
            _conn: conn,
            handle: Some(handle),
        };
        fixture.app.events.clear();
        fixture
    }

    fn touch_cmd(&mut self, phase: TouchPhase, contacts: &[(i32, f64, f64)]) {
        self.touch_cmd_at(phase, 0, contacts);
    }

    fn touch_cmd_at(&mut self, phase: TouchPhase, time_ms: u32, contacts: &[(i32, f64, f64)]) {
        let handle = self.handle.as_ref().expect("compositor running");
        handle
            .command_tx
            .send(CompositorCommand::Touch {
                owner_id: 1,
                surface_id: self.surface_id,
                phase,
                time_ms,
                contacts: contacts
                    .iter()
                    .map(|&(id, x, y)| TouchPoint { id, x, y })
                    .collect(),
            })
            .expect("send touch");
        handle.wake();
        // The compositor handles the command on its own thread.
        std::thread::sleep(Duration::from_millis(50));
        self.queue.roundtrip(&mut self.app).expect("roundtrip");
    }

    fn serial_of(&self, wayland_id: i32) -> u32 {
        self.app
            .down_serials
            .iter()
            .rev()
            .find(|(id, _)| *id == wayland_id)
            .map(|(_, serial)| *serial)
            .expect("a down for that contact")
    }

    fn dispatch_until_motion_count(&mut self, count: usize) {
        self.dispatch_until(|app| app.motion_times.len() >= count);
    }

    fn dispatch_until(&mut self, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done(&self.app) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for touch events");
            let mut pollfd = libc::pollfd {
                fd: self.queue.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
            assert!(ready > 0, "timed out waiting for the Wayland touch socket");
            self.queue
                .blocking_dispatch(&mut self.app)
                .expect("dispatch touch event");
        }
    }
}

/// One transport message is one `wl_touch.frame`, so contacts that changed
/// together stay atomic and a browser's pinch arithmetic works.
#[test]
fn contacts_that_changed_together_arrive_in_one_frame() {
    let mut f = Fixture::new();

    f.touch_cmd(TouchPhase::Down, &[(10, 4.0, 5.0), (11, 20.0, 25.0)]);
    assert_eq!(
        f.app.events,
        vec![
            T::Down {
                id: 0,
                x: 4.0,
                y: 5.0
            },
            T::Down {
                id: 1,
                x: 20.0,
                y: 25.0
            },
            T::Frame,
        ],
    );

    f.app.events.clear();
    f.touch_cmd(TouchPhase::Motion, &[(10, 6.0, 7.0), (11, 21.0, 26.0)]);
    assert_eq!(
        f.app.events,
        vec![
            T::Motion {
                id: 0,
                x: 6.0,
                y: 7.0
            },
            T::Motion {
                id: 1,
                x: 21.0,
                y: 26.0
            },
            T::Frame,
        ],
    );

    f.app.events.clear();
    f.touch_cmd(TouchPhase::Up, &[(10, 6.0, 7.0)]);
    assert_eq!(f.app.events, vec![T::Up { id: 0 }, T::Frame]);

    // Cancel is terminal for the whole sequence and needs no frame.
    f.app.events.clear();
    f.touch_cmd(TouchPhase::Cancel, &[]);
    assert_eq!(f.app.events, vec![T::Cancel]);
}

/// A touch-started drag takes the seat over, and `wl_touch.cancel` is what that
/// means: the client is told to forget the whole sequence at `start_drag`.
///
/// The alternative — freezing the swallowed contacts and reporting their `up`
/// individually — leaves the app watching a finger stop dead mid-gesture, and it
/// is not expressible anyway: `cancel` has no per-contact form, so any contact
/// the drag swallows can only be retired with the rest of the sequence.
#[test]
fn a_touch_drag_cancels_the_sequence_it_takes_over() {
    let mut f = Fixture::new();
    let qh = f.queue.handle();
    let device = f.ddm.get_data_device(&f.seat, &qh, ());

    // Both contacts go down before the drag: a new down during one is ignored
    // outright, which is a different (already correct) path.
    f.touch_cmd(TouchPhase::Down, &[(10, 4.0, 5.0), (11, 20.0, 25.0)]);
    assert_eq!(
        f.app.events,
        vec![
            T::Down {
                id: 0,
                x: 4.0,
                y: 5.0
            },
            T::Down {
                id: 1,
                x: 20.0,
                y: 25.0
            },
            T::Frame,
        ],
    );

    // Authorise the drag with contact 0's down serial, so the grab follows it.
    f.app.events.clear();
    let source = f.ddm.create_data_source(&qh, ());
    source.offer("text/plain".to_string());
    source.set_actions(wl_data_device_manager::DndAction::Copy);
    device.start_drag(Some(&source), &f.surface, None, f.serial_of(0));
    f.queue.roundtrip(&mut f.app).expect("start_drag roundtrip");
    assert_eq!(
        f.app.events,
        vec![T::Cancel],
        "the drag took the seat without telling the client",
    );

    // Both contacts are retired as far as the client is concerned, so neither
    // lift may produce anything: reporting an `up` for a cancelled contact is
    // exactly the inconsistency `cancel` exists to avoid.
    f.app.events.clear();
    f.touch_cmd(TouchPhase::Up, &[(11, 20.0, 25.0)]);
    assert_eq!(f.app.events, Vec::new());

    // The drag's own contact still drives the drop, and still sends no touch
    // event — the drag ends through the data device.
    f.touch_cmd(TouchPhase::Up, &[(10, 4.0, 5.0)]);
    assert_eq!(f.app.events, Vec::new());

    // The sequence is fully retired, so touch works again. Released Wayland ids
    // are reusable; keeping them bounded matches physical touchscreen slots and
    // prevents Chromium exhausting its gesture recognizer's pointer-id range.
    f.touch_cmd(TouchPhase::Down, &[(12, 8.0, 9.0)]);
    assert_eq!(
        f.app.events,
        vec![
            T::Down {
                id: 0,
                x: 8.0,
                y: 9.0
            },
            T::Frame,
        ],
    );
}

/// Motion timestamps must carry the browser's spacing, not the compositor's
/// clock at drain time.
///
/// Apps derive fling velocity by differentiating position against `time`. The
/// compositor drains its whole command queue in one pass, so reading its own
/// clock per event collapsed a burst of coalesced browser moves onto a single
/// instant — the velocity estimate became a division by zero and inertial
/// scrolling silently stopped working, while dragging (positions only) still did.
#[test]
fn motion_timestamps_preserve_the_browser_cadence() {
    let mut f = Fixture::new();
    // Down and moves are queued in ONE batch, deliberately. Letting the
    // compositor wake in between leaves its clock enough headroom to absorb the
    // burst, which hid a clamp that flattened the spacing anyway.
    let delivery_started = {
        let handle = f.handle.as_ref().expect("compositor running");
        let send = |phase, time_ms, x: f64| {
            handle
                .command_tx
                .send(CompositorCommand::Touch {
                    owner_id: 1,
                    surface_id: f.surface_id,
                    phase,
                    time_ms,
                    contacts: vec![TouchPoint { id: 10, x, y: 0.0 }],
                })
                .expect("send touch");
        };
        send(TouchPhase::Down, 1_000, 0.0);
        // A browser sending 8ms-apart moves whose messages arrive together.
        for i in 1..=5u32 {
            send(TouchPhase::Motion, 1_000 + i * 8, f64::from(i) * 20.0);
        }
        let started = Instant::now();
        handle.wake();
        started
    };
    f.dispatch_until_motion_count(5);

    let times = f.app.motion_times.clone();
    assert_eq!(times.len(), 5, "expected one motion per queued command");
    let deltas: Vec<u32> = times.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(
        deltas,
        vec![8, 8, 8, 8],
        "browser cadence was replaced by drain-time stamps: {times:?}"
    );
    assert!(
        delivery_started.elapsed() >= Duration::from_millis(20),
        "touch frames reached the client in one compositor drain"
    );

    // Only the deltas are honoured. The client's epoch is its own, so the first
    // event anchors to ours instead of arriving as 1008 — `wl_touch.time` shares
    // a millisecond domain with the rest of the seat's input.
    assert!(
        times[0] > 100_000,
        "client epoch leaked into wl_touch.time: {times:?}"
    );
}

/// Falling behind a sustained iPad stream must shed stale positions. Keeping
/// every 120 Hz sample delays the final lift until Chromium has discarded the
/// velocity history, so a gesture can work initially and lose inertia once the
/// compositor has been busy long enough to accumulate a backlog.
#[test]
fn sustained_touch_backlog_stays_bounded_and_reaches_the_latest_position() {
    let mut f = Fixture::new();
    let handle = f.handle.as_ref().expect("compositor running");
    let send = |phase, time_ms, x: f64| {
        handle
            .command_tx
            .send(CompositorCommand::Touch {
                owner_id: 1,
                surface_id: f.surface_id,
                phase,
                time_ms,
                contacts: vec![TouchPoint { id: 10, x, y: 0.0 }],
            })
            .expect("send touch");
    };
    send(TouchPhase::Down, 1_000, 0.0);
    for i in 1..=120u32 {
        send(TouchPhase::Motion, 1_000 + i * 8, f64::from(i) * 20.0);
    }
    send(TouchPhase::Up, 1_968, 2_400.0);
    let started = Instant::now();
    handle.wake();

    f.dispatch_until(|app| app.events.iter().any(|event| matches!(event, T::Up { .. })));

    assert!(
        started.elapsed() < Duration::from_millis(500),
        "stale touch samples delayed the release by {:?}",
        started.elapsed()
    );
    let motions: Vec<_> = f
        .app
        .events
        .iter()
        .filter_map(|event| match event {
            T::Motion { x, .. } => Some(*x),
            _ => None,
        })
        .collect();
    assert!(motions.len() >= 5, "too few velocity samples: {motions:?}");
    assert!(
        motions.len() < 120,
        "the stale motion backlog was not compacted"
    );
    assert_eq!(motions.last(), Some(&2_400.0));
    assert!(
        f.app.motion_times.last().unwrap() - f.app.motion_times.first().unwrap() <= 100,
        "compaction leaked the discarded browser-time gap: {:?}",
        f.app.motion_times
    );
}

/// A different browser's clock must not become the anchor for direct touch.
///
/// The dev stack routinely has a desktop viewer and an iPad attached at once.
/// Their DOM timestamps use different page epochs. If pointer input from one
/// viewer anchors the shared clock, every queued iPad touch event falls back to
/// compositor drain time and the whole motion burst once again has a zero span.
#[test]
fn direct_touch_has_its_own_browser_clock() {
    let mut f = Fixture::new();
    let handle = f.handle.as_ref().expect("compositor running");

    handle
        .command_tx
        .send(CompositorCommand::PointerMotion {
            surface_id: f.surface_id,
            x: 0.0,
            y: 0.0,
            time_ms: 1_000,
        })
        .expect("send foreign pointer time");
    let send_touch = |phase, time_ms, x: f64| {
        handle
            .command_tx
            .send(CompositorCommand::Touch {
                owner_id: 2,
                surface_id: f.surface_id,
                phase,
                time_ms,
                contacts: vec![TouchPoint { id: 10, x, y: 0.0 }],
            })
            .expect("send touch");
    };
    send_touch(TouchPhase::Down, 1_000_000, 0.0);
    for i in 1..=5u32 {
        send_touch(TouchPhase::Motion, 1_000_000 + i * 8, f64::from(i) * 20.0);
    }
    handle.wake();

    std::thread::sleep(Duration::from_millis(80));
    f.queue.roundtrip(&mut f.app).expect("roundtrip");
    let deltas: Vec<u32> = f
        .app
        .motion_times
        .windows(2)
        .map(|times| times[1] - times[0])
        .collect();
    assert_eq!(
        deltas,
        vec![8, 8, 8, 8],
        "another browser flattened the direct-touch cadence: {:?}",
        f.app.motion_times
    );
}

#[test]
fn withdrawing_touch_cancels_a_down_that_is_still_scheduled() {
    let mut f = Fixture::new();
    let handle = f.handle.as_ref().expect("compositor running");
    handle
        .command_tx
        .send(CompositorCommand::Touch {
            owner_id: 1,
            surface_id: f.surface_id,
            phase: TouchPhase::Down,
            time_ms: 1_000,
            contacts: vec![TouchPoint {
                id: 10,
                x: 10.0,
                y: 20.0,
            }],
        })
        .expect("queue down");
    handle
        .command_tx
        .send(CompositorCommand::SetTouchEnabled { enabled: false })
        .expect("withdraw touch");
    handle.wake();

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "no cancellation for the queued down");
        match handle.event_rx.recv_timeout(remaining) {
            Ok(CompositorEvent::TouchCancelled { owner_id }) => {
                assert_eq!(owner_id, None);
                break;
            }
            Ok(_) => {}
            Err(error) => panic!("no cancellation for the queued down: {error}"),
        }
    }
    f.queue.roundtrip(&mut f.app).expect("roundtrip");
    assert!(
        f.app.events.is_empty(),
        "the discarded down leaked to the Wayland client: {:?}",
        f.app.events
    );
}
