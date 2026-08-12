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

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use wayland_client::backend::ObjectId;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_subcompositor, wl_subsurface, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};

use blit_compositor::{CompositorCommand, CompositorEvent, CompositorHandle, spawn_compositor};

use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// A pointer event, reduced to what this test cares about.
#[derive(Debug, PartialEq, Clone)]
enum Ptr {
    Enter(ObjectId),
    Leave(ObjectId),
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
    shm: Option<wl_shm::WlShm>,
    subcompositor: Option<wl_subcompositor::WlSubcompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    events: Vec<Ptr>,
    /// `time` of each axis event, which is what a toolkit integrates against for
    /// kinetic scrolling.
    axis_times: Vec<u32>,
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
            "wl_subcompositor" => {
                state.subcompositor =
                    Some(registry.bind::<wl_subcompositor::WlSubcompositor, _, _>(name, 1, qh, ()));
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
            wl_pointer::Event::Enter { surface, .. } => state.events.push(Ptr::Enter(surface.id())),
            wl_pointer::Event::Leave { surface, .. } => state.events.push(Ptr::Leave(surface.id())),
            wl_pointer::Event::AxisSource { axis_source } => state.events.push(Ptr::Source(
                axis_source.into_result().map(|s| s as u32).unwrap_or(99),
            )),
            wl_pointer::Event::Axis { axis, value, time } => {
                state.axis_times.push(time);
                state.events.push(Ptr::Axis {
                    axis: axis_of(axis),
                    value,
                })
            }
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
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(App: ignore wl_subsurface::WlSubsurface);
delegate_noop!(App: ignore wl_surface::WlSurface);

struct Fixture {
    app: App,
    queue: EventQueue<App>,
    surface_id: u16,
    compositor: wl_compositor::WlCompositor,
    subcompositor: wl_subcompositor::WlSubcompositor,
    pool: wl_shm_pool::WlShmPool,
    _pointer: wl_pointer::WlPointer,
    _surface: wl_surface::WlSurface,
    _root_buffer: wl_buffer::WlBuffer,
    _backing: OwnedFd,
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
        let shm = app.shm.clone().expect("wl_shm advertised");
        let subcompositor = app
            .subcompositor
            .clone()
            .expect("wl_subcompositor advertised");
        let wm_base = app.wm_base.clone().expect("xdg_wm_base advertised");
        let seat = app.seat.clone().expect("wl_seat advertised");
        let pointer = seat.get_pointer(&qh, ());

        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let _toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        queue.roundtrip(&mut app).expect("configure roundtrip");

        const ROOT_W: i32 = 64;
        const ROOT_H: i32 = 64;
        const CHILD_W: i32 = 24;
        const CHILD_H: i32 = 24;
        let root_bytes = ROOT_W * ROOT_H * 4;
        let pool_bytes = root_bytes + CHILD_W * CHILD_H * 4;
        let raw_fd = unsafe { libc::memfd_create(c"pointer-axis".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw_fd >= 0, "memfd_create failed");
        assert_eq!(unsafe { libc::ftruncate(raw_fd, pool_bytes.into()) }, 0);
        let backing = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let pool = shm.create_pool(backing.as_fd(), pool_bytes, &qh, ());
        let root_buffer = pool.create_buffer(
            0,
            ROOT_W,
            ROOT_H,
            ROOT_W * 4,
            wl_shm::Format::Xrgb8888,
            &qh,
            (),
        );
        surface.attach(Some(&root_buffer), 0, 0);
        surface.damage_buffer(0, 0, ROOT_W, ROOT_H);
        surface.commit();
        queue.roundtrip(&mut app).expect("map roundtrip");

        // The compositor names the surface in an event. Seed a pointer point
        // there so later axis messages can preserve the precise hit target as
        // well as use the surface's scale.
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
            compositor,
            subcompositor,
            pool,
            _pointer: pointer,
            _surface: surface,
            _root_buffer: root_buffer,
            _backing: backing,
            _xdg_surface: xdg_surface,
            _conn: conn,
            handle: Some(handle),
        };
        // Scroll only reaches a surface the pointer is inside.
        fixture.send(CompositorCommand::PointerMotion {
            surface_id,
            x: 10.0,
            y: 10.0,
            time_ms: 0,
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

    fn map_subsurface(
        &mut self,
    ) -> (
        wl_surface::WlSurface,
        wl_subsurface::WlSubsurface,
        wl_buffer::WlBuffer,
    ) {
        const ROOT_BYTES: i32 = 64 * 64 * 4;
        const CHILD_W: i32 = 24;
        const CHILD_H: i32 = 24;
        let qh = self.queue.handle();
        let child = self.compositor.create_surface(&qh, ());
        let subsurface = self
            .subcompositor
            .get_subsurface(&child, &self._surface, &qh, ());
        subsurface.set_position(4, 4);
        let buffer = self.pool.create_buffer(
            ROOT_BYTES,
            CHILD_W,
            CHILD_H,
            CHILD_W * 4,
            wl_shm::Format::Xrgb8888,
            &qh,
            (),
        );
        child.attach(Some(&buffer), 0, 0);
        child.damage_buffer(0, 0, CHILD_W, CHILD_H);
        child.commit();
        self.queue
            .roundtrip(&mut self.app)
            .expect("subsurface map roundtrip");
        (child, subsurface, buffer)
    }

    fn map_toplevel(
        &mut self,
    ) -> (
        u16,
        wl_surface::WlSurface,
        xdg_surface::XdgSurface,
        xdg_toplevel::XdgToplevel,
    ) {
        let qh = self.queue.handle();
        let surface = self.compositor.create_surface(&qh, ());
        let wm_base = self.app.wm_base.as_ref().expect("xdg_wm_base advertised");
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        self.queue
            .roundtrip(&mut self.app)
            .expect("second configure roundtrip");

        surface.attach(Some(&self._root_buffer), 0, 0);
        surface.damage_buffer(0, 0, 64, 64);
        surface.commit();
        self.queue
            .roundtrip(&mut self.app)
            .expect("second map roundtrip");

        let handle = self.handle.as_ref().expect("compositor running");
        let surface_id = loop {
            match handle.event_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(CompositorEvent::SurfaceCreated { surface_id, .. }) => break surface_id,
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => {
                    panic!("no second SurfaceCreated within 5s")
                }
                Err(e) => panic!("compositor event channel closed: {e}"),
            }
        };
        (surface_id, surface, xdg_surface, toplevel)
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
        time_ms: 0,
    }
}

const VERTICAL: u32 = 0;
const HORIZONTAL: u32 = 1;

#[test]
fn the_surface_named_by_scroll_is_the_target() {
    let mut f = Fixture::new();
    let (second_id, second, _xdg_surface, _toplevel) = f.map_toplevel();

    // Pointer focus is still on the first toplevel. The axis message names
    // the second one, so routing it through the stale global focus would
    // scroll the wrong window.
    let events = f.scroll(finger(second_id, 0.0, 4.5));
    assert!(events.contains(&Ptr::Leave(f._surface.id())));
    assert!(events.contains(&Ptr::Enter(second.id())));
    assert!(events.contains(&Ptr::Axis {
        axis: VERTICAL,
        value: 4.5,
    }));
}

#[test]
fn an_unknown_scroll_target_does_not_fall_through_to_pointer_focus() {
    let mut f = Fixture::new();

    let events = f.scroll(finger(u16::MAX, 0.0, 4.5));
    assert!(events.is_empty());
}

#[test]
fn null_buffer_unmap_retargets_scroll_from_a_stale_subsurface() {
    let mut f = Fixture::new();
    let (child, _subsurface, buffer) = f.map_subsurface();

    // Enter the child and make it the client's current pointer/axis target.
    f.app.events.clear();
    f.send(CompositorCommand::PointerMotion {
        surface_id: f.surface_id,
        x: 10.0,
        y: 10.0,
        time_ms: 0,
    });
    assert!(f.app.events.contains(&Ptr::Enter(child.id())));

    // Wayland clients hide reusable popups and subsurfaces this way.  The
    // null attach must retire the old hit target even though the wl_surface
    // object and its place in the tree both survive.
    f.app.events.clear();
    child.attach(None, 0, 0);
    child.commit();
    f.queue
        .roundtrip(&mut f.app)
        .expect("subsurface unmap roundtrip");
    assert!(f.app.events.contains(&Ptr::Leave(child.id())));

    // macOS momentum delivers another wheel event without a physical mouse
    // move.  The browser re-seeds motion at the wheel coordinates; it must
    // now enter the root, and the following continuous axis must arrive.
    f.app.events.clear();
    f.send(CompositorCommand::PointerMotion {
        surface_id: f.surface_id,
        x: 10.0,
        y: 10.0,
        time_ms: 0,
    });
    assert!(f.app.events.contains(&Ptr::Enter(f._surface.id())));
    let events = f.scroll(CompositorCommand::PointerAxis {
        surface_id: f.surface_id,
        dx: 0.0,
        dy: 4.5,
        v120_x: 0,
        v120_y: 0,
        source: Some(2),
        stop: false,
        time_ms: 0,
    });
    assert!(events.contains(&Ptr::Axis {
        axis: VERTICAL,
        value: 4.5,
    }));

    // The unmap retained the role and tree position, so attaching content
    // again maps the same child and makes it hittable without recreating it.
    child.attach(Some(&buffer), 0, 0);
    child.damage_buffer(0, 0, 24, 24);
    child.commit();
    f.queue
        .roundtrip(&mut f.app)
        .expect("subsurface remap roundtrip");
    f.app.events.clear();
    f.send(CompositorCommand::PointerMotion {
        surface_id: f.surface_id,
        x: 10.0,
        y: 10.0,
        time_ms: 0,
    });
    assert!(f.app.events.contains(&Ptr::Enter(child.id())));
}

/// An unmapped toplevel must not be re-entered by the next motion event.
///
/// GTK's `gtk_widget_hide()` and Chromium hiding a window before teardown both
/// unmap the toplevel with `attach(NULL)` while keeping the `xdg_toplevel` role.
/// The compositor owes it a `leave` — and then must not hand it a fresh `enter`
/// with nothing on screen, which is what the hit-test's unconditional
/// fall-back-to-the-root did.
#[test]
fn an_unmapped_toplevel_is_not_re_entered_by_the_input_fallback() {
    let mut f = Fixture::new();
    let root = f._surface.clone();
    let buffer = f._root_buffer.clone();
    // `Fixture::new` has already entered the root at (10, 10).

    f.app.events.clear();
    root.attach(None, 0, 0);
    root.commit();
    f.queue
        .roundtrip(&mut f.app)
        .expect("toplevel unmap roundtrip");
    assert!(f.app.events.contains(&Ptr::Leave(root.id())));

    // Nothing is mapped anywhere in the tree, so this motion has no legal
    // target at all.  Before, it re-entered the hidden toplevel and every
    // later scroll went to an invisible window.
    f.app.events.clear();
    f.send(CompositorCommand::PointerMotion {
        surface_id: f.surface_id,
        x: 12.0,
        y: 12.0,
        time_ms: 0,
    });
    assert_eq!(f.app.events, Vec::new(), "unmapped toplevel got input");

    // Attaching content again maps the same surface, and input returns.
    root.attach(Some(&buffer), 0, 0);
    root.damage_buffer(0, 0, 64, 64);
    root.commit();
    f.queue
        .roundtrip(&mut f.app)
        .expect("toplevel remap roundtrip");
    f.app.events.clear();
    f.send(CompositorCommand::PointerMotion {
        surface_id: f.surface_id,
        x: 10.0,
        y: 10.0,
        time_ms: 0,
    });
    assert!(f.app.events.contains(&Ptr::Enter(root.id())));
}

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
        time_ms: 0,
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
        time_ms: 0,
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
        time_ms: 0,
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
        time_ms: 0,
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
        time_ms: 0,
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
        time_ms: 0,
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

/// Axis timestamps must carry the browser's spacing, not the compositor's clock
/// at drain time.
///
/// Toolkits integrate axis deltas against `wl_pointer.axis` timestamps to fling a
/// kinetic scroll. The compositor drains its whole command queue in one pass, so
/// reading its own clock per event collapsed a burst of rAF-batched deltas onto
/// one instant — the velocity became a division by zero and the fling never
/// started, exactly as it did for direct touch.
#[test]
fn axis_timestamps_preserve_the_browser_cadence() {
    let mut f = Fixture::new();
    f.app.axis_times.clear();
    {
        let handle = f.handle.as_ref().expect("compositor running");
        for i in 0..5u32 {
            handle
                .command_tx
                .send(CompositorCommand::PointerAxis {
                    surface_id: f.surface_id,
                    dx: 0.0,
                    dy: 4.0,
                    v120_x: 0,
                    v120_y: 0,
                    source: Some(1),
                    stop: false,
                    time_ms: 5_000 + i * 16,
                })
                .expect("send axis");
        }
        handle.wake();
    }
    std::thread::sleep(Duration::from_millis(80));
    f.queue.roundtrip(&mut f.app).expect("roundtrip");

    let times = f.app.axis_times.clone();
    assert_eq!(times.len(), 5, "expected one axis event per command");
    let deltas: Vec<u32> = times.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(
        deltas,
        vec![16, 16, 16, 16],
        "browser cadence was replaced by drain-time stamps: {times:?}"
    );
    // Only the deltas are honoured: the client's epoch is its own, so the first
    // event anchors to ours rather than arriving as 5000.
    assert!(
        times[0] > 100_000,
        "client epoch leaked into wl_pointer.axis: {times:?}"
    );
}
