//! Audio capture pipeline: PipeWire spawn → pw-cat pipe → Opus encode.
//!
//! Each compositor instance gets its own PipeWire + pipewire-pulse pair.
//! Apps connect via PulseAudio; PipeWire mixes into a null sink; pw-cat
//! captures the monitor source and writes interleaved f32 PCM to stdout.
//! We read that pipe, frame into 20 ms chunks, and Opus-encode for delivery.

use blit_remote::{AUDIO_FRAME_CODEC_OPUS, S2C_AUDIO_FRAME};
use opus::{Application, Channels, Encoder as OpusEncoder};
use std::collections::VecDeque;
use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

/// Returns a closure suitable for `Command::pre_exec` that sets
/// `PR_SET_PDEATHSIG(SIGTERM)` so the child is killed when the parent (blit
/// server) dies — even via SIGKILL where Rust destructors can't run.
///
fn pdeathsig_hook() -> impl FnMut() -> std::io::Result<()> {
    // SAFETY: `prctl(PR_SET_PDEATHSIG, …)` is async-signal-safe and runs in
    // the child between fork and exec.
    || unsafe {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// 48 kHz, stereo, 20 ms = 960 samples per channel = 1920 interleaved samples.
const FRAME_SAMPLES: usize = 960;
const CHANNELS: usize = 2;
const FRAME_FLOATS: usize = FRAME_SAMPLES * CHANNELS;
/// Maximum Opus packet size (RFC 6716 recommends 4000 bytes as upper bound).
const MAX_OPUS_PACKET: usize = 4000;

/// Default Opus bitrate in bits/sec.
pub const DEFAULT_BITRATE: i32 = 64_000;

/// Server-side ring buffer depth: 200 ms = 10 Opus frames at 20 ms.
const RING_CAPACITY: usize = 10;

/// Minimum interval between sub-process heal attempts.
const HEAL_COOLDOWN: Duration = Duration::from_secs(1);
/// Maximum sub-process restarts in a burst window before giving up.
const MAX_HEALS: u32 = 5;
/// Duration of the burst window for counting heal attempts.
const HEAL_WINDOW: Duration = Duration::from_secs(30);

/// An encoded Opus frame ready for wire delivery.
#[derive(Clone)]
pub struct OpusFrame {
    /// Wall-clock milliseconds since the compositor epoch — same timebase
    /// as video frame timestamps for A/V sync.
    pub timestamp: u32,
    /// Opus-encoded bytes.
    pub data: Vec<u8>,
}

/// Manages the PipeWire child processes and produces Opus frames.
pub struct AudioPipeline {
    dbus_child: Child,
    pipewire_child: Child,
    wireplumber_child: Option<Child>,
    pipewire_pulse_child: Child,
    pw_cat_child: Child,
    /// Receives encoded Opus frames from the reader/encoder task.
    opus_rx: mpsc::Receiver<OpusFrame>,
    /// Recent frames for catch-up on new subscribers.
    ring: VecDeque<OpusFrame>,
    /// The XDG_RUNTIME_DIR used by this pipeline's PipeWire instance.
    pub runtime_dir: PathBuf,
    /// True when the pipeline is still running.
    alive: bool,
    /// Send bitrate updates to the encoder task.
    bitrate_tx: tokio::sync::watch::Sender<i32>,
    /// Shared flag set to `false` when the reader/encoder task exits.
    /// Lets `is_alive()` detect a dead encoder even if pw-cat hasn't exited.
    encoder_alive: Arc<AtomicBool>,
    /// True while at least one client is subscribed to audio. When false
    /// the encoder drains the pipe but skips the Opus encode step.
    has_listener: Arc<AtomicBool>,
    /// D-Bus session bus address for restarting sub-processes.
    dbus_address: String,
    /// Cloned sender for spawning new encoder tasks on pw-cat restart.
    opus_tx: mpsc::Sender<OpusFrame>,
    /// Verbose logging flag.
    verbose: bool,
    /// Shared time origin for A/V sync timestamps.
    epoch: Instant,
    /// Last sub-process heal attempt timestamp.
    last_heal: Option<Instant>,
    /// Start of the current heal burst window.
    first_heal_at: Option<Instant>,
    /// Number of heals in the current burst window.
    heals: u32,
}

/// PipeWire configuration template.
const PIPEWIRE_CONF_TEMPLATE: &str = r#"
context.properties = {
    core.daemon          = true
    core.name            = pipewire-0
    default.clock.rate   = 48000
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-access }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-spa-node-factory }
]
context.objects = [
    {   factory = adapter
        args = {
            factory.name          = support.null-audio-sink
            node.name             = blit-sink
            media.class           = Audio/Sink
            object.linger         = true
            audio.position        = [ FL FR ]
            audio.rate            = 48000
            monitor.channel-volumes = true
            monitor.passthrough     = true
        }
    }
]
"#;

