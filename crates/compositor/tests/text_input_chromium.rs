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

/// Kills the browser even when an assertion unwinds past it.
struct Browser(Child);

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "starts a real browser"]
fn chromium_inserts_composed_text() {
    if Command::new("chromium").arg("--version").output().is_err() {
        eprintln!("chromium not on PATH; skipping");
        return;
    }

    let dir = std::env::temp_dir().join(format!("blit-ime-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let page = dir.join("page.html");
    std::fs::File::create(&page)
        .and_then(|mut f| f.write_all(PAGE.as_bytes()))
        .expect("write page");

    let handle = spawn_compositor(false, Arc::new(|| {}), "");

    let _browser = Browser(
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
            .env("WAYLAND_DISPLAY", &handle.socket_name)
            .env("GDK_BACKEND", "wayland")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn chromium"),
    );

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
