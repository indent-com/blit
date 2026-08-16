//! The same contract, but judged by a real toolkit instead of by us.
//!
//! `text_input.rs` drives a client we wrote, so it proves we send what we
//! meant to send -- not that anything accepts it.  Serial semantics, the
//! double-buffered `enable`, and the requirement that `done` follow
//! `commit_string` are all rules a client is free to enforce, and a hand-
//! written client that ignores them cannot tell us we got them wrong.
//!
//! Chromium implements `zwp_text_input_v3` for real, so it is the judge.  The
//! page echoes whatever lands in its focused field back into `document.title`,
//! which Chromium turns into `xdg_toplevel.set_title` -- so the answer comes
//! back over the same Wayland connection, with no debugger attached.
//!
//! Ignored by default: it starts a browser.  Run it with
//! `cargo test -p blit-compositor --test text_input_chromium -- --ignored`.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use blit_compositor::{CompositorCommand, CompositorEvent, spawn_compositor};

/// ASCII first, then the part that has no key.  One string, so the reply
/// distinguishes the three outcomes that matter: `hi日本語` is the fix
/// working, a bare `hi` is the composed half still being dropped, and no
/// title at all is a harness that never got the field focused.
const TYPED: &str = "hi日本語";

/// What a composition in progress should look like on the way through.
const COMPOSING: &str = "にほn";

const PAGE: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>waiting</title>
<input id="f" autofocus>
<script>
  const f = document.getElementById('f');
  f.focus();
  // A composition shows up in `value` like anything else, so the title has
  // to say which one it is — otherwise the `input` that follows every
  // compositionupdate overwrites the pending text with the same string
  // under the committed label.
  let composing = false;
  f.addEventListener('compositionstart', () => { composing = true; });
  f.addEventListener('compositionend', () => { composing = false; });
  f.addEventListener('input', () => {
    document.title = (composing ? 'PRE:' : 'GOT:') + f.value;
  });
</script>
"#;

/// A field that moves itself by a known distance, halfway through the run.
///
/// Absolute positions are not comparable: the page's coordinates are
/// viewport-relative while `set_cursor_rectangle` is surface-relative, and
/// window decorations sit between the two.  A *displacement* cancels that
/// constant out, and at a 2x surface scale it also pins the logical→physical
/// conversion, which is the part of the forwarding that can be wrong while
/// still looking plausible.
const CARET_PAGE: &str = r#"<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>waiting</title>
<style>
  html, body { margin: 0; padding: 0; }
  #f { position: fixed; left: 120px; top: 80px; width: 200px; height: 30px;
       font-size: 16px; box-sizing: border-box; }
</style>
<input id="f" autofocus>
<script>
  const f = document.getElementById('f');
  f.focus();
  function report() {
    const r = f.getBoundingClientRect();
    document.title =
      `RECT:${Math.round(r.left)},${Math.round(r.top)},` +
      `${Math.round(r.width)},${Math.round(r.height)},` +
      `${devicePixelRatio}`;
  }
  report();
  addEventListener('resize', report);
  f.addEventListener('focus', report);
  setTimeout(() => {
    f.style.left = '220px';
    f.style.top = '180px';
    f.focus();
    report();
  }, 15000);
</script>
"#;

/// How far the page moves its field, in CSS pixels.
const FIELD_MOVE: (i32, i32) = (100, 100);

/// Kills the browser even when an assertion unwinds past it.
struct Browser(Child);

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Write `html` into a scratch dir and open it in a Chromium that speaks
/// text-input-v3 to `wayland_display`.
fn spawn_chromium(dir: &std::path::Path, html: &str, wayland_display: &str) -> Browser {
    std::fs::create_dir_all(dir).expect("scratch dir");
    let page = dir.join("page.html");
    std::fs::File::create(&page)
        .and_then(|mut f| f.write_all(html.as_bytes()))
        .expect("write page");
    Browser(
        Command::new("chromium")
            .args([
                "--ozone-platform=wayland",
                // Without this Chromium never binds zwp_text_input_v3 at all
                // and composed text has nowhere to go.
                "--enable-wayland-ime",
                "--wayland-text-input-version=3",
                "--no-sandbox",
                "--disable-gpu",
                "--no-first-run",
                "--noerrdialogs",
                "--disable-features=Translate",
            ])
            .arg(format!("--user-data-dir={}", dir.join("profile").display()))
            .arg(format!("--app=file://{}", page.display()))
            .env("WAYLAND_DISPLAY", wayland_display)
            .env("GDK_BACKEND", "wayland")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn chromium"),
    )
}

