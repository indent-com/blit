//! Direct touch judged by Chromium's gesture stack rather than a protocol client.
//!
//! Ignored by default because it starts Chromium. Run with:
//! `cargo test -p blit-compositor --test direct_touch_chromium -- --ignored --nocapture`.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use blit_compositor::{
    CompositorCommand, CompositorEvent, TouchPhase, TouchPoint, spawn_compositor,
};

const PAGE: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>READY</title>
<style>
  html, body { margin: 0; height: 100%; overflow: hidden; background: #fff; }
  #scroller { width: 100%; height: 100%; overflow-y: auto; touch-action: pan-y; }
  #marker { width: 100%; height: 1000000px;
            background: linear-gradient(#fff, #bbb); }
</style>
<div id="scroller"><div id="marker"></div></div>
<script>
  document.title = 'READY';
  scroller.addEventListener('scroll', () => {
    document.title = 'Y:' + Math.round(scroller.scrollTop);
  }, { passive: true });
</script>
"#;

struct Browser(Child);

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "starts a real browser"]
fn chromium_flings_after_direct_touch_lifts() {
    if Command::new("chromium").arg("--version").output().is_err() {
        eprintln!("chromium not on PATH; skipping");
        return;
    }

    let dir = std::env::temp_dir().join(format!("blit-touch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let page = dir.join("page.html");
    std::fs::File::create(&page)
        .and_then(|mut file| file.write_all(PAGE.as_bytes()))
        .expect("write page");

    let handle = spawn_compositor(false, Arc::new(|| {}), "");
    handle
        .command_tx
        .send(CompositorCommand::SetTouchEnabled { enabled: true })
        .expect("enable direct touch");
    handle.wake();

    let browser = Browser(
        Command::new("chromium")
            .args([
                "--ozone-platform=wayland",
                "--no-sandbox",
                "--disable-gpu",
                "--no-first-run",
                "--noerrdialogs",
                "--disable-features=Translate",
                "--touch-events=enabled",
                "--window-size=800,700",
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

    let startup_deadline = Instant::now() + Duration::from_secs(60);
    let surface_id = loop {
        assert!(
            Instant::now() < startup_deadline,
            "Chromium page never became ready"
        );
        let Ok(event) = handle.event_rx.recv_timeout(Duration::from_millis(250)) else {
            continue;
        };
        if let CompositorEvent::SurfaceTitle { surface_id, title } = event
            && title == "READY"
        {
            break surface_id;
        }
    };

    // `set_title("READY")` is sent before Chromium's first wl_surface commit.
    // Input before that commit reaches the protocol object but not a live
    // RenderWidgetHost, so wait until the page is actually mapped.
    loop {
        assert!(
            Instant::now() < startup_deadline,
            "Chromium page never mapped"
        );
        let Ok(event) = handle.event_rx.recv_timeout(Duration::from_millis(250)) else {
            continue;
        };
        if let CompositorEvent::SurfaceCommit { surface_id: id, .. } = event
            && id == surface_id
        {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(250));

    handle
        .command_tx
        .send(CompositorCommand::SurfaceFocus { surface_id })
        .expect("focus Chromium");

    let send = |phase, time_ms, y| {
        handle
            .command_tx
            .send(CompositorCommand::Touch {
                owner_id: 1,
                surface_id,
                phase,
                time_ms,
                contacts: vec![TouchPoint {
                    id: 10,
                    x: 400.0,
                    y,
                }],
            })
            .expect("send direct touch");
    };
    // Long enough to cross the pacer's bounded-backlog threshold. The queued
    // tail must remain continuous with already-played history or Chromium sees
    // a teleport and suppresses inertia at lift.
    send(TouchPhase::Down, 1_000, 620.0);
    for i in 1..=5u32 {
        send(
            TouchPhase::Motion,
            1_000 + i * 8,
            620.0 - f64::from(i) * 4.0,
        );
    }
    handle.wake();
    std::thread::sleep(Duration::from_millis(80));
    for i in 6..=120u32 {
        send(
            TouchPhase::Motion,
            1_000 + i * 8,
            620.0 - f64::from(i) * 4.0,
        );
    }
    send(TouchPhase::Up, 1_968, 140.0);
    let gesture_sent = Instant::now();
    handle.wake();

    let observation_deadline = gesture_sent + Duration::from_secs(3);
    let mut first_post_lift = None;
    let mut late_max = 0u32;
    while Instant::now() < observation_deadline {
        let Ok(event) = handle.event_rx.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        let CompositorEvent::SurfaceTitle {
            surface_id: id,
            title,
        } = event
        else {
            continue;
        };
        if id != surface_id {
            continue;
        }
        let Some(y) = title
            .strip_prefix("Y:")
            .and_then(|value| value.parse().ok())
        else {
            continue;
        };
        let elapsed = gesture_sent.elapsed();
        if elapsed >= Duration::from_millis(200) {
            first_post_lift.get_or_insert(y);
            late_max = late_max.max(y);
        }
        eprintln!("[scroll +{elapsed:?}] {y}");
    }

    let first_post_lift = first_post_lift.expect("the direct touch never scrolled Chromium");
    assert!(
        late_max > first_post_lift,
        "scroll stopped with the finger: first post-lift={first_post_lift}, late={late_max}"
    );

    // Burn through Chromium's 32-id range cheaply, then make the next gesture
    // the one whose inertia we judge. Wayland releases an id at `up`, so every
    // tap should reuse slot zero; the former seat-global counter instead made
    // the final fling id 32, where Chromium stops recognizing it as a fling.
    for tap in 0..31u32 {
        let base = 10_000 + tap * 2;
        send(TouchPhase::Down, base, 620.0);
        send(TouchPhase::Up, base + 1, 620.0);
    }
    handle.wake();
    std::thread::sleep(Duration::from_millis(100));

    let base = 20_000;
    send(TouchPhase::Down, base, 620.0);
    handle.wake();
    for motion in 1..=10u32 {
        std::thread::sleep(Duration::from_millis(8));
        send(
            TouchPhase::Motion,
            base + motion * 8,
            620.0 - f64::from(motion) * 24.0,
        );
        handle.wake();
    }
    send(TouchPhase::Up, base + 84, 380.0);
    let lifted = Instant::now();
    handle.wake();

    let deadline = lifted + Duration::from_millis(400);
    let mut early = None;
    let mut latest_y = late_max;
    while Instant::now() < deadline {
        if early.is_none() && lifted.elapsed() >= Duration::from_millis(100) {
            early = Some(latest_y);
        }
        let Ok(event) = handle.event_rx.recv_timeout(Duration::from_millis(10)) else {
            continue;
        };
        if let CompositorEvent::SurfaceTitle {
            surface_id: id,
            title,
        } = event
            && id == surface_id
            && let Some(y) = title
                .strip_prefix("Y:")
                .and_then(|value| value.parse().ok())
        {
            latest_y = y;
        }
    }
    let early = early.unwrap_or(latest_y);
    assert!(
        latest_y > early + 500,
        "gesture after 32 released contacts stopped at lift: early={early}, late={latest_y}"
    );

    drop(browser);
    handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
