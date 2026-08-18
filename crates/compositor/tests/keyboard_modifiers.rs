//! A surface taking keyboard focus has to be told which modifiers are held.
//!
//! Modifier state is seat-wide but delivery is not: `wl_keyboard.modifiers`
//! goes to whoever holds focus when a modifier key moves, and a client's own
//! state starts empty and moves on nothing else.  Ctrl held down while another
//! pane had focus is therefore a fact the newly focused client cannot derive
//! from anything it receives -- `enter` carries no modifiers of its own -- and
//! it reads the next keystroke unmodified.  Ctrl+K becomes a bare K.
//!
//! The keymap here is the compositor's own US-QWERTY one, so these tests speak
//! in its modifier bits rather than in xkbcommon's.

#![cfg(target_os = "linux")]

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use wayland_client::protocol::{wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use blit_compositor::{
    CompositorCommand, CompositorEvent, CompositorHandle, spawn_compositor_without_renderer,
};

/// Modifier bits, as `data/us-qwerty.xkb` numbers them.
const MOD_SHIFT: u32 = 1 << 0;
const MOD_LOCK: u32 = 1 << 1;
const MOD_CONTROL: u32 = 1 << 2;

const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_CAPSLOCK: u32 = 58;
const KEY_K: u32 = 37;

/// The keyboard events these tests care about, in arrival order -- the order is
/// half the property: a `modifiers` that trails the key it qualifies is a
/// modifier the app applied to the wrong keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    Enter,
    Leave,
    /// modifiers(depressed, locked)
    Mods(u32, u32),
    Key(u32, bool),
}

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    log: Vec<Ev>,
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

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { .. } => state.log.push(Ev::Enter),
            wl_keyboard::Event::Leave { .. } => state.log.push(Ev::Leave),
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_locked,
                ..
            } => state.log.push(Ev::Mods(mods_depressed, mods_locked)),
            wl_keyboard::Event::Key { key, state: s, .. } => {
                let pressed = matches!(s.into_result(), Ok(wl_keyboard::KeyState::Pressed));
                state.log.push(Ev::Key(key, pressed));
            }
            _ => {}
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore xdg_toplevel::XdgToplevel);
delegate_noop!(App: ignore wl_seat::WlSeat);

struct Fixture {
    app: App,
    queue: EventQueue<App>,
    conn: Connection,
    surface_id: u16,
    _surface: wl_surface::WlSurface,
    _toplevel: xdg_toplevel::XdgToplevel,
    _xdg_surface: xdg_surface::XdgSurface,
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
    /// A mapped toplevel with a `wl_keyboard`, wound back to unfocused: every
    /// test here is about what focus itself delivers, and mapping a toplevel
    /// hands it focus already.
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
        let seat = app.seat.clone().expect("wl_seat advertised");

        let _keyboard = seat.get_keyboard(&qh, ());
        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        queue.roundtrip(&mut app).expect("map roundtrip");

        let mut fx = Self {
            app,
            queue,
            conn,
            surface_id: 0,
            _surface: surface,
            _toplevel: toplevel,
            _xdg_surface: xdg_surface,
            handle: Some(handle),
        };
        fx.surface_id = fx.await_surface_id();
        fx.settle();
        fx.focus(0);
        fx.take_log();
        fx
    }

    fn command(&self, cmd: CompositorCommand) {
        let handle = self.handle.as_ref().expect("compositor running");
        handle.command_tx.send(cmd).expect("send command");
        handle.wake();
    }

    /// Flush our requests, let the compositor's own thread act, read back.
    fn settle(&mut self) {
        self.conn.flush().expect("flush");
        std::thread::sleep(Duration::from_millis(50));
        self.queue.roundtrip(&mut self.app).expect("roundtrip");
    }

    fn focus(&mut self, surface_id: u16) {
        self.command(CompositorCommand::SurfaceFocus { surface_id });
        self.settle();
    }

    /// Type a key the way the browser's `C2S_SURFACE_INPUT` does.
    fn key(&mut self, keycode: u32, pressed: bool) {
        let surface_id = self.surface_id;
        self.command(CompositorCommand::KeyInput {
            surface_id,
            keycode,
            pressed,
            time_ms: 0,
        });
        self.settle();
    }

    fn tap(&mut self, keycode: u32) {
        self.key(keycode, true);
        self.key(keycode, false);
    }

    fn take_log(&mut self) -> Vec<Ev> {
        std::mem::take(&mut self.app.log)
    }

    fn await_surface_id(&mut self) -> u16 {
        let handle = self.handle.as_ref().expect("compositor running");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(CompositorEvent::SurfaceCreated { surface_id, .. }) =
                handle.event_rx.recv_timeout(Duration::from_millis(200))
            {
                return surface_id;
            }
        }
        panic!("compositor never announced the surface");
    }
}

