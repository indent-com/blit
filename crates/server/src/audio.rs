//! Audio capture pipeline: PipeWire spawn → pw-cat pipe → Opus encode.
//!
//! Each compositor instance gets its own PipeWire + pipewire-pulse pair.
//! Apps connect via PulseAudio; PipeWire mixes into a null sink; pw-cat
//! captures the monitor source and writes interleaved f32 PCM to stdout.
//! We read that pipe, frame into 20 ms chunks, and Opus-encode for delivery.

use blit_remote::{AUDIO_FRAME_CODEC_OPUS, S2C_AUDIO_FRAME};
use opus::{Application, Channels, Encoder as OpusEncoder};
use std::collections::{HashMap, VecDeque};
use std::io::BufRead;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
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
/// Also sizes the encoder -> fan-out channel (at `RING_CAPACITY * 2`),
/// which needs slack for a briefly-descheduled fan-out task — so keep
/// this independent of how much of the ring we actually replay.
const RING_CAPACITY: usize = 10;

/// How many of the ring's newest frames a new subscriber is sent as
/// catch-up.  Only enough to prime the client's jitter buffer past its
/// floor (3 frames / 60 ms) and cover the gap until the first live
/// frame; every frame beyond that is start-up latency the client then
/// has to spend seconds draining at its ±2 % servo rate.  Replaying the
/// full 200 ms ring put every fresh subscribe ~140 ms in the hole.
const CATCHUP_FRAMES: usize = 4;

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

/// Shared state between the per-client subscribe/unsubscribe API on
/// [`AudioPipeline`] and the fan-out task that drains encoded frames
/// from the encoder.
///
/// Lives outside the pipeline so it persists across pipeline restarts:
/// clients stay subscribed even when pw-cat or the encoder task dies and
/// is respawned.  Wrap in `Arc` at the caller.
pub struct AudioBroadcast {
    /// Per-client audio MPSC senders, keyed by client id.
    subscribers: std::sync::Mutex<HashMap<u64, mpsc::UnboundedSender<Vec<u8>>>>,
    /// Recent frames for catch-up on new subscribers.  Kept in sync with
    /// delivery: every frame delivered to subscribers is first appended
    /// here, so a late-subscribing client gets the same tail.
    ring: std::sync::Mutex<VecDeque<OpusFrame>>,
    /// Shared flag telling the encoder task whether to bother encoding.
    /// Updated atomically from subscribe/unsubscribe.  Encoder still
    /// drains pw-cat's pipe (to avoid PipeWire backpressure) but skips
    /// the Opus encode when no one is listening.
    has_listener: Arc<AtomicBool>,
}

impl AudioBroadcast {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            subscribers: std::sync::Mutex::new(HashMap::new()),
            ring: std::sync::Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
            has_listener: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Register a client for future frames and push the newest
    /// `CATCHUP_FRAMES` of the ring onto its tx queue.
    ///
    /// Ordering guarantee: the fan-out task cannot deliver a frame to
    /// this client before we finish snapshotting+enqueuing the catch-up
    /// tail, because we hold the ring lock while inserting into the
    /// subscribers map — and the fan-out task takes ring-then-subs in
    /// that order.  Callers can therefore rely on strict ordering of
    /// the client's mpsc queue: catch-up frames first, then live frames.
    pub fn subscribe(&self, id: u64, tx: mpsc::UnboundedSender<Vec<u8>>) {
        let ring_guard = self.ring.lock().unwrap();
        // Push catch-up into the client's queue while the fan-out task
        // is blocked on ring lock.  Any frame the fan-out task is about
        // to publish is either already in the ring we just enumerated
        // (so replayed as catch-up) or will arrive after we release the
        // lock and register (so delivered live) — never both, never
        // neither.
        let skip = ring_guard.len().saturating_sub(CATCHUP_FRAMES);
        for frame in ring_guard.iter().skip(skip) {
            let _ = tx.send(msg_audio_frame(frame));
        }
        let mut subs = self.subscribers.lock().unwrap();
        subs.insert(id, tx);
        self.has_listener.store(true, Ordering::Release);
    }

