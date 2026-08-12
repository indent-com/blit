//! Composed text has to arrive as text, because it cannot arrive as keys.
//!
//! The browser resolves a keystroke to a character and sends the character,
//! so layout differences stay the browser's problem.  The compositor turns
//! that character back into a US-QWERTY press/release pair -- which works
//! for exactly the characters US-QWERTY has.  Everything an input method
//! exists to produce (CJK, accented Latin, emoji) has no key to synthesise
//! and falls off the end of that table.
//!
//! `zwp_text_input_v3` is the protocol for precisely this case, and the
//! compositor advertises it.  A client that binds it, is told `enter`, and
//! answers `enable` has declared itself ready to be given text.  These tests
//! hold the compositor to that: what the client negotiated is what it gets,
//! and a client that negotiated nothing still gets the keys it always did.
//!
//! The division is by character, not by client.  A key that exists keeps
//! going out as a key even to a client with an input method attached --
//! an app reading scancodes (a game's WASD, a chat window that also moves a
//! character) must not lose them the moment its text field takes focus.
//! Only what the keymap cannot say at all is worth routing elsewhere.

#![cfg(target_os = "linux")]

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use wayland_client::protocol::{wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3 as ti_mgr, zwp_text_input_v3 as ti,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use blit_compositor::{CompositorCommand, CompositorEvent, CompositorHandle, spawn_compositor};

/// Japanese, i.e. the whole reason an input method is in the loop.
const COMPOSED: &str = "日本語";

// evdev keycodes for the letters these tests type.
const KEY_C: u32 = 46;
const KEY_A: u32 = 30;
const KEY_F: u32 = 33;
const KEY_H: u32 = 35;
const KEY_I: u32 = 23;

/// One log of both delivery channels, because the interesting property of a
/// mixed string is the order the two arrive in relative to each other.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    Key(u32, bool),
    Commit(String),
    /// preedit_string(text, cursor_begin, cursor_end)
    Preedit(String, i32, i32),
    Done(u32),
}

/// Press and release, the pair a synthesised character comes as.
fn tap(keycode: u32) -> [Ev; 2] {
    [Ev::Key(keycode, true), Ev::Key(keycode, false)]
}

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    text_input_manager: Option<ti_mgr::ZwpTextInputManagerV3>,
    /// Whether our text input has been told it holds focus.
    text_input_entered: bool,
    /// Keys and text-input events together, in arrival order.
    log: Vec<Ev>,
}

