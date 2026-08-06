//! Scroll has to reach the client as a described gesture, not a bare delta.
//!
//! `wl_pointer.axis_source`'s zero value *is* `wheel`, so a compositor that
//! omits the event is not saying "unknown" -- it is saying "notched wheel",
//! and the spec invites clients to treat those as "discrete steps of a
//! number of lines".  A trackpad's smooth pixel stream then gets scaled up
//! by a lines-per-click factor.  These tests pin the events a client
//! actually receives, because the difference is invisible from the
//! compositor side.

#![cfg(target_os = "linux")]

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use wayland_client::protocol::{wl_compositor, wl_pointer, wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};

use blit_compositor::{CompositorCommand, CompositorEvent, CompositorHandle, spawn_compositor};

use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// A pointer event, reduced to what this test cares about.
#[derive(Debug, PartialEq, Clone)]
enum Ptr {
    Source(u32),
    Axis { axis: u32, value: f64 },
    Value120 { axis: u32, value: i32 },
    Discrete { axis: u32, value: i32 },
    Stop { axis: u32 },
    Frame,
}

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    events: Vec<Ptr>,
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
            "xdg_wm_base" => {
                state.wm_base =
                    Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(name, 1, qh, ()));
            }
            "wl_seat" => {
                // Bind at whatever the test asked for, so one fixture can
                // exercise both the value120 and the axis_discrete path.
                state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(
                    name,
                    version.min(SEAT_VERSION.with(|v| *v.borrow())),
                    qh,
                    (),
                ));
            }
            _ => {}
        }
    }
}

thread_local! {
    /// Version the next fixture binds wl_seat at.
    static SEAT_VERSION: std::cell::RefCell<u32> = const { std::cell::RefCell::new(9) };
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_client::WEnum;
        let axis_of = |a: WEnum<wl_pointer::Axis>| a.into_result().map(|a| a as u32).unwrap_or(99);
        match event {
            wl_pointer::Event::AxisSource { axis_source } => state.events.push(Ptr::Source(
                axis_source.into_result().map(|s| s as u32).unwrap_or(99),
            )),
            wl_pointer::Event::Axis { axis, value, .. } => state.events.push(Ptr::Axis {
                axis: axis_of(axis),
                value,
            }),
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                state.events.push(Ptr::Value120 {
                    axis: axis_of(axis),
                    value: value120,
                })
            }
            wl_pointer::Event::AxisDiscrete { axis, discrete } => {
                state.events.push(Ptr::Discrete {
                    axis: axis_of(axis),
                    value: discrete,
                })
            }
            wl_pointer::Event::AxisStop { axis, .. } => state.events.push(Ptr::Stop {
                axis: axis_of(axis),
            }),
            wl_pointer::Event::Frame => state.events.push(Ptr::Frame),
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

struct Fixture {
    app: App,
    queue: EventQueue<App>,
    surface_id: u16,
    _pointer: wl_pointer::WlPointer,
    _surface: wl_surface::WlSurface,
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
        Self::with_seat_version(9)
    }

    fn with_seat_version(version: u32) -> Self {
        SEAT_VERSION.with(|v| *v.borrow_mut() = version);
        let handle = spawn_compositor(false, Arc::new(|| {}), "");

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
        let seat = app.seat.clone().expect("wl_seat advertised");
        let pointer = seat.get_pointer(&qh, ());

        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let _toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        queue.roundtrip(&mut app).expect("map roundtrip");

        // The compositor names the surface in an event; scroll is routed to
        // whichever surface the pointer last entered, so the id is needed
        // both to aim the motion and to look up the scale.
        let surface_id = loop {
            match handle.event_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(CompositorEvent::SurfaceCreated { surface_id, .. }) => break surface_id,
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => panic!("no SurfaceCreated within 5s"),
                Err(e) => panic!("compositor event channel closed: {e}"),
            }
        };

        let mut fixture = Self {
            app,
            queue,
            surface_id,
            _pointer: pointer,
            _surface: surface,
            _xdg_surface: xdg_surface,
            _conn: conn,
            handle: Some(handle),
        };
        // Scroll only reaches a surface the pointer is inside.
        fixture.send(CompositorCommand::PointerMotion {
            surface_id,
            x: 10.0,
            y: 10.0,
        });
        fixture.app.events.clear();
        fixture
    }

    fn send(&mut self, cmd: CompositorCommand) {
        let handle = self.handle.as_ref().expect("compositor running");
        handle.command_tx.send(cmd).expect("send command");
        handle.wake();
        // The compositor handles the command on its own thread, so give it
        // a moment before asking the client what arrived.
        std::thread::sleep(Duration::from_millis(50));
        self.queue.roundtrip(&mut self.app).expect("roundtrip");
    }

    fn scroll(&mut self, cmd: CompositorCommand) -> Vec<Ptr> {
        self.app.events.clear();
        self.send(cmd);
        self.app.events.clone()
    }
}

fn finger(surface_id: u16, dx: f64, dy: f64) -> CompositorCommand {
    CompositorCommand::PointerAxis {
        surface_id,
        dx,
        dy,
        v120_x: 0,
        v120_y: 0,
        source: Some(1), // finger
        stop: false,
    }
}

const VERTICAL: u32 = 0;
const HORIZONTAL: u32 = 1;

#[test]
fn a_trackpad_stream_is_labelled_a_finger_not_a_wheel() {
    let mut f = Fixture::new();
    let events = f.scroll(finger(f.surface_id, 0.0, 12.5));
    assert_eq!(
        events,
        vec![
            Ptr::Source(1),
            Ptr::Axis {
                axis: VERTICAL,
                value: 12.5
            },
            Ptr::Frame,
        ],
        "a finger-sourced scroll must announce its source before the delta"
    );
}

