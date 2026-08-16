//! Private D-Bus session for applications running on the headless compositor.
//!
//! A host session bus is tied to the host compositor. Portal calls made on it
//! therefore create dialogs outside blit's Wayland display, where they may be
//! invisible (or have no output at all). A compositor-scoped bus keeps D-Bus
//! activation in the same Wayland environment as the PTYs that use it.

use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

pub struct DesktopBus {
    child: Child,
    address: String,
    bridge: Option<blit_desktop::Bridge>,
    portal_child: Option<Child>,
    portal_root: Option<PathBuf>,
    /// Kept so [`DesktopBus::start_portal`] can spawn the frontend after this
    /// bus is built, rather than during `spawn`.
    runtime_dir: PathBuf,
    display: std::ffi::OsString,
}

impl DesktopBus {
    pub fn spawn(
        wayland_socket: &str,
        verbose: bool,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, String> {
        let socket = Path::new(wayland_socket);
        let runtime_dir = socket.parent().unwrap_or(Path::new("/tmp"));
        let display = socket
            .file_name()
            .ok_or_else(|| format!("Wayland socket has no filename: {wayland_socket}"))?;

        // The activation environment belongs to this private bus. Services
        // such as xdg-desktop-portal-gtk inherit it and consequently map their
        // windows on blit's compositor instead of the host desktop.
        let mut child = unsafe {
            Command::new("dbus-daemon")
                .args(["--session", "--print-address=1", "--nofork"])
                .env("XDG_RUNTIME_DIR", runtime_dir)
                .env("WAYLAND_DISPLAY", display)
                .env("XDG_SESSION_TYPE", "wayland")
                .env("XDG_CURRENT_DESKTOP", "blit")
                .env("NIXOS_OZONE_WL", "1")
                .env("ELECTRON_OZONE_PLATFORM_HINT", "wayland")
                .env("MOZ_ENABLE_WAYLAND", "1")
                .env("GDK_BACKEND", "wayland")
                .env("QT_QPA_PLATFORM", "wayland")
                .env("SDL_VIDEODRIVER", "wayland")
                .env_remove("DISPLAY")
                .env_remove("DBUS_SESSION_BUS_ADDRESS")
                .env_remove("DBUS_SYSTEM_BUS_ADDRESS")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(if verbose {
                    Stdio::inherit()
                } else {
                    Stdio::null()
                })
                .pre_exec(crate::audio::pdeathsig_hook())
                .spawn()
                .map_err(|e| format!("failed to start private D-Bus session: {e}"))?
        };

        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("private D-Bus session stdout missing".into());
        };
        let mut reader = std::io::BufReader::new(stdout);
        let mut address = String::new();
        if let Err(e) = reader.read_line(&mut address) {
            drop(reader);
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("failed to read private D-Bus address: {e}"));
        }
        let address = address.trim().to_string();
        if address.is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return Err("private D-Bus session exited without printing an address".into());
        }

        let disabled = std::env::var("BLIT_DESKTOP").is_ok_and(|value| value == "0");
        let bridge = if disabled {
            None
        } else {
            let parse_duration = |name: &str, default_ms: u64| {
                std::env::var(name)
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_millis)
                    .unwrap_or(Duration::from_millis(default_ms))
            };
            let minimum_timeout = parse_duration("BLIT_NOTIFICATION_TIMEOUT_MIN_MS", 1_000);
            let maximum_timeout =
                parse_duration("BLIT_NOTIFICATION_TIMEOUT_MAX_MS", 86_400_000).max(minimum_timeout);
            let default_timeout = parse_duration("BLIT_NOTIFICATION_TIMEOUT_MS", 10_000)
                .clamp(minimum_timeout, maximum_timeout);
            let config = blit_desktop::Config {
                default_timeout,
                minimum_timeout,
                maximum_timeout,
            };
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(blit_desktop::Bridge::start(&address, config, notify))
            });
            match result {
                Ok(bridge) => Some(bridge),
                Err(error) => {
                    if verbose {
                        eprintln!("[desktop-bus] desktop services unavailable: {error}");
                    }
                    None
                }
            }
        };

        Ok(Self {
            child,
            address,
            bridge,
            // The frontend is started by `start_portal`, once the caller has
            // a PipeWire socket to point it at.  See that method.
            portal_child: None,
            portal_root: None,
            runtime_dir: runtime_dir.to_path_buf(),
            display: display.to_os_string(),
        })
    }

    /// Start the portal frontend, pointed at `pipewire_remote`.
    ///
    /// Deliberately not part of `spawn`. The frontend connects to PipeWire
    /// exactly once, at its own startup, and if that fails it reports
    /// `IsCameraPresent = false` and never offers ScreenCast for the rest of
    /// its life — no retry. blit's PipeWire lives in the audio pipeline, and
    /// the pipeline can only be built *after* this bus exists because it needs
    /// the bus address. Spawning the frontend inside `spawn` therefore raced
    /// the socket it depends on, and won.
    ///
    /// The socket also isn't where the frontend would look by itself: it lives
    /// in the audio pipeline's own directory (`blit-audio-<pid>-<instance>`),
    /// while XDG_RUNTIME_DIR has to stay pointed at the Wayland socket's
    /// directory. Without PIPEWIRE_REMOTE the frontend falls back to
    /// `$XDG_RUNTIME_DIR/pipewire-0` and either finds nothing (a runtime dir
    /// with no PipeWire of its own, which is the `/tmp` fallback case) or —
    /// worse, because it is silent — connects to the *host* graph, where the
    /// camera a browser is sharing into blit does not exist.
    ///
    /// `None` when there is no pipeline to point at: the frontend still owns
    /// FileChooser, OpenURI, Settings and friends, so it is worth having
    /// without a camera.
    pub fn start_portal(&mut self, pipewire_remote: Option<&str>, verbose: bool) {
        if self.portal_child.is_some()
            || self.bridge.is_none()
            || std::env::var("BLIT_PORTALS").is_ok_and(|value| value == "0")
            || find_portal_frontend().is_none()
        {
            return;
        }
        match spawn_portal_frontend(
            &self.runtime_dir,
            &self.display,
            &self.address,
            pipewire_remote,
            verbose,
        ) {
            Ok((child, root)) => {
                self.portal_child = Some(child);
                self.portal_root = Some(root);
            }
            Err(error) => {
                if verbose {
                    eprintln!("[portal] {error}");
                }
            }
        }
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    /// Reap an unexpectedly exited daemon and report whether it is alive.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn services_live(&self) -> bool {
        self.bridge.is_some()
    }

    /// Consume one unexpected portal-frontend exit. Keeping this as a
    /// one-shot transition avoids coupling cleanup to a fragile
    /// `configured && !live` pair of polls, and a transient `try_wait` error
    /// does not lose the only handle to a potentially live child.
    pub fn take_portal_frontend_exit(&mut self) -> bool {
        take_child_exit(&mut self.portal_child)
    }

    pub fn portals_configured(&self) -> bool {
        self.portal_child.is_some()
    }

    pub fn try_recv(&mut self) -> Option<blit_desktop::Event> {
        self.bridge.as_mut()?.try_recv()
    }

    pub fn try_command(&self, command: blit_desktop::Command) -> bool {
        self.bridge
            .as_ref()
            .is_some_and(|bridge| bridge.try_command(command))
    }
}

