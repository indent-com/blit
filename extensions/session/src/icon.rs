//! Finding the artwork an `Icon=` key names, and turning it into something a
//! browser can draw.
//!
//! The XDG icon-theme spec is a theme-inheritance search with a size-matching
//! rule per directory, driven by an `index.theme` per theme. None of that is
//! implemented here, and deliberately: the panel wants one small square per
//! application, from whatever theme happens to have it, and the difference
//! between the spec's answer and "the best-sized file of that name anywhere on
//! the icon path" is invisible at 2em. What is *not* invisible is the cost —
//! parsing every `index.theme` would be a directory walk and a read per theme
//! before the first icon appears.
//!
//! So the shape is two shell round trips: one that stats candidate paths for a
//! batch of names, and one that base64s the files the ranking picked. Shell,
//! rather than the fs family, for the same reason [`super::main`] reads desktop
//! files that way — one child and one round trip beats a sync session per file.
//!
//! Everything here is pure string work so it can be tested natively; the host
//! only ever runs the scripts these build and hands back what they printed.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Separates one section of a script's output from the next.
///
/// Printable for the same reason the desktop reader's is: a NUL cannot survive
/// a POSIX `printf` format string. No icon name or path can begin with it.
pub const SEPARATOR: &str = "@@@blit-icon@@@";

/// The pixel size a raster icon is ranked against.
///
/// The panel draws roughly a 2em square, so 128 covers it on a 2x display with
/// nothing left over. Bigger files are a worse trade than a slightly soft one:
/// they cross a channel, and a 512x512 PNG is twenty times the bytes for pixels
/// that get thrown away on the way to the element.
const TARGET_PIXELS: u32 = 128;

/// Largest file worth carrying. A handful of themes ship megabyte SVGs with
/// every gradient the artist owned; that is not a panel icon.
pub const MAX_ICON_BYTES: u64 = 128 * 1024;

/// Whether an `Icon=` value is a plain name this can look up.
///
/// The scripts interpolate the name into single quotes, so the rule is not
/// "escape it" but "refuse anything that would need escaping". An icon name
/// with a quote or a slash in it is not a thing that exists.
pub fn is_lookup_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// Whether a path can be interpolated into a script and read.
///
/// Absolute `Icon=` values are legal and common in third-party packages, and
/// the second script also reads paths this module's own glob produced.
pub fn is_readable_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains('\'')
        && !path.contains('\n')
        && !path.contains('\r')
        && path.len() <= 1024
}

/// A script that lists every candidate file for a batch of icon names.
///
/// Two globs cover both directory layouts in the wild — `theme/size/category`
/// and `theme/category/size` — and they are expanded once into the positional
/// parameters rather than per name, because the walk is the expensive half and
/// the names only cost a `test -f` each against it.
///
/// Returns `None` when there is nothing to search or nothing to search for.
pub fn search_script(
    theme_roots: &[String],
    flat_roots: &[String],
    names: &[&str],
) -> Option<String> {
    let names: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| is_lookup_name(n))
        .collect();
    if names.is_empty() {
        return None;
    }
    let mut script = String::from("set --; ");
    let mut any_root = false;
    for root in theme_roots.iter().filter(|root| !root.contains('\'')) {
        any_root = true;
        script.push_str(&format!(
            "for d in '{root}'/*/*/apps '{root}'/*/apps/* '{root}'/apps; do \
             [ -d \"$d\" ] && set -- \"$@\" \"$d\"; done; "
        ));
    }
    // Pixmaps is flat: the name sits directly in it, with no theme or size.
    for root in flat_roots.iter().filter(|root| !root.contains('\'')) {
        any_root = true;
        script.push_str(&format!("[ -d '{root}' ] && set -- \"$@\" '{root}'; "));
    }
    if !any_root {
        return None;
    }
    script.push_str("for n in");
    for name in &names {
        script.push_str(&format!(" '{name}'"));
    }
    // SVG first only as a listing order; the ranking below decides, and it
    // prefers scalable art on its own merits.
    script.push_str(&format!(
        "; do printf '{SEPARATOR}%s\\n' \"$n\"; for d in \"$@\"; do \
         for e in svg png; do [ -f \"$d/$n.$e\" ] && printf '%s\\n' \"$d/$n.$e\"; \
         done; done; done; true"
    ));
    Some(script)
}

/// A script that base64s each path, skipping anything too big to be an icon.
///
/// The section header is printed before the size check, so a file that is
/// missing or oversized comes back as an empty body rather than as a gap the
/// caller would have to align by position.
pub fn read_script(paths: &[&str]) -> Option<String> {
    let paths: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|path| is_readable_path(path))
        .collect();
    if paths.is_empty() {
        return None;
    }
    let mut script = String::from("for p in");
    for path in &paths {
        script.push_str(&format!(" '{path}'"));
    }
    script.push_str(&format!(
        "; do printf '{SEPARATOR}%s\\n' \"$p\"; \
         if [ -f \"$p\" ] && [ \"$(wc -c < \"$p\")\" -le {MAX_ICON_BYTES} ]; then \
         base64 \"$p\" | tr -d '\\n'; fi; printf '\\n'; done"
    ));
    Some(script)
}

