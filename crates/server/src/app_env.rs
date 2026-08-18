//! The environment every GUI application in a session inherits.
//!
//! Shared by the PTYs (where a user types the app's name), the private D-Bus
//! session (where an activated service is launched on their behalf), and native
//! children spawned with `PROCESS_SPAWN_SESSION_ENV`, so an app reaches the same
//! display however it was started.

/// The session-scoped environment a GUI child needs.
///
/// Two halves, because the variables that must be *absent* matter as much as the
/// ones that must be present: a child that inherits the host session's `DISPLAY`
/// will happily pick X11 and come up with no window at all. `build_child_env`
/// applies `remove` by filtering as it assembles a fresh envp; `Command`-based
/// spawns apply it with `env_remove`.
#[derive(Debug, Default, Clone)]
pub struct SessionEnv {
    /// Variables to set, last-write-wins against anything inherited.
    pub set: Vec<(String, String)>,
    /// Variables belonging to the host session, which must not leak through.
    pub remove: Vec<&'static str>,
}

/// Resolve the session environment from the handles a live session holds.
///
/// `TERM`/`COLORTERM` are deliberately *not* here: they describe a terminal, and
/// a native pipe child does not have one.
pub fn session_env(
    wayland_display: Option<&str>,
    x_display: Option<&str>,
    desktop_bus: Option<&str>,
    pulse_server: Option<&str>,
    pipewire_remote: Option<&str>,
) -> SessionEnv {
    let mut env = SessionEnv::default();
    let mut set = |key: &str, value: String| env.set.push((key.to_string(), value));

    if let Some(wd) = wayland_display {
        let wd_path = std::path::Path::new(wd);
        if let Some(dir) = wd_path.parent() {
            let inherited = std::env::var_os("XDG_RUNTIME_DIR");
            let differs = match &inherited {
                Some(current) => std::path::Path::new(current) != dir,
                None => true,
            };
            if differs {
                set("XDG_RUNTIME_DIR", dir.to_string_lossy().into_owned());
            }
        }
        // WAYLAND_DISPLAY must be just the socket filename (e.g. "wayland-2"),
        // not a full path.  Clients resolve it under XDG_RUNTIME_DIR.
        let name = wd_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| wd.to_string());
        set("WAYLAND_DISPLAY", name);
        for (key, value) in toolkit_env(x_display) {
            set(key, value);
        }
    }
    // The host's display belongs to a different session. `toolkit_env` names
    // this one when a bridge is listening; with no bridge the variable has to
    // go, or a toolkit offered "wayland,x11" reaches for an X server that has
    // nothing to do with blit.
    if x_display.is_none() {
        env.remove.push("DISPLAY");
    }
    // Likewise both buses: the server's own and the host desktop's each belong
    // elsewhere. The compositor-scoped bus activates portals on this Wayland
    // display while still satisfying apps (notably Spotify) that require a bus.
    env.remove.push("DBUS_SYSTEM_BUS_ADDRESS");
    if let Some(address) = desktop_bus {
        set("DBUS_SESSION_BUS_ADDRESS", address.to_string());
    } else {
        env.remove.push("DBUS_SESSION_BUS_ADDRESS");
    }
    if let Some(server) = pulse_server {
        set("PULSE_SERVER", server.to_string());
    } else {
        // No audio pipeline — point PULSE_SERVER at a path that will make
        // libpulse fail immediately.  Without this, libpulse falls back to
        // autospawn (`pulseaudio --start`) which hangs in headless /
        // container environments.  Setting PULSE_SERVER explicitly also
        // prevents inheriting a host PulseAudio server that would bypass
        // blit's audio pipeline.
        set("PULSE_SERVER", "/dev/null".to_string());
    }
    // Absolute, so it works regardless of the child's XDG_RUNTIME_DIR (which
    // points at the Wayland socket directory).
    if let Some(remote) = pipewire_remote {
        set("PIPEWIRE_REMOTE", remote.to_string());
    } else {
        env.remove.push("PIPEWIRE_REMOTE");
    }
    env
}

/// Toolkit steering for apps on blit's compositor.
///
/// Every toolkit here defaults to X11 when it is left to choose, and blit
/// used to have no X11 at all — hence the pinning. `x_display` is the display
/// of the X11 bridge when one is running: with it the pins become ordered
/// preferences instead, so a Wayland-capable app still gets Wayland and an
/// X11-only one gets a display rather than nothing.
pub fn toolkit_env(x_display: Option<&str>) -> Vec<(&'static str, String)> {
    let wayland_only = x_display.is_none();
    let pick = |only: &str, ordered: &str| if wayland_only { only } else { ordered }.to_string();
    let mut env = vec![
        ("XDG_SESSION_TYPE", "wayland".to_string()),
        ("NIXOS_OZONE_WL", "1".to_string()),
        ("ELECTRON_OZONE_PLATFORM_HINT", "wayland".to_string()),
        ("MOZ_ENABLE_WAYLAND", "1".to_string()),
        // GTK and SDL take a comma-separated list, Qt a semicolon-separated
        // one, each tried in order.
        ("GDK_BACKEND", pick("wayland", "wayland,x11")),
        ("QT_QPA_PLATFORM", pick("wayland", "wayland;xcb")),
        ("SDL_VIDEODRIVER", pick("wayland", "wayland,x11")),
    ];
    if let Some(display) = x_display {
        // A toolkit handed a display it cannot reach fails outright, so this
        // is only ever set alongside a bridge that is actually listening.
        env.push(("DISPLAY", display.to_string()));
        // Java's AWT waits for a reparenting window manager to acknowledge
        // its windows.  Nothing reparents under a bridge that hands every X
        // window straight to an xdg_toplevel, so without this a Swing app
        // shows a permanently blank frame.
        env.push(("_JAVA_AWT_WM_NONREPARENTING", "1".to_string()));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::toolkit_env;

    fn lookup(x_display: Option<&str>, key: &str) -> Option<String> {
        toolkit_env(x_display)
            .into_iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value)
    }

    /// Without a bridge there is no X to fall back to, so the pins have to
    /// stay absolute: a toolkit offered "wayland,x11" and no display can
    /// still try X11 and fail there instead of drawing.
    #[test]
    fn a_session_without_a_bridge_names_no_display_and_pins_wayland() {
        assert_eq!(lookup(None, "DISPLAY"), None);
        assert_eq!(lookup(None, "GDK_BACKEND").as_deref(), Some("wayland"));
        assert_eq!(lookup(None, "QT_QPA_PLATFORM").as_deref(), Some("wayland"));
        assert_eq!(lookup(None, "SDL_VIDEODRIVER").as_deref(), Some("wayland"));
        assert_eq!(lookup(None, "_JAVA_AWT_WM_NONREPARENTING"), None);
    }

    /// With one, Wayland still comes first for everything that can speak it —
    /// the bridge is the fallback, not the destination.
    #[test]
    fn a_bridged_session_prefers_wayland_and_offers_x11_behind_it() {
        assert_eq!(lookup(Some(":20"), "DISPLAY").as_deref(), Some(":20"));
        assert_eq!(
            lookup(Some(":20"), "GDK_BACKEND").as_deref(),
            Some("wayland,x11")
        );
        assert_eq!(
            lookup(Some(":20"), "QT_QPA_PLATFORM").as_deref(),
            Some("wayland;xcb")
        );
        assert_eq!(
            lookup(Some(":20"), "SDL_VIDEODRIVER").as_deref(),
            Some("wayland,x11")
        );
        assert_eq!(
            lookup(Some(":20"), "_JAVA_AWT_WM_NONREPARENTING").as_deref(),
            Some("1")
        );
    }
}