impl Drop for DesktopBus {
    fn drop(&mut self) {
        if let Some(child) = self.portal_child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(root) = self.portal_root.take() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

fn find_program(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|directory| Path::new(directory).join(name))
        .find(|path| path.is_file())
}

/// Directories holding the portal frontend on distributions that do not put
/// it on `PATH` — which is all of them. It is a D-Bus activated service
/// binary, so nobody is expected to type its name: Fedora and Debian ship it
/// in `/usr/libexec`, Arch in `/usr/lib`, and the nixpkgs package has no
/// `bin/` at all. A `PATH` lookup alone therefore finds it almost nowhere,
/// and the frontend silently never starts — taking the Camera and ScreenCast
/// portals with it, which is what makes a shared camera invisible to Firefox
/// and Chromium and drops `getDisplayMedia` back to in-browser tab capture.
const PORTAL_LIBEXEC_DIRS: &[&str] = &[
    "/usr/local/libexec",
    "/usr/libexec",
    "/usr/local/lib/xdg-desktop-portal",
    "/usr/lib/xdg-desktop-portal",
    "/usr/local/lib",
    "/usr/lib",
];

/// Locate `xdg-desktop-portal`.
///
/// `BLIT_PORTAL_BIN` overrides everything, for an install in an unusual
/// prefix. Otherwise `PATH` first — a Nix devShell or a systemd unit can put
/// the libexec directory there — then the conventional locations above.
fn find_portal_frontend() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("BLIT_PORTAL_BIN") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    find_program("xdg-desktop-portal").or_else(|| {
        PORTAL_LIBEXEC_DIRS
            .iter()
            .map(|directory| Path::new(directory).join("xdg-desktop-portal"))
            .find(|path| path.is_file())
    })
}