/// Split a script's output into `(header, body lines)` sections.
///
/// Output before the first separator is dropped: a shell that printed a warning
/// to the merged stderr must not have it read as an icon path.
pub fn sections(output: &str) -> Vec<(&str, Vec<&str>)> {
    let mut out = Vec::new();
    for chunk in output.split(SEPARATOR).skip(1) {
        let Some((header, body)) = chunk.split_once('\n') else {
            continue;
        };
        out.push((
            header,
            body.lines().filter(|line| !line.is_empty()).collect(),
        ));
    }
    out
}

/// The pixel size a themed icon directory promises, if its name says one.
///
/// `48x48` is 48, and `48x48@2` is 96 — a scale suffix means the same nominal
/// size drawn at twice the density, which for this purpose is just a bigger
/// file. A `scalable` directory has no size, and neither does anything else.
fn directory_pixels(component: &str) -> Option<u32> {
    let (nominal, scale) = match component.split_once('@') {
        Some((nominal, scale)) => (nominal, scale.parse::<u32>().ok()?),
        None => (component, 1),
    };
    let (width, height) = nominal.split_once('x')?;
    let width: u32 = width.parse().ok()?;
    if width != height.parse::<u32>().ok()? {
        return None;
    }
    width.checked_mul(scale)
}

/// How good a candidate is: lower sorts first.
///
/// Scalable art wins outright — it is usually the smallest file *and* the only
/// one that stays sharp at any zoom. Among raster files, the smallest that is
/// still at least [`TARGET_PIXELS`] beats the largest that is not, because
/// downscaling looks like the icon and upscaling looks like a mistake.
fn rank(path: &str) -> (u8, u32) {
    if path.ends_with(".svg") {
        return (0, 0);
    }
    let pixels = path
        .rsplit('/')
        .nth(1)
        .and_then(directory_pixels)
        // KDE's layout puts the size one level further out.
        .or_else(|| path.rsplit('/').nth(2).and_then(directory_pixels));
    match pixels {
        Some(pixels) if pixels >= TARGET_PIXELS => (1, pixels - TARGET_PIXELS),
        Some(pixels) => (2, TARGET_PIXELS - pixels),
        // A pixmap, or a theme laying its directories out some third way.
        None => (3, 0),
    }
}

/// Pick the file to actually read, out of everything that matched a name.
///
/// Ties go to the earlier candidate, which is the earlier root — the search
/// script emits them in the icon path's own precedence order, so a user's own
/// override in `~/.local/share/icons` beats the system copy of the same size.
pub fn best<'a>(candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .enumerate()
        .min_by_key(|(index, path)| (rank(path), *index))
        .map(|(_, path)| path)
}

/// Wrap base64 bytes as a data URL, if the extension names a format a browser
/// will draw. XPM is deliberately absent: nothing renders it, and a pixmap-only
/// application is better served by the panel's own fallback.
pub fn data_url(path: &str, base64: &str) -> Option<String> {
    if base64.is_empty() {
        return None;
    }
    let mime = if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else {
        return None;
    };
    Some(format!("data:{mime};base64,{base64}"))
}