    /// Remove a client from the subscriber set.  Idempotent.
    pub fn unsubscribe(&self, id: u64) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.remove(&id);
        if subs.is_empty() {
            self.has_listener.store(false, Ordering::Release);
        }
    }

    fn has_listener_flag(&self) -> Arc<AtomicBool> {
        self.has_listener.clone()
    }
}

/// Dedicated task: drains encoded Opus frames from the encoder's MPSC and
/// fans them out to every subscribed client, **off the main server tick
/// loop**.  Running independently is the whole point — on a shared-tick
/// design, long video writes or compositor work would starve audio
/// delivery and the bounded encoder channel would overflow and silently
/// drop frames, starving the client's jitter buffer below real-time.
async fn fanout_task(mut opus_rx: mpsc::Receiver<OpusFrame>, broadcast: Arc<AudioBroadcast>) {
    while let Some(frame) = opus_rx.recv().await {
        // Serialize once per frame, then clone the Vec per subscriber.
        // Opus packets are small (~100–300 B at 64 kbps), so the clone
        // cost is dwarfed by the MPSC send syscall overhead.
        let msg = msg_audio_frame(&frame);
        {
            let mut ring = broadcast.ring.lock().unwrap();
            if ring.len() >= RING_CAPACITY {
                ring.pop_front();
            }
            ring.push_back(frame);
        }
        let subs = broadcast.subscribers.lock().unwrap();
        for tx in subs.values() {
            let _ = tx.send(msg.clone());
        }
    }
}

/// Manages the PipeWire child processes and produces Opus frames.
pub struct AudioPipeline {
    dbus_child: Child,
    pipewire_child: Child,
    wireplumber_child: Option<Child>,
    pipewire_pulse_child: Child,
    /// In-process PipeWire capture stream (replaces pw-cat).  `None`
    /// only transiently during construction failure paths.
    capture: Option<crate::audio_pw::Capture>,
    /// The XDG_RUNTIME_DIR used by this pipeline's PipeWire instance.
    pub runtime_dir: PathBuf,
    /// True when the pipeline is still running.
    alive: bool,
    /// Send bitrate updates to the encoder task.
    bitrate_tx: tokio::sync::watch::Sender<i32>,
    /// Shared flag set to `false` when the encoder task exits.
    encoder_alive: Arc<AtomicBool>,
    /// D-Bus session bus address for restarting sub-processes.
    dbus_address: String,
    /// Verbose logging flag.
    verbose: bool,
    /// Last sub-process heal attempt timestamp.
    last_heal: Option<Instant>,
    /// Start of the current heal burst window.
    first_heal_at: Option<Instant>,
    /// Number of heals in the current burst window.
    heals: u32,
}