#[test]
fn a_wheel_carries_detents_alongside_the_smooth_delta() {
    let mut f = Fixture::new();
    let events = f.scroll(CompositorCommand::PointerAxis {
        surface_id: f.surface_id,
        dx: 0.0,
        dy: 120.0,
        v120_x: 0,
        v120_y: 120,
        source: Some(0), // wheel
        stop: false,
    });
    assert_eq!(
        events,
        vec![
            Ptr::Source(0),
            Ptr::Value120 {
                axis: VERTICAL,
                value: 120
            },
            Ptr::Axis {
                axis: VERTICAL,
                value: 120.0
            },
            Ptr::Frame,
        ],
        "value120 must be coupled with an axis event in the same frame"
    );
}

/// Every wheel event a browser reports that isn't provably notched now
/// travels as `continuous`, which makes this the source most scrolls take.
/// It has to arrive as itself: `wheel` would hand a toolkit detents to
/// scale up by its lines-per-click factor, and `finger` is what licenses
/// the invented momentum the labelling exists to avoid.
#[test]
fn a_smooth_stream_of_unknown_origin_stays_continuous() {
    let mut f = Fixture::new();
    let events = f.scroll(CompositorCommand::PointerAxis {
        surface_id: f.surface_id,
        dx: 0.0,
        dy: 40.0,
        v120_x: 0,
        v120_y: 0,
        source: Some(2), // continuous
        stop: false,
    });
    assert_eq!(
        events,
        vec![
            Ptr::Source(2),
            Ptr::Axis {
                axis: VERTICAL,
                value: 40.0
            },
            Ptr::Frame,
        ],
        "a continuous source must reach the client as continuous"
    );
}

#[test]
fn a_diagonal_gesture_stays_in_one_frame() {
    let mut f = Fixture::new();
    let events = f.scroll(finger(f.surface_id, 4.0, 8.0));
    assert_eq!(
        events,
        vec![
            Ptr::Source(1),
            Ptr::Axis {
                axis: VERTICAL,
                value: 8.0
            },
            Ptr::Axis {
                axis: HORIZONTAL,
                value: 4.0
            },
            Ptr::Frame,
        ],
        "both axes of one gesture belong to one frame"
    );
}

#[test]
fn a_finger_lift_terminates_the_sequence() {
    let mut f = Fixture::new();
    f.scroll(finger(f.surface_id, 0.0, 5.0));
    let events = f.scroll(CompositorCommand::PointerAxis {
        surface_id: f.surface_id,
        dx: 0.0,
        dy: 0.0,
        v120_x: 0,
        v120_y: 0,
        source: Some(1),
        stop: true,
    });
    assert_eq!(
        events,
        vec![
            Ptr::Source(1),
            Ptr::Stop { axis: VERTICAL },
            Ptr::Stop { axis: HORIZONTAL },
            Ptr::Frame,
        ],
        "a finger source promises an axis_stop when the finger lifts"
    );
}

/// The legacy `0x22` opcode carries no source, and must stay that way --
/// guessing one would label a scroll wrong rather than leave it unlabelled.
#[test]
fn an_unclassified_scroll_announces_no_source() {
    let mut f = Fixture::new();
    let events = f.scroll(CompositorCommand::PointerAxis {
        surface_id: f.surface_id,
        dx: 0.0,
        dy: 7.0,
        v120_x: 0,
        v120_y: 0,
        source: None,
        stop: false,
    });
    assert_eq!(
        events,
        vec![
            Ptr::Axis {
                axis: VERTICAL,
                value: 7.0
            },
            Ptr::Frame,
        ]
    );
}

/// `axis_value120` is v8+; older clients get the `axis_discrete` spelling
/// instead, and must never receive both.
#[test]
fn a_pre_v8_client_gets_axis_discrete_instead_of_value120() {
    let mut f = Fixture::with_seat_version(7);
    let events = f.scroll(CompositorCommand::PointerAxis {
        surface_id: f.surface_id,
        dx: 0.0,
        dy: 240.0,
        v120_x: 0,
        v120_y: 240,
        source: Some(0),
        stop: false,
    });
    assert_eq!(
        events,
        vec![
            Ptr::Source(0),
            Ptr::Discrete {
                axis: VERTICAL,
                value: 2
            },
            Ptr::Axis {
                axis: VERTICAL,
                value: 240.0
            },
            Ptr::Frame,
        ]
    );
}

/// Sub-detent travel has no `axis_discrete` spelling; a pre-v8 client must
/// get it as smooth motion rather than a rounded-to-zero notch.
#[test]
fn sub_detent_travel_reaches_a_pre_v8_client_as_smooth_motion() {
    let mut f = Fixture::with_seat_version(7);
    let events = f.scroll(CompositorCommand::PointerAxis {
        surface_id: f.surface_id,
        dx: 0.0,
        dy: 30.0,
        v120_x: 0,
        v120_y: 30,
        source: Some(0),
        stop: false,
    });
    assert_eq!(
        events,
        vec![
            Ptr::Source(0),
            Ptr::Axis {
                axis: VERTICAL,
                value: 30.0
            },
            Ptr::Frame,
        ]
    );
}

/// An empty scroll would otherwise become a `wl_pointer.frame` carrying
/// nothing, which clients are entitled to find surprising.
#[test]
fn a_zero_delta_sends_nothing() {
    let mut f = Fixture::new();
    let events = f.scroll(finger(f.surface_id, 0.0, 0.0));
    assert_eq!(events, Vec::new());
}