#[test]
#[ignore = "starts a real browser"]
fn chromium_inserts_composed_text() {
    if Command::new("chromium").arg("--version").output().is_err() {
        eprintln!("chromium not on PATH; skipping");
        return;
    }

    let dir = std::env::temp_dir().join(format!("blit-ime-{}", std::process::id()));
    let handle = spawn_compositor(false, Arc::new(|| {}), "");
    let _browser = spawn_chromium(&dir, PAGE, &handle.socket_name);

    // Chromium takes its time to come up, map a window, and lay out the page.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut surface_id = None;
    let mut inserted = String::new();
    let mut preedit = String::new();
    let mut typed = false;

    while Instant::now() < deadline {
        let Ok(ev) = handle.event_rx.recv_timeout(Duration::from_millis(250)) else {
            // A quiet moment after the window exists is the page having
            // settled: focus it, compose, then commit — the order a user
            // types in, so the commit has a preedit to replace.
            if let Some(id) = surface_id
                && !typed
            {
                handle
                    .command_tx
                    .send(CompositorCommand::SurfaceFocus { surface_id: id })
                    .expect("focus");
                std::thread::sleep(Duration::from_millis(500));
                handle
                    .command_tx
                    .send(CompositorCommand::Preedit {
                        text: COMPOSING.to_string(),
                        cursor: COMPOSING.len() as u16,
                    })
                    .expect("compose");
                std::thread::sleep(Duration::from_millis(300));
                handle
                    .command_tx
                    .send(CompositorCommand::TextInput {
                        text: TYPED.to_string(),
                    })
                    .expect("type");
                typed = true;
            }
            continue;
        };
        match ev {
            CompositorEvent::SurfaceCreated { surface_id: id, .. } => surface_id = Some(id),
            CompositorEvent::SurfaceTitle { title: t, .. } => {
                // With --nocapture this is the whole story in two lines:
                // what the app drew while composing, and what it kept.
                eprintln!("[title] {t}");
                if let Some(got) = t.strip_prefix("PRE:") {
                    preedit = got.to_string();
                } else if let Some(got) = t.strip_prefix("GOT:") {
                    inserted = got.to_string();
                    // Chromium retitles per keystroke; the last one is the
                    // whole field, so keep reading until it stops changing.
                    if inserted == TYPED {
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !inserted.is_empty(),
        "chromium never reported any text -- the field was never focused, \
         so this run says nothing about the input method"
    );
    assert_eq!(
        inserted, TYPED,
        "chromium should have inserted the composed characters too"
    );
    assert_eq!(
        preedit, COMPOSING,
        "chromium should have shown the composition while it was pending"
    );
}

/// The caret rectangle a real toolkit sends, in the space the browser client
/// draws in.
///
/// `set_cursor_rectangle` is surface-local and *logical*; the frame the web
/// client lays its canvas out in is physical.  At scale 1 the two agree and a
/// missing conversion would pass unnoticed, so this runs the surface at 2x
/// and asks the page where its field is in (scale-independent) CSS pixels.
/// The forwarded rectangle has to land inside twice that box.
#[test]
#[ignore = "starts a real browser"]
fn chromium_reports_where_its_caret_is() {
    if Command::new("chromium").arg("--version").output().is_err() {
        eprintln!("chromium not on PATH; skipping");
        return;
    }

    let dir = std::env::temp_dir().join(format!("blit-ime-caret-{}", std::process::id()));
    let handle = spawn_compositor(false, Arc::new(|| {}), "");
    let _browser = spawn_chromium(&dir, CARET_PAGE, &handle.socket_name);

    const SCALE_120: u16 = 240;
    let scale = i32::from(SCALE_120) / 120;
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut surface_id = None;
    // Field position, as the page reports it, and the caret Chromium sent
    // while composing there. One pair before the page moves its field, one
    // after.
    /// Where the page put its field, and the caret we were handed there.
    type Measurement = ((i32, i32), (i32, i32, i32, i32));
    let mut field: Option<(i32, i32)> = None;
    let mut before: Option<Measurement> = None;
    let mut after: Option<Measurement> = None;
    let mut driven = false;

    /// A caret rectangle only reaches us while something is composing:
    /// Chromium reports its caret bounds when an input method needs them,
    /// not merely because a field has focus.
    fn compose(handle: &blit_compositor::CompositorHandle) {
        handle
            .command_tx
            .send(CompositorCommand::Preedit {
                text: COMPOSING.to_string(),
                cursor: COMPOSING.len() as u16,
            })
            .expect("compose");
    }

    while Instant::now() < deadline {
        let Ok(ev) = handle.event_rx.recv_timeout(Duration::from_millis(250)) else {
            // A quiet moment after the window exists: put it at a known size
            // and scale, hand it the keyboard so its field enables its text
            // input, and start a composition.
            if let Some(id) = surface_id
                && !driven
            {
                handle
                    .command_tx
                    .send(CompositorCommand::SurfaceResize {
                        surface_id: id,
                        width: 800,
                        height: 600,
                        scale_120: SCALE_120,
                    })
                    .expect("resize");
                std::thread::sleep(Duration::from_millis(1500));
                handle
                    .command_tx
                    .send(CompositorCommand::SurfaceFocus { surface_id: id })
                    .expect("focus");
                std::thread::sleep(Duration::from_millis(500));
                compose(&handle);
                driven = true;
            }
            continue;
        };
        match ev {
            CompositorEvent::SurfaceCreated { surface_id: id, .. } => surface_id = Some(id),
            CompositorEvent::SurfaceTitle { title, .. } => {
                eprintln!("[title] {title}");
                if let Some(rest) = title.strip_prefix("RECT:") {
                    let n: Vec<i32> = rest
                        .split(',')
                        .filter_map(|v| v.parse::<f64>().ok())
                        .map(|v| v as i32)
                        .collect();
                    if n.len() >= 4 {
                        let moved = field.is_some_and(|(x, y)| (x, y) != (n[0], n[1]));
                        field = Some((n[0], n[1]));
                        // The page just moved its field: compose again there.
                        if moved && before.is_some() {
                            compose(&handle);
                        }
                    }
                }
            }
            CompositorEvent::SurfaceTextInput {
                cursor_rect: Some(rect),
                ..
            } => {
                eprintln!("[caret] {rect:?} field={field:?}");
                let Some(at) = field else { continue };
                if before.is_none() {
                    before = Some((at, rect));
                } else if before.is_some_and(|(was, _)| was != at) {
                    after = Some((at, rect));
                    break;
                }
            }
            _ => {}
        }
    }

    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);

    let ((ax, ay), (cx, cy, _, ch)) = before.expect(
        "chromium never sent a cursor rectangle -- with none forwarded the \
         browser has nowhere to park its IME capture element",
    );
    let ((bx, by), (dx, dy, ..)) = after.expect(
        "the field never moved under a second composition, so nothing here \
         pins the coordinate conversion",
    );

    assert_eq!(
        (bx - ax, by - ay),
        FIELD_MOVE,
        "the page should have moved its field by exactly this much"
    );
    // The caret follows in physical pixels: the same move, times the scale.
    assert_eq!(
        (dx - cx, dy - cy),
        (FIELD_MOVE.0 * scale, FIELD_MOVE.1 * scale),
        "a field that moved {FIELD_MOVE:?} CSS px at {scale}x should move the \
         forwarded caret by {scale}x that; got ({cx},{cy}) then ({dx},{dy})"
    );
    // A caret is a text line tall, in physical pixels: a rectangle still in
    // logical units would come back at half this.
    assert!(
        (24..=80).contains(&ch),
        "caret height {ch} does not look like a 16px line at {scale}x"
    );
}