/// PipeWire configuration template.
///
/// Clock quanta are sized for the remote-desktop path, not local
/// monitoring: the browser client sits behind a >= 60 ms jitter buffer,
/// so a 21 ms graph cycle (1024/48000) adds no perceptible latency while
/// giving the (possibly non-RT) graph threads 4x more scheduling slack
/// per deadline than the PipeWire default under encode-saturated CPU.
/// `min-quantum` stops any client stream from dragging the graph back
/// down to millisecond cycles it can't reliably meet.
const PIPEWIRE_CONF_TEMPLATE: &str = r#"
context.properties = {
    core.daemon          = true
    core.name            = pipewire-0
    default.clock.rate   = 48000
    default.clock.quantum     = 1024
    default.clock.min-quantum = 1024
    default.clock.max-quantum = 2048
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}
context.modules = [
    # RT scheduling for the graph threads (falls back to nice -11 when
    # RLIMIT_RTPRIO / RTKit aren't available).  Without this the stripped
    # config runs the timer-driven null sink at SCHED_OTHER, and heavy
    # video-encode load on the same host makes graph cycles miss their
    # deadlines — audible as capture gaps baked into the Opus stream.
    { name = libpipewire-module-rt
        args = {
            nice.level   = -11
            rt.prio      = 88
            rt.time.soft = -1
            rt.time.hard = -1
        }
        flags = [ ifexists nofail ]
    }
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

/// Check whether the required PipeWire + D-Bus binaries and
/// `libpipewire-0.3.so.0` are available.  Capture is done in-process
/// via `audio_pw`, so `pw-cat` is no longer required.
pub fn pipewire_available() -> bool {
    missing_pipewire_binaries().is_empty() && crate::audio_pw::available()
}

/// Returns the list of required PipeWire / D-Bus binaries that are not
/// found on `$PATH`.  Empty list means audio can run (provided
/// libpipewire is also loadable at runtime; see `pipewire_available`).
pub fn missing_pipewire_binaries() -> Vec<&'static str> {
    ["pipewire", "pipewire-pulse", "dbus-daemon"]
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

/// Name of the private runtime directory holding instance `instance_id`'s
/// PipeWire and pulse sockets.  Scoped to this process; see the call site in
/// `spawn` for why the pid has to be in there.
fn audio_dir_name(instance_id: u16) -> String {
    format!("blit-audio-{}-{instance_id}", std::process::id())
}

/// Delete `blit-audio-<pid>-<instance>` directories whose owning blit server
/// is gone.  Restarting a pipeline inside one process reuses its own name, so
/// the only leftovers are from servers that exited without running
/// `shutdown()` — SIGKILL, or a panic that skipped the `Drop`.
///
/// Deliberately conservative, since the whole point of the pid in the name is
/// to stop one server from deleting another's live sockets.  A directory goes
/// only when its name parses, no process holds the pid, and we own it.
/// Anything else is left alone, including the legacy `blit-audio-<instance>`
/// names written by older servers: those carry no pid, so there is no way to
/// tell a stale one from a running server's.
///
/// The pid check assumes the directory's owner shares our PID namespace.
/// That holds for servers sharing a runtime dir on one host; a container that
/// bind-mounts the host's temp dir but not its PID namespace could still see
/// a live directory as stale.
fn sweep_stale_audio_dirs(runtime_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return;
    };
    // SAFETY: `getuid` is always successful and has no preconditions.
    let self_uid = unsafe { libc::getuid() };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix("blit-audio-")) else {
            continue;
        };
        let Some((pid, instance)) = rest.split_once('-') else {
            continue;
        };
        if pid.parse::<u32>().is_err() || instance.parse::<u16>().is_err() {
            continue;
        }
        if Path::new("/proc").join(pid).exists() {
            continue;
        }
        // symlink_metadata, not metadata: never follow a link planted in a
        // shared runtime dir such as /tmp.
        let path = entry.path();
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if !meta.is_dir() || meta.uid() != self_uid {
            continue;
        }
        let _ = std::fs::remove_dir_all(&path);
    }
}