fn take_child_exit(child: &mut Option<Child>) -> bool {
    let exited = child
        .as_mut()
        .is_some_and(|child| matches!(child.try_wait(), Ok(Some(_))));
    if exited {
        *child = None;
    }
    exited
}

fn spawn_portal_frontend(
    runtime_dir: &Path,
    display: &std::ffi::OsStr,
    address: &str,
    pipewire_remote: Option<&str>,
    verbose: bool,
) -> Result<(Child, PathBuf), String> {
    let root = runtime_dir.join(format!(
        "blit-portals-{}-{}",
        std::process::id(),
        display.to_string_lossy()
    ));
    if root.exists() {
        std::fs::remove_dir_all(&root).map_err(|error| error.to_string())?;
    }
    let config_home = root.join("config");
    let data_home = root.join("data");
    std::fs::create_dir_all(config_home.join("xdg-desktop-portal"))
        .map_err(|error| error.to_string())?;
    std::fs::create_dir_all(data_home.join("xdg-desktop-portal/portals"))
        .map_err(|error| error.to_string())?;
    std::fs::write(
        data_home.join("xdg-desktop-portal/portals/blit.portal"),
        "[portal]\nDBusName=org.freedesktop.impl.portal.desktop.blit\nInterfaces=org.freedesktop.impl.portal.Access;org.freedesktop.impl.portal.ScreenCast;\nUseIn=blit\n",
    )
    .map_err(|error| error.to_string())?;
    let fallback = std::env::var("BLIT_PORTAL_FALLBACK").unwrap_or_else(|_| "gtk;*".into());
    let fallback = if fallback.is_empty() { "*" } else { &fallback };
    std::fs::write(
        config_home.join("xdg-desktop-portal/blit-portals.conf"),
        format!(
            "[preferred]\ndefault={fallback}\norg.freedesktop.impl.portal.Access=blit\norg.freedesktop.impl.portal.ScreenCast=blit\norg.freedesktop.impl.portal.RemoteDesktop=none\norg.freedesktop.impl.portal.InputCapture=none\n"
        ),
    )
    .map_err(|error| error.to_string())?;
    let config_dirs = prepend_xdg(&config_home, "XDG_CONFIG_DIRS", "/etc/xdg");
    let data_dirs = prepend_xdg(&data_home, "XDG_DATA_DIRS", "/usr/local/share:/usr/share");
    // Spawn by absolute path: the gate above already resolved it, and on most
    // distributions the name alone is not on `PATH` to spawn by.
    let program =
        find_portal_frontend().ok_or_else(|| "xdg-desktop-portal not found".to_string())?;
    let child = unsafe {
        let mut command = Command::new(&program);
        // Absolute, because XDG_RUNTIME_DIR below has to name the Wayland
        // socket's directory and the PipeWire socket is not in it. See
        // `start_portal` for what goes wrong without this.
        if let Some(remote) = pipewire_remote {
            command.env("PIPEWIRE_REMOTE", remote);
        }
        command
            .env("DBUS_SESSION_BUS_ADDRESS", address)
            .env("XDG_RUNTIME_DIR", runtime_dir)
            .env("WAYLAND_DISPLAY", display)
            .env("XDG_SESSION_TYPE", "wayland")
            .env("XDG_CURRENT_DESKTOP", "blit")
            .env("XDG_CONFIG_HOME", &config_home)
            .env("XDG_DATA_HOME", &data_home)
            .env("XDG_CONFIG_DIRS", config_dirs)
            .env("XDG_DATA_DIRS", data_dirs)
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(if verbose {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .pre_exec(crate::audio::pdeathsig_hook())
            .spawn()
            .map_err(|error| format!("failed to start xdg-desktop-portal: {error}"))?
    };
    Ok((child, root))
}

fn prepend_xdg(home: &Path, name: &str, fallback: &str) -> String {
    let suffix = std::env::var(name).unwrap_or_else(|_| fallback.into());
    format!("{}:{suffix}", home.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_frontend_exit_is_consumed_once() {
        let mut process = Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn short-lived portal stand-in");
        process.wait().expect("reap portal stand-in");
        let mut child = Some(process);

        assert!(take_child_exit(&mut child));
        assert!(child.is_none());
        assert!(!take_child_exit(&mut child));
    }
}