#[test]
fn focus_states_the_modifiers_already_held() {
    // The bug: Ctrl goes down while this surface has no focus, so its own
    // client is never in the set `update_and_send_modifiers` writes to. Focus
    // then arrives and the client has been told nothing at all.
    let mut fx = Fixture::new();
    fx.key(KEY_LEFTCTRL, true);
    assert_eq!(
        fx.take_log(),
        vec![],
        "an unfocused client should hear nothing about the press"
    );

    fx.focus(fx.surface_id);

    assert_eq!(
        fx.take_log(),
        vec![Ev::Enter, Ev::Mods(MOD_CONTROL, 0)],
        "focus should restate the held modifier, and after the enter it qualifies"
    );
}

#[test]
fn a_chord_typed_right_after_focus_is_modified() {
    // What the user actually does: hold Ctrl somewhere else, click into the
    // app, press K.  The app has to see the K as Ctrl+K.
    let mut fx = Fixture::new();
    fx.key(KEY_LEFTCTRL, true);

    fx.focus(fx.surface_id);
    fx.tap(KEY_K);

    assert_eq!(
        fx.take_log(),
        vec![
            Ev::Enter,
            Ev::Mods(MOD_CONTROL, 0),
            Ev::Key(KEY_K, true),
            Ev::Key(KEY_K, false),
        ],
        "Ctrl has to be stated ahead of the K it qualifies, or the app types a bare k"
    );
}

#[test]
fn focus_states_an_empty_modifier_set_too() {
    // Not just a nicety: without it the app keeps whatever it was last told,
    // and a client that had focus while Ctrl was held, lost it, and got it
    // back would still be holding Ctrl after the user let go.
    let mut fx = Fixture::new();
    fx.focus(fx.surface_id);
    assert_eq!(fx.take_log(), vec![Ev::Enter, Ev::Mods(0, 0)]);

    fx.key(KEY_LEFTCTRL, true);
    fx.focus(0);
    fx.key(KEY_LEFTCTRL, false);
    fx.take_log();

    fx.focus(fx.surface_id);

    assert_eq!(
        fx.take_log(),
        vec![Ev::Enter, Ev::Mods(0, 0)],
        "the release happened elsewhere, so focus has to say Ctrl is up"
    );
}

#[test]
fn focus_carries_the_capslock_latch() {
    // A locked modifier survives focus moving away and back, so it is the one
    // piece of state that is definitely still true when the client returns.
    let mut fx = Fixture::new();
    fx.focus(fx.surface_id);
    fx.tap(KEY_CAPSLOCK);
    fx.focus(0);
    fx.take_log();

    fx.focus(fx.surface_id);

    assert_eq!(
        fx.take_log(),
        vec![Ev::Enter, Ev::Mods(0, MOD_LOCK)],
        "CapsLock is latched on, and only a modifiers event can say so"
    );
}

#[test]
fn several_held_modifiers_arrive_together() {
    let mut fx = Fixture::new();
    fx.key(KEY_LEFTCTRL, true);
    fx.key(KEY_LEFTSHIFT, true);

    fx.focus(fx.surface_id);

    assert_eq!(
        fx.take_log(),
        vec![Ev::Enter, Ev::Mods(MOD_CONTROL | MOD_SHIFT, 0)],
        "one modifiers event carries the whole mask, not one per key"
    );
}