impl App {
    fn committed(&self) -> Vec<&str> {
        self.log
            .iter()
            .filter_map(|e| match e {
                Ev::Commit(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }
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
            "zwp_text_input_manager_v3" => {
                state.text_input_manager =
                    Some(registry.bind::<ti_mgr::ZwpTextInputManagerV3, _, _>(name, 1, qh, ()));
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
        if let wl_keyboard::Event::Key { key, state: s, .. } = event {
            let pressed = matches!(s.into_result(), Ok(wl_keyboard::KeyState::Pressed));
            state.log.push(Ev::Key(key, pressed));
        }
    }
}

impl Dispatch<ti::ZwpTextInputV3, ()> for App {
    fn event(
        state: &mut Self,
        _: &ti::ZwpTextInputV3,
        event: ti::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ti::Event::Enter { .. } => state.text_input_entered = true,
            ti::Event::Leave { .. } => state.text_input_entered = false,
            ti::Event::CommitString { text } => {
                state.log.push(Ev::Commit(text.unwrap_or_default()));
            }
            ti::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => state.log.push(Ev::Preedit(
                text.unwrap_or_default(),
                cursor_begin,
                cursor_end,
            )),
            ti::Event::Done { serial } => state.log.push(Ev::Done(serial)),
            _ => {}
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore xdg_toplevel::XdgToplevel);
delegate_noop!(App: ignore wl_seat::WlSeat);
delegate_noop!(App: ignore ti_mgr::ZwpTextInputManagerV3);

struct Fixture {
    app: App,
    queue: EventQueue<App>,
    conn: Connection,
    text_input: Option<ti::ZwpTextInputV3>,
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
    /// A mapped, focused toplevel with a `wl_keyboard`.  `text_input` decides
    /// whether this client negotiates an input method at all -- the two
    /// halves of the contract under test.
    fn new(text_input: bool) -> Self {
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

        // The keyboard is what the synthesised-key path arrives on, so both
        // kinds of client need it to tell delivery from silence.
        let _keyboard = seat.get_keyboard(&qh, ());
        let text_input = text_input.then(|| {
            let mgr = app
                .text_input_manager
                .clone()
                .expect("zwp_text_input_manager_v3 advertised");
            mgr.get_text_input(&seat, &qh, ())
        });

        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        surface.commit();
        queue.roundtrip(&mut app).expect("map roundtrip");

        let mut fx = Self {
            app,
            queue,
            conn,
            text_input,
            _surface: surface,
            _toplevel: toplevel,
            _xdg_surface: xdg_surface,
            handle: Some(handle),
        };
        let surface_id = fx.surface_id();
        fx.command(CompositorCommand::SurfaceFocus { surface_id });
        fx.settle();
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

    /// Answer `enter` the way a client with a focused text field does.
    /// Returns the number of commit requests issued, which is the serial the
    /// compositor owes us back on `done`.
    fn enable_text_input(&mut self) -> u32 {
        let ti = self.text_input.as_ref().expect("client has a text input");
        ti.enable();
        ti.commit();
        self.settle();
        1
    }

    fn type_text(&mut self, text: &str) {
        self.command(CompositorCommand::TextInput {
            text: text.to_string(),
        });
        self.settle();
    }

    fn compose(&mut self, text: &str, cursor: u16) {
        self.command(CompositorCommand::Preedit {
            text: text.to_string(),
            cursor,
        });
        self.settle();
    }

    fn take_log(&mut self) -> Vec<Ev> {
        std::mem::take(&mut self.app.log)
    }

    fn text_input_event(&self, timeout: Duration) -> Option<(u16, bool, bool, u32, u32)> {
        let handle = self.handle.as_ref().expect("compositor running");
        let deadline = std::time::Instant::now() + timeout;
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match handle.event_rx.recv_timeout(left) {
                Ok(CompositorEvent::SurfaceTextInput {
                    surface_id,
                    enabled,
                    requested,
                    hint,
                    purpose,
                }) => return Some((surface_id, enabled, requested, hint, purpose)),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
        None
    }

    /// The surface id the compositor gave our toplevel.
    fn surface_id(&mut self) -> u16 {
        let handle = self.handle.as_ref().expect("compositor running");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match handle.event_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(CompositorEvent::SurfaceCreated { surface_id, .. }) => return surface_id,
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        panic!("compositor never announced the surface");
    }
}

#[test]
fn composed_text_reaches_a_client_that_asked_for_it() {
    let mut fx = Fixture::new(true);
    assert!(
        fx.app.text_input_entered,
        "a focused toplevel's text input should be told it has focus"
    );
    let commits = fx.enable_text_input();

    fx.type_text(COMPOSED);

    assert_eq!(
        fx.app.log,
        vec![Ev::Commit(COMPOSED.to_string()), Ev::Done(commits)],
        "composed text should arrive as commit_string, and only a done applies it"
    );
}

#[test]
fn the_done_serial_counts_that_object_s_commits() {
    // A serial that isn't the client's own commit count tells the client to
    // apply the text but distrust the state -- the spec's escape hatch for a
    // compositor answering a stale request, which this is not.
    let mut fx = Fixture::new(true);
    fx.enable_text_input();
    // A second round of pending state, as a client re-arming its text field
    // after a cursor move does.
    {
        let ti = fx.text_input.as_ref().expect("text input");
        ti.set_cursor_rectangle(0, 0, 10, 10);
        ti.commit();
    }
    fx.settle();

    fx.type_text(COMPOSED);

    assert_eq!(
        fx.app.log,
        vec![Ev::Commit(COMPOSED.to_string()), Ev::Done(2)],
        "two commit requests were issued, so the done serial should be 2"
    );
}

#[test]
fn a_client_without_a_text_input_still_gets_ascii_keys() {
    let mut fx = Fixture::new(false);

    fx.type_text("hi");

    assert_eq!(
        fx.app.log,
        [tap(KEY_H), tap(KEY_I)].concat(),
        "a client that negotiated no input method keeps the synthesised keys"
    );
}

#[test]
fn keys_that_exist_stay_keys_even_with_an_input_method_up() {
    // The regression this guards: an app whose text field is focused still
    // reads scancodes for everything else it does.  Rerouting characters the
    // keymap can express would take those away exactly when it can least
    // afford it.
    let mut fx = Fixture::new(true);
    fx.enable_text_input();

    fx.type_text("hi");

    assert_eq!(
        fx.app.log,
        [tap(KEY_H), tap(KEY_I)].concat(),
        "an enabled input method should not swallow keys the keymap has"
    );
}

#[test]
fn an_unenabled_text_input_is_not_handed_text() {
    // Bound but never enabled: a client with an input method object and no
    // focused text field.  Nothing has offered to receive text, so the
    // characters go nowhere -- but they must not go to the input method.
    let mut fx = Fixture::new(true);

    fx.type_text(COMPOSED);

    assert!(
        fx.app.committed().is_empty(),
        "an input method that was never enabled should not be handed text"
    );
}

#[test]
fn enable_only_counts_once_the_client_commits_it() {
    // enable is double-buffered: it applies on the next commit request.
    // Acting on the uncommitted request delivers text into a text input the
    // client has not actually turned on yet.
    let mut fx = Fixture::new(true);
    fx.text_input.as_ref().expect("text input").enable();
    fx.settle();

    fx.type_text(COMPOSED);

    assert!(
        fx.app.committed().is_empty(),
        "an enable with no commit behind it should not enable anything"
    );
}

#[test]
fn committed_enable_and_content_type_are_forwarded_to_viewers() {
    let mut fx = Fixture::new(true);
    let ti = fx.text_input.as_ref().expect("text input");
    ti.enable();
    ti.set_content_type(
        ti::ContentHint::Spellcheck | ti::ContentHint::AutoCapitalization,
        ti::ContentPurpose::Email,
    );
    fx.settle();
    assert_eq!(
        fx.text_input_event(Duration::from_millis(25)),
        None,
        "pending state must not escape before commit"
    );

    fx.text_input.as_ref().expect("text input").commit();
    fx.settle();
    let (_, enabled, requested, hint, purpose) = fx
        .text_input_event(Duration::from_secs(1))
        .expect("committed enable event");
    assert!(enabled);
    assert!(requested);
    assert_eq!(
        hint,
        (ti::ContentHint::Spellcheck | ti::ContentHint::AutoCapitalization).bits()
    );
    assert_eq!(purpose, ti::ContentPurpose::Email as u32);

    // Metadata-only commits preserve state but are not new requests to show
    // a keyboard the user may have dismissed.
    let ti = fx.text_input.as_ref().expect("text input");
    ti.set_cursor_rectangle(1, 2, 3, 4);
    ti.commit();
    fx.settle();
    assert_eq!(fx.text_input_event(Duration::from_millis(25)), None);

    let ti = fx.text_input.as_ref().expect("text input");
    ti.disable();
    ti.commit();
    fx.settle();
    let (_, enabled, requested, hint, purpose) = fx
        .text_input_event(Duration::from_secs(1))
        .expect("committed disable event");
    assert!(!enabled);
    assert!(!requested);
    assert_eq!((hint, purpose), (0, 0));
}

#[test]
fn a_composition_in_progress_is_shown_before_it_is_committed() {
    // The browser captures the composition in a 1px transparent textarea, so
    // the app drawing the preedit is the only way the user can read what they
    // have typed so far.
    let mut fx = Fixture::new(true);
    let commits = fx.enable_text_input();

    fx.compose("に", 3);

    assert_eq!(
        fx.take_log(),
        vec![Ev::Preedit("に".to_string(), 3, 3), Ev::Done(commits)],
        "the pending composition should reach the app as a preedit"
    );
}

#[test]
fn committing_withdraws_the_preedit_before_inserting() {
    // Order matters to a real text field, not just to this log: a
    // commit_string applied while the composition is still up lands at the
    // composition's anchor instead of the caret.  Chromium turns "hi日本語"
    // into "日本語hi" when the preedit is left standing.
    let mut fx = Fixture::new(true);
    let commits = fx.enable_text_input();
    fx.compose("にほn", 4);
    fx.take_log();

    fx.type_text("日本");

    assert_eq!(
        fx.take_log(),
        vec![
            Ev::Done(commits),
            Ev::Commit("日本".to_string()),
            Ev::Done(commits)
        ],
        "the composition should be withdrawn first, then the text inserted"
    );
}

#[test]
fn an_ascii_commit_still_takes_back_the_preedit() {
    // The trap: "hi" is typeable, so it commits as synthesised keys, which
    // send no `done` at all.  Without the withdrawal above, nothing would
    // clear the preedit and the app would draw the abandoned composition
    // forever.
    let mut fx = Fixture::new(true);
    let commits = fx.enable_text_input();
    fx.compose("hi", 2);
    fx.take_log();

    fx.type_text("hi");

    assert_eq!(
        fx.take_log(),
        [
            &[Ev::Done(commits)],
            tap(KEY_H).as_slice(),
            tap(KEY_I).as_slice()
        ]
        .concat(),
        "a key-path commit owes the preedit a withdrawal of its own"
    );
}

#[test]
fn a_cancelled_composition_withdraws_the_preedit() {
    let mut fx = Fixture::new(true);
    let commits = fx.enable_text_input();
    fx.compose("に", 3);
    fx.take_log();

    fx.compose("", 0);

    assert_eq!(
        fx.take_log(),
        vec![Ev::Preedit(String::new(), 0, 0), Ev::Done(commits)],
        "an empty preedit should withdraw what the app is drawing"
    );
}

#[test]
fn withdrawing_a_preedit_nobody_shows_says_nothing() {
    // A client that never had a preedit does not need a state reset.
    let mut fx = Fixture::new(true);
    fx.enable_text_input();
    fx.take_log();

    fx.compose("", 0);

    assert_eq!(fx.take_log(), vec![], "there was nothing to take back");
}

#[test]
fn a_client_without_an_input_method_is_sent_no_preedit() {
    // A preedit has nowhere to go but the app's own text field.
    let mut fx = Fixture::new(true);

    fx.compose("に", 3);

    assert_eq!(
        fx.take_log(),
        vec![],
        "an unenabled input method should not be handed a composition"
    );
}

#[test]
fn a_mixed_string_arrives_in_order() {
    // "café" splits across both channels.  The app inserts what it receives
    // in the order it receives it, so a commit_string that overtakes the
    // keys before it spells "féca".
    let mut fx = Fixture::new(true);
    let commits = fx.enable_text_input();

    fx.type_text("café");

    assert_eq!(
        fx.app.log,
        [
            tap(KEY_C).as_slice(),
            tap(KEY_A).as_slice(),
            tap(KEY_F).as_slice(),
            &[Ev::Commit("é".to_string()), Ev::Done(commits)],
        ]
        .concat(),
        "the keyed and composed halves should interleave in source order"
    );
}