/// Minimal WirePlumber configuration: only stream linking policy.
/// No ALSA, Bluetooth, camera, portal, MPRIS, or device reservation —
/// those conflict with the system WirePlumber on the same D-Bus.
///
/// `hardware.audio` MUST stay enabled (the default) — it contains
/// `policy.node`, the module that links playback streams to sinks.
/// Without it, apps like mpv hang because their audio stream is never
/// connected to blit-sink.  We disable only the sub-features we don't
/// need (ALSA monitor, device reservation).
const WIREPLUMBER_CONF_TEMPLATE: &str = r#"
wireplumber.profiles = {
  main = {
    support.dbus = disabled
    support.portal-permissionstore = disabled
    support.reserve-device = disabled
    # hardware.audio stays enabled — its policy.node links streams to sinks.
    hardware.bluetooth = disabled
    hardware.video-capture = disabled
    monitor.alsa = disabled
    monitor.alsa.reserve-device = disabled
    monitor.bluez = disabled
    monitor.bluez.midi = disabled
    monitor.bluez.seat-monitoring = disabled
    monitor.libcamera = disabled
    monitor.v4l2 = disabled
  }
}
"#;

/// Resolve a program to an absolute path by searching $PATH.
fn find_program(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        let candidate = Path::new(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Check whether the required PipeWire and D-Bus binaries are available.
pub fn pipewire_available() -> bool {
    missing_pipewire_binaries().is_empty()
}

/// Returns the list of required PipeWire / D-Bus binaries that are not
/// found on `$PATH`.  Empty list means audio can run.
pub fn missing_pipewire_binaries() -> Vec<&'static str> {
    ["pipewire", "pipewire-pulse", "pw-cat", "dbus-daemon"]
        .into_iter()
        .filter(|name| find_program(name).is_none())
        .collect()
}

/// Poll for a socket file to appear, sleeping 50 ms between checks.
/// Returns `true` if the socket appeared within `timeout`, `false` otherwise.
/// Falls back gracefully on timeout — the caller proceeds with a best-effort
/// attempt rather than failing hard.
fn wait_for_socket(path: &Path, timeout: std::time::Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// Look up blit-sink's PipeWire object serial via `pw-cli ls Node`.
/// Returns `None` if the serial can't be determined.
fn lookup_sink_serial(runtime_dir: &Path) -> Option<String> {
    let pipewire_remote = runtime_dir.join("pipewire-0");
    let output = Command::new("pw-cli")
        .args(["ls", "Node"])
        .env("PIPEWIRE_REMOTE", &pipewire_remote)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);

    // Strategy 1: serial appears after node.name in the pw-cli output.
    let mut serial = None;
    let mut in_blit_sink = false;
    for line in text.lines() {
        if line.contains("node.name") && line.contains("blit-sink") {
            in_blit_sink = true;
        } else if in_blit_sink && line.contains("object.serial") {
            serial = line.split('"').nth(1).map(|s| s.to_string());
            break;
        } else if line.starts_with('\t') && line.contains("id ") {
            in_blit_sink = false;
        }
    }
    if serial.is_some() {
        return serial;
    }

    // Strategy 2: serial appears before node.name (some PipeWire versions).
    let mut current_serial = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("object.serial = \"") {
            current_serial = rest.strip_suffix('"').map(|s| s.to_string());
        }
        if trimmed.contains("node.name") && trimmed.contains("blit-sink") {
            return current_serial;
        }
    }
    None
}

