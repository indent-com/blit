//! The environment every GUI application in a session inherits.
//!
//! Shared by the PTYs (where a user types the app's name) and the private
//! D-Bus session (where an activated service is launched on their behalf), so
//! an app reaches the same display either way.

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
