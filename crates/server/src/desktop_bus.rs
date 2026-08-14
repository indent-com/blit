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

        let (portal_child, portal_root) = if bridge.is_some()
            && std::env::var("BLIT_PORTALS").map_or(true, |value| value != "0")
            && find_program("xdg-desktop-portal").is_some()
        {
            match spawn_portal_frontend(runtime_dir, display, &address, verbose) {
                Ok(value) => (Some(value.0), Some(value.1)),
                Err(error) => {
                    if verbose {
                        eprintln!("[portal] {error}");
                    }
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        Ok(Self {
            child,
            address,
            bridge,
            portal_child,
            portal_root,
        })
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
    let child = unsafe {
        Command::new("xdg-desktop-portal")
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
