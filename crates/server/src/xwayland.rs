//! X11 applications on the headless compositor, via `xwayland-satellite`.
//!
//! blit's compositor speaks Wayland only, so an X11 client finds no display
//! at all — it does not fall back, it fails to start. `xwayland-satellite`
//! bridges the two: it runs Xwayland rootless, and republishes every X window
//! as an ordinary `xdg_toplevel` on blit's compositor. From the compositor's
//! side the whole X session is one Wayland client with a lot of windows,
//! which is why the compositor gives that one client a single screen (see
//! `CompositorCommand::SetXwaylandPid`).
//!
//! The bridge is optional in the strongest sense: when the binary is not
//! installed nothing here runs, no `DISPLAY` is exported, and the session is
//! exactly what it was before. `BLIT_XWAYLAND=0` opts out even when it is.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Where X clients look for a display, and where Xwayland puts one.
const X11_SOCKET_DIR: &str = "/tmp/.X11-unix";

/// Displays below this belong to whatever real X session may be running on
/// the host. blit takes a number well clear of it — the directory is shared
/// with the whole machine, unlike `XDG_RUNTIME_DIR`, so two blit servers on
/// one host must not land on the same one either.
const FIRST_DISPLAY: u32 = 20;
const LAST_DISPLAY: u32 = 99;

/// How long to wait for the bridge to publish its socket before carrying on
/// without it. Xwayland is up in well under a second; the wait exists so an
/// app launched immediately after the session starts still finds a display.
const READY_TIMEOUT: Duration = Duration::from_millis(2_000);

pub struct Xwayland {
    child: Child,
    /// The value to export as `DISPLAY`, e.g. `:20`.
    display: String,
}

impl Xwayland {
    /// Start the bridge against `wayland_socket`, if one is installed.
    ///
    /// `None` means X11 apps will not run in this session, which is the
    /// status quo — never an error worth failing the session over.
    pub fn spawn(wayland_socket: &str, verbose: bool) -> Option<Self> {
        if std::env::var("BLIT_XWAYLAND").is_ok_and(|value| value == "0") {
            return None;
        }
        let bridge = find_program("xwayland-satellite")?;
        let socket = Path::new(wayland_socket);
        let runtime_dir = socket.parent().unwrap_or(Path::new("/tmp"));
        let display_name = socket.file_name()?;

        // A number that looked free can be taken between the check and the
        // exec — by another blit server, or by a host X session starting.
        // The loser's Xwayland exits immediately, so try the next one rather
        // than leaving the session without X.
        for number in free_displays().take(3) {
            let display = format!(":{number}");
            let child = Command::new(&bridge)
                .arg(&display)
                .env("XDG_RUNTIME_DIR", runtime_dir)
                .env("WAYLAND_DISPLAY", display_name)
                .env("XDG_SESSION_TYPE", "wayland")
                .env("XDG_CURRENT_DESKTOP", "blit")
                // Its own X server is the one it is about to start; an
                // inherited DISPLAY would point it at the host's.
                .env_remove("DISPLAY")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(if verbose {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                })
                .pre_exec_pdeathsig()
                .spawn();
            let mut child = match child {
                Ok(child) => child,
                Err(e) => {
                    if verbose {
                        eprintln!("[xwayland] failed to start {}: {e}", bridge.display());
                    }
                    return None;
                }
            };
            match wait_ready(&mut child, number) {
                Ready::Listening => {
                    if verbose {
                        eprintln!("[xwayland] X11 applications available on DISPLAY={display}");
                    }
                    return Some(Self { child, display });
                }
                // Still starting. Keep it: the socket is the bridge's to
                // create, and an app launched later will find it.
                Ready::Slow => {
                    if verbose {
                        eprintln!(
                            "[xwayland] no X socket after {}ms; keeping DISPLAY={display} anyway",
                            READY_TIMEOUT.as_millis()
                        );
                    }
                    return Some(Self { child, display });
                }
                Ready::Exited => {
                    let _ = child.wait();
                    if verbose {
                        eprintln!("[xwayland] bridge exited on {display}; trying another display");
                    }
                }
            }
        }
        if verbose {
            eprintln!("[xwayland] no display number could be claimed");
        }
        None
    }

    /// The `DISPLAY` X clients in this session should use.
    pub fn display(&self) -> &str {
        &self.display
    }

    /// The bridge's pid, which is how the compositor recognises its
    /// connection — and Xwayland's, which is a child of it.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Reap an unexpectedly exited bridge and report whether it is alive.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Xwayland {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

enum Ready {
    /// The X socket is there; clients can connect.
    Listening,
    /// Nothing yet, but the bridge is still running.
    Slow,
    /// The bridge gave up — most likely the display number was taken, or
    /// Xwayland is not installed alongside it.
    Exited,
}

fn wait_ready(child: &mut Child, number: u32) -> Ready {
    let socket = Path::new(X11_SOCKET_DIR).join(format!("X{number}"));
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if socket.exists() {
            return Ready::Listening;
        }
        if !matches!(child.try_wait(), Ok(None)) {
            return Ready::Exited;
        }
        if Instant::now() >= deadline {
            return Ready::Slow;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Display numbers nothing is currently listening on, lowest first.
///
/// Both files matter: the socket is what a client connects to, and the lock
/// is what an X server leaves behind to claim the number even before its
/// socket exists.
fn free_displays() -> impl Iterator<Item = u32> {
    (FIRST_DISPLAY..=LAST_DISPLAY).filter(|number| {
        !Path::new(X11_SOCKET_DIR)
            .join(format!("X{number}"))
            .exists()
            && !Path::new("/tmp").join(format!(".X{number}-lock")).exists()
    })
}

fn find_program(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|directory| Path::new(directory).join(name))
        .find(|path| path.is_file())
}

/// `Command::pre_exec` with the hook every blit-owned daemon uses, so the
/// bridge cannot outlive the server that started it.
trait PreExecPdeathsig {
    fn pre_exec_pdeathsig(&mut self) -> &mut Self;
}

impl PreExecPdeathsig for Command {
    fn pre_exec_pdeathsig(&mut self) -> &mut Self {
        use std::os::unix::process::CommandExt;
        // SAFETY: the hook only calls `prctl`, which is async-signal-safe.
        unsafe { self.pre_exec(crate::audio::pdeathsig_hook()) }
    }
}