impl AudioPipeline {
    /// Spawn a new PipeWire instance and start capturing audio.
    ///
    /// `runtime_dir` is the XDG_RUNTIME_DIR for this compositor instance.
    /// `instance_id` names the PipeWire remote uniquely *within this
    /// process*; see the runtime-dir comment below for why that is not
    /// enough on its own.
    /// `bitrate` is the Opus encoder bitrate in bits/sec (0 = default).
    /// `epoch` is the shared time origin (same `Instant` used by video
    /// timestamps) so audio and video share a common timebase for A/V sync.
    /// `broadcast` is the shared fan-out state; pass the same `Arc` across
    /// restarts so subscribed clients stay connected to the output.
    pub fn spawn(
        runtime_dir: &Path,
        instance_id: u16,
        bitrate: i32,
        verbose: bool,
        epoch: Instant,
        broadcast: Arc<AudioBroadcast>,
    ) -> Result<Self, String> {
        // Use a private subdirectory so the PulseAudio socket doesn't
        // collide with the system's or with other blit instances.
        //
        // The pid is part of the name because `instance_id` is a per-server
        // counter that restarts at 1 in every blit server process — it is
        // unique within one server, not across servers.  Two servers whose
        // runtime dirs resolve to the same place both pick instance 1, and
        // that is the common case rather than an exotic one: a compositor
        // with no writable XDG_RUNTIME_DIR falls back to the temp dir, and
        // it exports that fallback to its PTY children, so a dev stack
        // started from inside a blit terminal inherits it too.  Both then
        // land on /tmp/blit-audio-1, and the `remove_dir_all` below deletes
        // the other server's live sockets: the loser's capture stream ends
        // up reading a daemon that nothing feeds, its pipeline restarts,
        // and the restart deletes the winner's directory in turn.  Audio
        // then ping-pongs between the two servers, silent on whichever one
        // did not spawn last.
        let audio_dir = runtime_dir.join(audio_dir_name(instance_id));

        // Remove leftovers from a previous unclean exit so we don't trip
        // over stale PipeWire/pulse sockets ("Address already in use").
        if audio_dir.exists() {
            let _ = std::fs::remove_dir_all(&audio_dir);
        }

        // Pid-scoped names are never reused, so a server that died without
        // running `shutdown()` leaves its directory behind for good unless
        // somebody collects it.
        sweep_stale_audio_dirs(runtime_dir);

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
            // Every other bail-out below waits what it kills; this one did
            // not, so a dbus-daemon that started and then said nothing was
            // left a zombie for the life of the server.
            let _ = dbus_child.wait();
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

        // 3. Open an in-process PipeWire capture stream on blit-sink's
        //    monitor.  No more pw-cat subprocess, no pipe buffer — the
        //    RT callback hands us PCM frames directly.  Target by name
        //    since the `target.object` property accepts node names.
        let (capture, capture_rx) = match crate::audio_pw::Capture::start(&audio_dir, "blit-sink") {
            Ok(pair) => pair,
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
                return Err(format!("failed to start PipeWire capture: {e}"));
            }
        };

        if verbose {
            eprintln!(
                "[audio] spawned dbus={} pipewire={} pipewire-pulse={} capture=in-process dir={}",
                dbus_child.id(),
                pipewire_child.id(),
                pipewire_pulse_child.id(),
                audio_dir.display(),
            );
        }

        // Spawn the async encoder task.
        let (opus_tx, opus_rx) = mpsc::channel::<OpusFrame>(RING_CAPACITY * 2);
        let bitrate = if bitrate > 0 {
            bitrate
        } else {
            DEFAULT_BITRATE
        };
        let (bitrate_tx, bitrate_rx) = tokio::sync::watch::channel(bitrate);
        let encoder_alive = Arc::new(AtomicBool::new(true));
        let encoder_alive_clone = encoder_alive.clone();
        let has_listener = broadcast.has_listener_flag();
        let has_listener_clone = has_listener.clone();
        let verbose_copy = verbose;
        tokio::spawn(async move {
            let result = encoder_task(
                capture_rx,
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
                eprintln!("[audio] encoder task exited: {e}");
            }
        });

        // Spawn the fan-out task: drains encoded frames from the encoder
        // and pushes them to every subscribed client's mpsc, independent
        // of the main server tick loop so long video writes can't starve
        // audio delivery.
        let broadcast_for_fanout = broadcast.clone();
        tokio::spawn(async move {
            fanout_task(opus_rx, broadcast_for_fanout).await;
        });