impl AudioPipeline {
    /// Spawn a new PipeWire instance and start capturing audio.
    ///
    /// `runtime_dir` is the XDG_RUNTIME_DIR for this compositor instance.
    /// `instance_id` is used to name the PipeWire remote uniquely.
    /// `bitrate` is the Opus encoder bitrate in bits/sec (0 = default).
    /// `epoch` is the shared time origin (same `Instant` used by video
    /// timestamps) so audio and video share a common timebase for A/V sync.
    pub fn spawn(
        runtime_dir: &Path,
        instance_id: u16,
        bitrate: i32,
        verbose: bool,
        epoch: Instant,
    ) -> Result<Self, String> {
        // Use a private subdirectory so the PulseAudio socket doesn't
        // collide with the system's or with other blit instances.
        let audio_dir = runtime_dir.join(format!("blit-audio-{instance_id}"));

        // Remove leftovers from a previous unclean exit so we don't trip
        // over stale PipeWire/pulse sockets ("Address already in use").
        if audio_dir.exists() {
            let _ = std::fs::remove_dir_all(&audio_dir);
        }

        std::fs::create_dir_all(&audio_dir)
            .map_err(|e| format!("failed to create audio runtime dir: {e}"))?;

        // Write the config at $audio_dir/pipewire/pipewire.conf so that
        // setting XDG_CONFIG_HOME=$audio_dir makes PipeWire pick it up
        // from $XDG_CONFIG_HOME/pipewire/pipewire.conf — which takes
        // priority over system / nix-store configs on all versions.
        let conf_dir = audio_dir.join("pipewire");
        std::fs::create_dir_all(&conf_dir)
            .map_err(|e| format!("failed to create PipeWire config dir: {e}"))?;
        let conf_path = conf_dir.join("pipewire.conf");
        std::fs::write(&conf_path, PIPEWIRE_CONF_TEMPLATE)
            .map_err(|e| format!("failed to write PipeWire config: {e}"))?;

        // 0. Start a private D-Bus session bus.
        //    PipeWire modules (rt, portal, jackdbus-detect, fallback-sink)
        //    need a session bus.  Without one the daemon fails to initialise
        //    in headless environments that have no $DISPLAY.
        let mut dbus_child = unsafe {
            Command::new("dbus-daemon")
                .args(["--session", "--print-address=1", "--nofork"])
                .env("XDG_RUNTIME_DIR", &audio_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(if verbose {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                })
                .pre_exec(pdeathsig_hook())
                .spawn()
                .map_err(|e| format!("failed to start dbus-daemon: {e}"))?
        };

        let dbus_stdout = dbus_child
            .stdout
            .take()
            .ok_or("dbus-daemon stdout missing")?;
        let mut dbus_reader = std::io::BufReader::new(dbus_stdout);
        let mut dbus_address = String::new();
        dbus_reader
            .read_line(&mut dbus_address)
            .map_err(|e| format!("failed to read dbus-daemon address: {e}"))?;
        let dbus_address = dbus_address.trim();
        if dbus_address.is_empty() {
            let _ = dbus_child.kill();
            return Err("dbus-daemon exited without printing an address".into());
        }

        // 1. Start pipewire.
        //    XDG_CONFIG_HOME=$audio_dir makes PipeWire load
        //    $audio_dir/pipewire/pipewire.conf, which takes priority over
        //    system and nix-store configs on all PipeWire versions.
        let mut pipewire_child = match unsafe {
            Command::new("pipewire")
                .env("XDG_CONFIG_HOME", &audio_dir)
                .env("DBUS_SESSION_BUS_ADDRESS", dbus_address)
                .env("XDG_RUNTIME_DIR", &audio_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(if verbose {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                })
                .pre_exec(pdeathsig_hook())
                .spawn()
        } {
            Ok(c) => c,
            Err(e) => {
                let _ = dbus_child.kill();
                let _ = dbus_child.wait();
                return Err(format!("failed to start pipewire: {e}"));
            }
        };

        // Wait for PipeWire to create its socket before spawning dependents.
        // Polls every 50 ms instead of a fixed 500 ms sleep — faster on fast
        // systems, more robust on slow ones (up to 2 s timeout).
        let pw_socket = audio_dir.join("pipewire-0");
        if !wait_for_socket(&pw_socket, std::time::Duration::from_secs(2)) {
            // Check that PipeWire hasn't already exited.
            if matches!(pipewire_child.try_wait(), Ok(Some(_))) {
                let _ = dbus_child.kill();
                let _ = dbus_child.wait();
                return Err("pipewire exited before creating its socket".into());
            }
            // Socket still missing but process alive — proceed anyway
            // (might just be slow; the next spawn will fail clearly).
        }

        // 1b. Start WirePlumber (session manager) if available.
        //     Without a session manager, pipewire-pulse can negotiate
        //     PulseAudio connections but can't create links between
        //     stream nodes and blit-sink — stream creation hangs.
        //     We use a minimal config that disables all hardware monitors
        //     (ALSA, Bluetooth, camera) to avoid conflicts with the
        //     system WirePlumber on the same D-Bus.
        let mut wireplumber_child = if find_program("wireplumber").is_some() {
            let wp_conf_dir = audio_dir.join("wireplumber").join("wireplumber.conf.d");
            let _ = std::fs::create_dir_all(&wp_conf_dir);
            let _ = std::fs::write(wp_conf_dir.join("99-blit.conf"), WIREPLUMBER_CONF_TEMPLATE);
            let child = unsafe {
                Command::new("wireplumber")
                    .env("PIPEWIRE_REMOTE", audio_dir.join("pipewire-0"))
                    .env("XDG_CONFIG_HOME", &audio_dir)
                    .env("DBUS_SESSION_BUS_ADDRESS", dbus_address)
                    .env("XDG_RUNTIME_DIR", &audio_dir)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(if verbose {
                        Stdio::inherit()
                    } else {
                        Stdio::null()
                    })
                    .pre_exec(pdeathsig_hook())
                    .spawn()
            };
            match child {
                Ok(c) => {
                    // Give WirePlumber a moment to register its policy module
                    // with PipeWire.  There's no socket to poll for here, so
                    // we use a short fixed sleep + liveness check.
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    Some(c)
                }
                Err(e) => {
                    if verbose {
                        eprintln!("[audio] failed to start wireplumber: {e}");
                    }
                    None
                }
            }
        } else {
            None
        };

        // 2. Start pipewire-pulse.
        let mut pipewire_pulse_child = match unsafe {
            Command::new("pipewire-pulse")
                .env("PIPEWIRE_REMOTE", audio_dir.join("pipewire-0"))
                .env("DBUS_SESSION_BUS_ADDRESS", dbus_address)
                .env("XDG_RUNTIME_DIR", &audio_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(if verbose {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                })
                .pre_exec(pdeathsig_hook())
                .spawn()
        } {
            Ok(c) => c,
            Err(e) => {
                if let Some(ref mut wp) = wireplumber_child {
                    let _ = wp.kill();
                }
                let _ = pipewire_child.kill();
                let _ = dbus_child.kill();
                if let Some(ref mut wp) = wireplumber_child {
                    let _ = wp.wait();
                }
                let _ = pipewire_child.wait();
                let _ = dbus_child.wait();
                return Err(format!("failed to start pipewire-pulse: {e}"));
            }
        };

        // Wait for pipewire-pulse to create the PulseAudio socket.
        let pulse_socket = audio_dir.join("pulse").join("native");
        if !wait_for_socket(&pulse_socket, std::time::Duration::from_secs(2))
            && matches!(pipewire_pulse_child.try_wait(), Ok(Some(_)))
        {
            if let Some(ref mut wp) = wireplumber_child {
                let _ = wp.kill();
            }
            let _ = pipewire_child.kill();
            let _ = dbus_child.kill();
            if let Some(ref mut wp) = wireplumber_child {
                let _ = wp.wait();
            }
            let _ = pipewire_child.wait();
            let _ = dbus_child.wait();
            return Err("pipewire-pulse exited before creating its socket".into());
        }

        // 3. Look up blit-sink's object serial for pw-cat --target.
        //    `--target blit-sink.monitor` no longer resolves in PipeWire 1.x,
        //    and `--target blit-sink` (by name) fails for record→sink routes.
        //    Using the numeric serial works reliably.
        let target = lookup_sink_serial(&audio_dir).unwrap_or_else(|| "blit-sink".to_string());
        if verbose {
            eprintln!("[audio] pw-cat target: {target}");
        }

        // 4. Start pw-cat to capture the monitor source.
        let pw_cat_child = match unsafe {
            Command::new("pw-cat")
                .args([
                    "--record",
                    "--rate",
                    "48000",
                    "--format",
                    "f32",
                    "--channels",
                    "2",
                    "--target",
                    &target,
                    "-", // write to stdout
                ])
                .env("PIPEWIRE_REMOTE", audio_dir.join("pipewire-0"))
                .env("DBUS_SESSION_BUS_ADDRESS", dbus_address)
                .env("XDG_RUNTIME_DIR", &audio_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(if verbose {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                })
                .pre_exec(pdeathsig_hook())
                .spawn()
        } {
            Ok(c) => c,
            Err(e) => {
                let _ = pipewire_pulse_child.kill();
                if let Some(ref mut wp) = wireplumber_child {
                    let _ = wp.kill();
                }
                let _ = pipewire_child.kill();
                let _ = dbus_child.kill();
                let _ = pipewire_pulse_child.wait();
                if let Some(ref mut wp) = wireplumber_child {
                    let _ = wp.wait();
                }
                let _ = pipewire_child.wait();
                let _ = dbus_child.wait();
                return Err(format!("failed to start pw-cat: {e}"));
            }
        };

        if verbose {
            eprintln!(
                "[audio] spawned dbus={} pipewire={} pipewire-pulse={} pw-cat={} dir={}",
                dbus_child.id(),
                pipewire_child.id(),
                pipewire_pulse_child.id(),
                pw_cat_child.id(),
                audio_dir.display(),
            );
        }

        // Take the stdout pipe from pw-cat for async reading.
        let mut pw_cat_child = pw_cat_child;
        let pw_cat_stdout = match pw_cat_child.stdout.take() {
            Some(s) => s,
            None => {
                let _ = pw_cat_child.kill();
                let _ = pipewire_pulse_child.kill();
                if let Some(ref mut wp) = wireplumber_child {
                    let _ = wp.kill();
                }
                let _ = pipewire_child.kill();
                let _ = dbus_child.kill();
                let _ = pw_cat_child.wait();
                let _ = pipewire_pulse_child.wait();
                if let Some(ref mut wp) = wireplumber_child {
                    let _ = wp.wait();
                }
                let _ = pipewire_child.wait();
                let _ = dbus_child.wait();
                return Err("pw-cat stdout missing".into());
            }
        };

        // Spawn the async reader + encoder task.
        let (opus_tx, opus_rx) = mpsc::channel::<OpusFrame>(RING_CAPACITY * 2);
        let opus_tx_keep = opus_tx.clone();
        let bitrate = if bitrate > 0 {
            bitrate
        } else {
            DEFAULT_BITRATE
        };
        let (bitrate_tx, bitrate_rx) = tokio::sync::watch::channel(bitrate);
        let encoder_alive = Arc::new(AtomicBool::new(true));
        let encoder_alive_clone = encoder_alive.clone();
        let has_listener = Arc::new(AtomicBool::new(false));
        let has_listener_clone = has_listener.clone();
        let verbose_copy = verbose;
        tokio::spawn(async move {
            let result = reader_encoder_task(
                pw_cat_stdout,
                opus_tx,
                bitrate,
                verbose_copy,
                epoch,
                bitrate_rx,
                has_listener_clone,
            )
            .await;
            encoder_alive_clone.store(false, Ordering::Release);
            if let Err(e) = result
                && verbose_copy
            {
                eprintln!("[audio] reader/encoder task exited: {e}");
            }
        });

        Ok(Self {
            dbus_child,
            pipewire_child,
            wireplumber_child,
            pipewire_pulse_child,
            pw_cat_child,
            opus_rx,
            ring: VecDeque::with_capacity(RING_CAPACITY),
            runtime_dir: audio_dir,
            alive: true,
            bitrate_tx,
            encoder_alive,
            has_listener,
            dbus_address: dbus_address.to_string(),
            opus_tx: opus_tx_keep,
            verbose,
            epoch,
            last_heal: None,
            first_heal_at: None,
            heals: 0,
        })
    }

    /// Drain newly encoded frames from the channel into the ring buffer.
    /// Returns a slice of all new frames received this call.
    pub fn poll_frames(&mut self) -> Vec<OpusFrame> {
        let mut new_frames = Vec::new();
        while let Ok(frame) = self.opus_rx.try_recv() {
            // Maintain ring capacity.
            if self.ring.len() >= RING_CAPACITY {
                self.ring.pop_front();
            }
            self.ring.push_back(frame.clone());
            new_frames.push(frame);
        }
        new_frames
    }

    /// Get the recent ring buffer (for catch-up on new subscribers).
    pub fn ring_frames(&self) -> impl Iterator<Item = &OpusFrame> {
        self.ring.iter()
    }

    /// Returns true if the pipeline is still producing (or can resume
    /// producing) audio.
    ///
    /// Automatically restarts dead sub-processes (WirePlumber,
    /// pipewire-pulse, pw-cat/encoder) without tearing down the entire
    /// pipeline.  Only returns false when core processes (PipeWire,
    /// dbus-daemon) die or when sub-process restarts keep failing.
    pub fn is_alive(&mut self) -> bool {
        if !self.alive {
            return false;
        }

        // Core processes: if dead, the whole pipeline must be rebuilt.
        if matches!(self.pipewire_child.try_wait(), Ok(Some(_)))
            || matches!(self.dbus_child.try_wait(), Ok(Some(_)))
        {
            self.alive = false;
            return false;
        }

        // Detect dead sub-processes.  Compute booleans first so we don't
        // hold borrows across the restart calls that take &mut self.
        let wp_dead = self
            .wireplumber_child
            .as_mut()
            .is_some_and(|wp| matches!(wp.try_wait(), Ok(Some(_))));
        let pulse_dead = matches!(self.pipewire_pulse_child.try_wait(), Ok(Some(_)));
        let encoder_dead = !self.encoder_alive.load(Ordering::Acquire);
        let pw_cat_dead = matches!(self.pw_cat_child.try_wait(), Ok(Some(_)));

        let needs_heal = wp_dead || pulse_dead || encoder_dead || pw_cat_dead;
        if !needs_heal {
            return true;
        }

        // Rate-limit heal attempts.
        let now = Instant::now();
        let can_heal = self
            .last_heal
            .is_none_or(|t| now.duration_since(t) >= HEAL_COOLDOWN);
        if !can_heal {
            // Still in cooldown — return true so the outer code doesn't
            // trigger a full pipeline restart while we're healing.
            return true;
        }

        // Burst limiter: give up after too many restarts in a window.
        if self
            .first_heal_at
            .is_none_or(|t| now.duration_since(t) > HEAL_WINDOW)
        {
            self.first_heal_at = Some(now);
            self.heals = 0;
        }
        self.heals += 1;
        if self.heals > MAX_HEALS {
            eprintln!(
                "[audio] too many sub-process restarts ({}), giving up",
                self.heals
            );
            self.alive = false;
            return false;
        }
        self.last_heal = Some(now);

        // Restart dead sub-processes individually.

        if wp_dead {
            eprintln!("[audio] wireplumber died, restarting");
            self.restart_wireplumber();
        }

        if pulse_dead {
            eprintln!("[audio] pipewire-pulse died, restarting");
            self.restart_pipewire_pulse();
        }

        if encoder_dead || pw_cat_dead {
            eprintln!("[audio] pw-cat/encoder died, restarting");
            if !self.restart_pw_cat() {
                self.alive = false;
                return false;
            }
        }

        true
    }

    /// Kill all child processes and clean up.
    pub fn shutdown(&mut self) {
        self.alive = false;
        let _ = self.pw_cat_child.kill();
        let _ = self.pipewire_pulse_child.kill();
        if let Some(ref mut wp) = self.wireplumber_child {
            let _ = wp.kill();
        }
        let _ = self.pipewire_child.kill();
        let _ = self.dbus_child.kill();
        let _ = self.pw_cat_child.wait();
        let _ = self.pipewire_pulse_child.wait();
        if let Some(ref mut wp) = self.wireplumber_child {
            let _ = wp.wait();
        }
        let _ = self.pipewire_child.wait();
        let _ = self.dbus_child.wait();
        // Remove the private runtime directory and everything in it
        // (config file, PipeWire socket, pulse/native socket, etc.).
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }

    /// Restart a dead WirePlumber sub-process.
    fn restart_wireplumber(&mut self) {
        if let Some(ref mut wp) = self.wireplumber_child {
            let _ = wp.kill();
            let _ = wp.wait();
        }
        let child = unsafe {
            Command::new("wireplumber")
                .env("PIPEWIRE_REMOTE", self.runtime_dir.join("pipewire-0"))
                .env("XDG_CONFIG_HOME", &self.runtime_dir)
                .env("DBUS_SESSION_BUS_ADDRESS", &self.dbus_address)
                .env("XDG_RUNTIME_DIR", &self.runtime_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(if self.verbose {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                })
                .pre_exec(pdeathsig_hook())
                .spawn()
        };
        match child {
            Ok(c) => {
                self.wireplumber_child = Some(c);
            }
            Err(e) => {
                eprintln!("[audio] failed to restart wireplumber: {e}");
                self.wireplumber_child = None;
            }
        }
    }

    /// Restart a dead pipewire-pulse sub-process.
    fn restart_pipewire_pulse(&mut self) {
        let _ = self.pipewire_pulse_child.kill();
        let _ = self.pipewire_pulse_child.wait();
        // Remove stale PulseAudio socket to avoid "Address already in use".
        let _ = std::fs::remove_dir_all(self.runtime_dir.join("pulse"));
        match unsafe {
            Command::new("pipewire-pulse")
                .env("PIPEWIRE_REMOTE", self.runtime_dir.join("pipewire-0"))
                .env("DBUS_SESSION_BUS_ADDRESS", &self.dbus_address)
                .env("XDG_RUNTIME_DIR", &self.runtime_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(if self.verbose {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                })
                .pre_exec(pdeathsig_hook())
                .spawn()
        } {
            Ok(c) => {
                self.pipewire_pulse_child = c;
            }
            Err(e) => {
                eprintln!("[audio] failed to restart pipewire-pulse: {e}");
            }
        }
    }

    /// Restart pw-cat and the encoder task.  Returns false if the spawn
    /// failed and the pipeline should be considered dead.
    fn restart_pw_cat(&mut self) -> bool {
        let _ = self.pw_cat_child.kill();
        let _ = self.pw_cat_child.wait();

        // Re-lookup the sink serial (it may have changed).
        let target =
            lookup_sink_serial(&self.runtime_dir).unwrap_or_else(|| "blit-sink".to_string());

        let mut new_child = match unsafe {
            Command::new("pw-cat")
                .args([
                    "--record",
                    "--rate",
                    "48000",
                    "--format",
                    "f32",
                    "--channels",
                    "2",
                    "--target",
                    &target,
                    "-",
                ])
                .env("PIPEWIRE_REMOTE", self.runtime_dir.join("pipewire-0"))
                .env("DBUS_SESSION_BUS_ADDRESS", &self.dbus_address)
                .env("XDG_RUNTIME_DIR", &self.runtime_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(if self.verbose {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                })
                .pre_exec(pdeathsig_hook())
                .spawn()
        } {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[audio] failed to restart pw-cat: {e}");
                return false;
            }
        };

        let stdout = match new_child.stdout.take() {
            Some(s) => s,
            None => {
                let _ = new_child.kill();
                return false;
            }
        };

        self.pw_cat_child = new_child;

        // Spawn a new encoder task.  The old task has already exited
        // (its pipe read returned EOF or error), so it won't compete.
        let encoder_alive = Arc::new(AtomicBool::new(true));
        self.encoder_alive = encoder_alive.clone();

        let opus_tx = self.opus_tx.clone();
        let bitrate = *self.bitrate_tx.borrow();
        let bitrate_rx = self.bitrate_tx.subscribe();
        let verbose = self.verbose;
        let epoch = self.epoch;
        let has_listener = self.has_listener.clone();

        tokio::spawn(async move {
            let result = reader_encoder_task(
                stdout,
                opus_tx,
                bitrate,
                verbose,
                epoch,
                bitrate_rx,
                has_listener,
            )
            .await;
            encoder_alive.store(false, Ordering::Release);
            if let Err(e) = result
                && verbose
            {
                eprintln!("[audio] reader/encoder task exited: {e}");
            }
        });

        if self.verbose {
            eprintln!("[audio] restarted pw-cat (target={target}) and encoder");
        }
        true
    }

    /// Update the Opus encoder bitrate. Takes effect on the next frame.
    pub fn set_bitrate(&self, bitrate: i32) {
        let _ = self.bitrate_tx.send(bitrate);
    }

    /// Tell the encoder whether any client is currently subscribed.
    /// When false, the encoder still drains pw-cat's pipe (to avoid
    /// backpressure into PipeWire) but skips the Opus encode step.
    pub fn set_has_listener(&self, has: bool) {
        self.has_listener.store(has, Ordering::Release);
    }

    /// Build the `PULSE_SERVER` value for child process environments.
    pub fn pulse_server_path(&self) -> String {
        let pulse_dir = self.runtime_dir.join("pulse");
        format!("unix:{}", pulse_dir.join("native").display())
    }

    /// Build the `PIPEWIRE_REMOTE` value for child process environments.
    ///
    /// Apps that speak PipeWire natively (mpv, Firefox, etc.) look for the
    /// PipeWire socket at `$XDG_RUNTIME_DIR/pipewire-0` by default.  Since the
    /// child's XDG_RUNTIME_DIR points at the Wayland socket directory (not the
    /// audio directory), those apps can't find the socket.  Setting
    /// PIPEWIRE_REMOTE to an absolute path lets them connect directly.
    pub fn pipewire_remote_path(&self) -> String {
        self.runtime_dir
            .join("pipewire-0")
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Build an S2C_AUDIO_FRAME wire message.
pub fn msg_audio_frame(frame: &OpusFrame) -> Vec<u8> {
    let mut msg = Vec::with_capacity(1 + 4 + 1 + frame.data.len());
    msg.push(S2C_AUDIO_FRAME);
    msg.extend_from_slice(&frame.timestamp.to_le_bytes());
    msg.push(AUDIO_FRAME_CODEC_OPUS);
    msg.extend_from_slice(&frame.data);
    msg
}

/// Async task: reads raw PCM from pw-cat stdout, frames into 20 ms chunks,
/// Opus-encodes, and sends to the channel.
///
/// `epoch` is the shared time origin for A/V sync — the same `Instant` used
/// by the video pipeline's `created_at`.  Audio timestamps are
/// `epoch.elapsed().as_millis()`, matching the video frame timestamps.
async fn reader_encoder_task(
    stdout: std::process::ChildStdout,
    tx: mpsc::Sender<OpusFrame>,
    bitrate: i32,
    verbose: bool,
    epoch: Instant,
    mut bitrate_rx: tokio::sync::watch::Receiver<i32>,
    has_listener: Arc<AtomicBool>,
) -> Result<(), String> {
    // Wrap the synchronous ChildStdout in a tokio async reader.
    let mut reader = tokio::process::ChildStdout::from_std(stdout)
        .map_err(|e| format!("failed to convert pw-cat stdout to async: {e}"))?;

    // Init Opus encoder.
    let mut encoder = OpusEncoder::new(48000, Channels::Stereo, Application::Audio)
        .map_err(|e| format!("failed to create Opus encoder: {e}"))?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(bitrate))
        .map_err(|e| format!("failed to set Opus bitrate: {e}"))?;
    // DTX: during silence the encoder emits tiny frames (or none at all),
    // cutting both bitrate and CPU across the CELT analysis pipeline.
    if let Err(e) = encoder.set_dtx(true)
        && verbose
    {
        eprintln!("[audio] failed to enable Opus DTX: {e}");
    }
    let mut current_bitrate = bitrate;

    if verbose {
        eprintln!("[audio] encoder ready, bitrate={bitrate} bps");
    }

    let mut pcm_buf = vec![0f32; FRAME_FLOATS];
    let mut byte_buf = vec![0u8; FRAME_FLOATS * 4]; // f32 = 4 bytes
    let mut byte_offset = 0usize;
    let mut opus_out = vec![0u8; MAX_OPUS_PACKET];

    loop {
        // Check for bitrate updates before reading the next chunk.
        if bitrate_rx.has_changed().unwrap_or(false) {
            let new_bitrate = *bitrate_rx.borrow_and_update();
            if new_bitrate != current_bitrate {
                if let Err(e) = encoder.set_bitrate(opus::Bitrate::Bits(new_bitrate)) {
                    if verbose {
                        eprintln!("[audio] failed to update bitrate to {new_bitrate}: {e}");
                    }
                } else {
                    if verbose {
                        eprintln!(
                            "[audio] bitrate updated: {current_bitrate} -> {new_bitrate} bps"
                        );
                    }
                    current_bitrate = new_bitrate;
                }
            }
        }

        let needed = (FRAME_FLOATS * 4) - byte_offset;
        let n = reader
            .read(&mut byte_buf[byte_offset..byte_offset + needed])
            .await
            .map_err(|e| format!("pipe read error: {e}"))?;
        if n == 0 {
            // Pipe closed — pw-cat exited.
            return Ok(());
        }
        byte_offset += n;

        // Process all complete 20 ms frames in the buffer.
        while byte_offset >= FRAME_FLOATS * 4 {
            let consumed = FRAME_FLOATS * 4;

            // When no client is listening, drain the pipe but skip the
            // per-frame f32 conversion and Opus encode — those are the
            // expensive steps. We still must consume the bytes so
            // pw-cat's stdout pipe doesn't fill and apply backpressure
            // into PipeWire.
            if !has_listener.load(Ordering::Acquire) {
                byte_buf.copy_within(consumed..byte_offset, 0);
                byte_offset -= consumed;
                continue;
            }

            // Convert bytes to f32 samples (little-endian).
            for (i, sample) in pcm_buf.iter_mut().enumerate() {
                let off = i * 4;
                *sample = f32::from_le_bytes([
                    byte_buf[off],
                    byte_buf[off + 1],
                    byte_buf[off + 2],
                    byte_buf[off + 3],
                ]);
            }

            // Encode.  Skip the frame on error instead of killing the
            // entire pipeline — a single dropped 20 ms frame is inaudible.
            let encoded_len = match encoder.encode_float(&pcm_buf, &mut opus_out) {
                Ok(len) => len,
                Err(e) => {
                    if verbose {
                        eprintln!("[audio] Opus encode error, skipping frame: {e}");
                    }
                    byte_buf.copy_within(consumed..byte_offset, 0);
                    byte_offset -= consumed;
                    continue;
                }
            };

            let frame = OpusFrame {
                // Wall-clock ms since the shared epoch — same timebase as
                // video's `created_at.elapsed().as_millis()` for A/V sync.
                timestamp: epoch.elapsed().as_millis() as u32,
                data: opus_out[..encoded_len].to_vec(),
            };

            match tx.try_send(frame) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // Channel full — drop this frame rather than blocking.
                    // Blocking here propagates back through pw-cat's stdout
                    // pipe → PipeWire's realtime thread → the app's audio
                    // submission, hanging mpv et al.  A dropped 20 ms Opus
                    // frame is inaudible; a hung audio pipeline is not.
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    // Receiver dropped — pipeline shutting down.
                    return Ok(());
                }
            }

            // Shift remaining bytes to the front.
            byte_buf.copy_within(consumed..byte_offset, 0);
            byte_offset -= consumed;
        }
    }
}