/// The icon path, from the same environment the catalog was read with.
///
/// `~/.icons` is not in any current spec but is still where a lot of art
/// installed by hand lands, so it is searched after the XDG home and before the
/// system directories.
pub fn roots(data_home: &str, home: &str, data_dirs: &str) -> (Vec<String>, Vec<String>) {
    let mut theme = Vec::new();
    let mut flat = Vec::new();
    if !data_home.is_empty() {
        theme.push(format!("{data_home}/icons"));
        flat.push(format!("{data_home}/pixmaps"));
    }
    if !home.is_empty() {
        theme.push(format!("{home}/.icons"));
    }
    for dir in data_dirs.split(':').filter(|dir| !dir.is_empty()) {
        theme.push(format!("{dir}/icons"));
        flat.push(format!("{dir}/pixmaps"));
    }
    theme.dedup();
    flat.dedup();
    (theme, flat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    #[test]
    fn scalable_art_beats_every_raster_size() {
        let candidates = [
            "/usr/share/icons/hicolor/512x512/apps/x.png",
            "/usr/share/icons/hicolor/scalable/apps/x.svg",
            "/usr/share/icons/hicolor/128x128/apps/x.png",
        ];
        assert_eq!(best(&candidates), Some(candidates[1]));
    }

    /// Downscaling looks like the icon; upscaling looks like a mistake. So the
    /// smallest file at or above the target wins, and everything below it loses
    /// to everything above it however close it gets.
    #[test]
    fn the_smallest_sufficient_raster_wins() {
        let candidates = [
            "/i/hicolor/48x48/apps/x.png",
            "/i/hicolor/512x512/apps/x.png",
            "/i/hicolor/256x256/apps/x.png",
            "/i/hicolor/96x96/apps/x.png",
        ];
        assert_eq!(best(&candidates), Some(candidates[2]));

        // Nothing reaches the target: take the largest that exists.
        let small = ["/i/hicolor/16x16/apps/x.png", "/i/hicolor/64x64/apps/x.png"];
        assert_eq!(best(&small), Some(small[1]));
    }

    /// A `@2` directory holds the same nominal size at twice the density, so it
    /// really is the bigger file and has to rank as one.
    #[test]
    fn a_scale_suffix_doubles_the_size_it_claims() {
        assert_eq!(directory_pixels("64x64@2"), Some(128));
        assert_eq!(directory_pixels("48x48"), Some(48));
        assert_eq!(directory_pixels("scalable"), None);
        // Non-square and malformed names promise nothing.
        assert_eq!(directory_pixels("16x24"), None);
        assert_eq!(directory_pixels("48x48@x"), None);
    }

    /// KDE-style themes put the category before the size, so the size is one
    /// component further from the file than hicolor puts it.
    #[test]
    fn both_directory_layouts_are_read() {
        let candidates = [
            "/i/breeze/apps/32/x.png",
            "/i/breeze/apps/128/x.png",
            "/i/hicolor/128x128/apps/x.png",
        ];
        // `apps/128` is not an `NxN` name, so those two rank as unsized and the
        // properly named one wins outright.
        assert_eq!(best(&candidates), Some(candidates[2]));

        let kde = ["/i/breeze/16x16/apps/x.png", "/i/breeze/apps/128x128/x.png"];
        assert_eq!(best(&kde), Some(kde[1]));
    }

    /// The earlier root is the higher-precedence one, so a tie must not be
    /// broken by anything else.
    #[test]
    fn ties_go_to_the_earlier_root() {
        let candidates = [
            "/home/me/.local/share/icons/hicolor/128x128/apps/x.png",
            "/usr/share/icons/hicolor/128x128/apps/x.png",
        ];
        assert_eq!(best(&candidates), Some(candidates[0]));
        assert_eq!(best(&[]), None);
    }

    #[test]
    fn only_names_that_need_no_escaping_are_looked_up() {
        assert!(is_lookup_name("org.gnome.Nautilus"));
        assert!(is_lookup_name("gimp-2.10"));
        assert!(!is_lookup_name(""));
        assert!(!is_lookup_name("../../etc/passwd"));
        assert!(!is_lookup_name("x'; rm -rf /; '"));
        assert!(!is_lookup_name("with space"));
    }

    #[test]
    fn a_search_needs_both_a_root_and_a_name() {
        let roots = vec!["/usr/share/icons".to_string()];
        assert!(search_script(&roots, &[], &["firefox"]).is_some());
        assert!(search_script(&[], &[], &["firefox"]).is_none());
        assert!(search_script(&roots, &[], &[]).is_none());
        // Every name refused leaves nothing to ask for.
        assert!(search_script(&roots, &[], &["a b"]).is_none());
    }

    /// The whole point of the separator is that a name and its candidates come
    /// back as one section even when a name matched nothing.
    #[test]
    fn sections_survive_empty_bodies_and_leading_noise() {
        let output = format!(
            "sh: warning\n{SEPARATOR}firefox\n/i/h/128x128/apps/firefox.png\n{SEPARATOR}nope\n"
        );
        let parsed = sections(&output);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "firefox");
        assert_eq!(parsed[0].1, vec!["/i/h/128x128/apps/firefox.png"]);
        assert_eq!(parsed[1].0, "nope");
        assert!(parsed[1].1.is_empty());
    }

    #[test]
    fn a_data_url_needs_a_format_a_browser_draws() {
        assert_eq!(
            data_url("/i/x.png", "AAAA").as_deref(),
            Some("data:image/png;base64,AAAA")
        );
        assert_eq!(
            data_url("/i/x.svg", "AAAA").as_deref(),
            Some("data:image/svg+xml;base64,AAAA")
        );
        assert!(data_url("/i/x.xpm", "AAAA").is_none());
        // An empty body is what an oversized or missing file leaves behind.
        assert!(data_url("/i/x.png", "").is_none());
    }

    #[test]
    fn absolute_icon_paths_are_readable_and_relative_ones_are_not() {
        assert!(is_readable_path("/opt/app/icon.png"));
        assert!(!is_readable_path("icon.png"));
        assert!(!is_readable_path("/opt/it's/icon.png"));
        assert!(read_script(&["relative.png"]).is_none());
        assert!(read_script(&["/opt/a.png"]).is_some());
    }

    #[test]
    fn the_icon_path_follows_the_data_path() {
        let (theme, flat) = roots("/h/.local/share", "/h", "/usr/local/share:/usr/share");
        assert_eq!(
            theme,
            vec![
                "/h/.local/share/icons".to_string(),
                "/h/.icons".to_string(),
                "/usr/local/share/icons".to_string(),
                "/usr/share/icons".to_string(),
            ]
        );
        assert_eq!(
            flat,
            vec![
                "/h/.local/share/pixmaps".to_string(),
                "/usr/local/share/pixmaps".to_string(),
                "/usr/share/pixmaps".to_string(),
            ]
        );
    }
}