        Ok(Self {
            dbus_child,
            pipewire_child,
            wireplumber_child,
            pipewire_pulse_child,
            capture: Some(capture),
            runtime_dir: audio_dir,
            alive: true,
            bitrate_tx,
            encoder_alive,
            dbus_address: dbus_address.to_string(),
            verbose,
            last_heal: None,
            first_heal_at: None,
            heals: 0,
        })
    }

    /// Collect any sub-process that has exited, and nothing else.
    ///
    /// `is_alive` already reaps as a side effect of its `try_wait` calls,
    /// but it lives in the delivery tick, which only runs while a client is
    /// attached — and it heals as well as observes, which is not something to
    /// do on a timer nobody asked for.  A server sitting idle with a dead
    /// PipeWire would keep the corpse until someone connected.
    ///
    /// That used to be covered by the reaper draining `waitpid(-1)` for the
    /// whole process; once that narrowed to PTY-owned pids, these had nobody.
    /// So the supervisor calls this instead: same `try_wait`, no restart, no
    /// state change beyond releasing a zombie.
    pub fn reap_children(&mut self) {
        let _ = self.dbus_child.try_wait();
        let _ = self.pipewire_child.try_wait();
        let _ = self.pipewire_pulse_child.try_wait();
        if let Some(ref mut wp) = self.wireplumber_child {
            let _ = wp.try_wait();
        }
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

        let needs_heal = wp_dead || pulse_dead || encoder_dead;
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

        if encoder_dead {
            // The encoder task can only exit if its capture receiver
            // closed (PipeWire stream gone) or it hit an unrecoverable
            // encode error.  Restarting the in-process capture cleanly
            // is not supported yet — bail so the caller triggers a full
            // pipeline restart (which re-spawns everything).
            eprintln!("[audio] encoder died, triggering full pipeline restart");
            self.alive = false;
            return false;
        }

        true
    }

    /// Kill all child processes and clean up.
    pub fn shutdown(&mut self) {
        self.alive = false;
        // Stop the in-process capture first so the PW thread-loop has
        // joined before we tear the daemon down under it.
        self.capture.take();
        let _ = self.pipewire_pulse_child.kill();
        if let Some(ref mut wp) = self.wireplumber_child {
            let _ = wp.kill();
        }
        let _ = self.pipewire_child.kill();
        let _ = self.dbus_child.kill();
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

    /// Update the Opus encoder bitrate. Takes effect on the next frame.
    pub fn set_bitrate(&self, bitrate: i32) {
        let _ = self.bitrate_tx.send(bitrate);
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

/// Async task: consumes raw PCM chunks delivered by the in-process
/// PipeWire capture (`audio_pw::Capture`), frames into 20 ms windows,
/// Opus-encodes, and sends to the fan-out channel.
///
/// `epoch` is the shared time origin for A/V sync — the same `Instant`
/// used by the video pipeline's `created_at`.  Audio timestamps are
/// `epoch.elapsed().as_millis()`, matching the video frame timestamps.
async fn encoder_task(
    mut pcm_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    tx: mpsc::Sender<OpusFrame>,
    bitrate: i32,
    verbose: bool,
    epoch: Instant,
    mut bitrate_rx: tokio::sync::watch::Receiver<i32>,
    has_listener: Arc<AtomicBool>,
) -> Result<(), String> {
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
    let mut byte_buf: Vec<u8> = Vec::with_capacity(FRAME_FLOATS * 4 * 2);
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

        // Receive the next capture chunk.  Chunks are whatever size
        // PipeWire gave us (typically one quantum ≈ 21 ms at 48 kHz for
        // the latency we requested), which we accumulate until we have
        // a full 20 ms Opus frame's worth of bytes.
        let chunk = match pcm_rx.recv().await {
            Some(c) => c,
            None => return Ok(()), // capture closed
        };
        byte_buf.extend_from_slice(&chunk);

        // Process all complete 20 ms frames in the buffer.
        while byte_buf.len() >= FRAME_FLOATS * 4 {
            let consumed = FRAME_FLOATS * 4;

            // When no client is listening, drain samples but skip the
            // per-frame f32 conversion and Opus encode — those are the
            // expensive steps.  We still must consume the bytes so the
            // capture's unbounded mpsc doesn't grow without bound.
            if !has_listener.load(Ordering::Acquire) {
                byte_buf.drain(..consumed);
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
                    byte_buf.drain(..consumed);
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
                    // A dropped 20 ms Opus frame is inaudible; blocking
                    // here would propagate backpressure into PipeWire's
                    // RT thread and hang audio-producing apps.
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    // Receiver dropped — pipeline shutting down.
                    return Ok(());
                }
            }

            byte_buf.drain(..consumed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ts: u32) -> OpusFrame {
        OpusFrame {
            timestamp: ts,
            data: vec![0u8; 8],
        }
    }

    /// Read the timestamp back out of a serialized S2C_AUDIO_FRAME.
    fn frame_ts(msg: &[u8]) -> u32 {
        u32::from_le_bytes(msg[1..5].try_into().unwrap())
    }

    /// A new subscriber must be primed, but only just: replaying the whole
    /// ring hands it a jitter buffer that starts already behind live, and
    /// the client can only drain that at its ±2 % servo rate.
    #[test]
    fn catch_up_replays_only_the_newest_frames() {
        let bc = AudioBroadcast::new();
        {
            let mut ring = bc.ring.lock().unwrap();
            for ts in 0..RING_CAPACITY as u32 {
                ring.push_back(frame(ts));
            }
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        bc.subscribe(1, tx);

        let mut got = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            got.push(frame_ts(&msg));
        }

        assert_eq!(got.len(), CATCHUP_FRAMES);
        // The newest frames, in order — never the stalest.
        let first = RING_CAPACITY as u32 - CATCHUP_FRAMES as u32;
        assert_eq!(got, (first..RING_CAPACITY as u32).collect::<Vec<_>>());
    }

    /// A ring shorter than the catch-up window is replayed whole rather
    /// than under-delivering.
    #[test]
    fn catch_up_handles_a_partially_filled_ring() {
        let bc = AudioBroadcast::new();
        bc.ring.lock().unwrap().push_back(frame(7));

        let (tx, mut rx) = mpsc::unbounded_channel();
        bc.subscribe(1, tx);

        let msg = rx.try_recv().expect("the one ring frame");
        assert_eq!(frame_ts(&msg), 7);
        assert!(rx.try_recv().is_err());
    }

    /// A pid with no `/proc` entry.  Scanning down from `pid_max` keeps us
    /// clear of the low numbers the kernel is about to hand out.
    fn dead_pid() -> u32 {
        let max = std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(32768);
        (2..max)
            .rev()
            .find(|p| !Path::new("/proc").join(p.to_string()).exists())
            .expect("no free pid to use as a dead one")
    }

    #[test]
    fn sweep_collects_dead_servers_and_spares_everyone_else() {
        let root =
            std::env::temp_dir().join(format!("blit-audio-sweep-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // Ours, for a pipeline that is currently running.  This is the case
        // the pid exists to protect: another server sweeping the same
        // runtime dir must not touch it.
        let live = root.join(audio_dir_name(1));
        // A server that died without cleaning up.
        let stale = root.join(format!("blit-audio-{}-1", dead_pid()));
        // Written by a pre-pid server: no way to tell stale from in-use.
        let legacy = root.join("blit-audio-1");
        // Neither of these parses as `<pid>-<instance>`.
        let unparsable = root.join("blit-audio-nope-1");
        let unrelated = root.join("wayland-0");
        for d in [&live, &stale, &legacy, &unparsable, &unrelated] {
            std::fs::create_dir(d).unwrap();
        }
        // Same name as a stale directory but a plain file, so `remove_dir_all`
        // must not be pointed at it.
        let file = root.join(format!("blit-audio-{}-2", dead_pid()));
        std::fs::write(&file, b"").unwrap();

        sweep_stale_audio_dirs(&root);

        assert!(live.exists(), "swept a live pipeline's directory");
        assert!(
            legacy.exists(),
            "swept a legacy directory of unknown status"
        );
        assert!(unparsable.exists(), "swept an unparsable name");
        assert!(unrelated.exists(), "swept an unrelated entry");
        assert!(file.exists(), "swept a non-directory");
        assert!(!stale.exists(), "left a dead server's directory behind");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn audio_dir_name_is_scoped_to_this_process() {
        let name = audio_dir_name(1);
        assert!(
            name.contains(&std::process::id().to_string()),
            "{name} carries no pid, so a second server would pick the same one"
        );
        // Distinct compositor sessions still get distinct directories.
        assert_ne!(audio_dir_name(1), audio_dir_name(2));
    }
}
