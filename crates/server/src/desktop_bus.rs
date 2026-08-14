//! Private D-Bus session for applications running on the headless compositor.
//!
//! A host session bus is tied to the host compositor. Portal calls made on it
//! therefore create dialogs outside blit's Wayland display, where they may be
//! invisible (or have no output at all). A compositor-scoped bus keeps D-Bus
//! activation in the same Wayland environment as the PTYs that use it.

use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

pub struct DesktopBus {
    child: Child,
    address: String,
    bridge: Option<blit_desktop::Bridge>,
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
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
