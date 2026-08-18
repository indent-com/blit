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

/// A script that lists the directories icons could be in, one line each.
///
/// Run once and cached, because it is the only part of this that depends on
/// what is installed rather than on what is being asked for: two globs cover
/// both layouts in the wild — `theme/size/category` and `theme/category/size` —
/// and expanding them is the expensive half. Ranked afterwards by
/// [`rank_directories`], which is what turns the cached list into a preference
/// order a shell can walk without knowing anything about icon sizes.
pub fn directories_script(theme_roots: &[String], flat_roots: &[String]) -> Option<String> {
    let mut script = String::new();
    for root in theme_roots.iter().filter(|root| !root.contains('\'')) {
        script.push_str(&format!(
            "for d in '{root}'/*/*/apps '{root}'/*/apps/* '{root}'/apps; do \
             [ -d \"$d\" ] && printf '%s\\n' \"$d\"; done; "
        ));
    }
    // Pixmaps is flat: the name sits directly in it, with no theme or size.
    for root in flat_roots.iter().filter(|root| !root.contains('\'')) {
        script.push_str(&format!("[ -d '{root}' ] && printf '%s\\n' '{root}'; "));
    }
    if script.is_empty() {
        return None;
    }
    script.push_str("true");
    Some(script)
}

/// Order the candidate directories best-first, so the first hit is the answer.
///
/// This is [`rank`]'s judgement moved from the file to the directory, which is
/// what lets one pass over the names do the whole job: scalable first, then the
/// smallest size at or above [`TARGET_PIXELS`], then the largest below it, then
/// anything whose name promises no size at all — a pixmaps directory, or a
/// theme laying itself out some third way.
///
/// The sort is stable, so directories that rank alike stay in the order the
/// icon path put them: a user's own `~/.local/share/icons` override still beats
/// the system copy of the same size.
pub fn rank_directories(dirs: &[&str]) -> Vec<String> {
    let mut ranked: Vec<&str> = dirs
        .iter()
        .copied()
        .filter(|dir| is_readable_path(dir))
        .collect();
    ranked.sort_by_key(|dir| directory_rank(dir));
    ranked.into_iter().map(String::from).collect()
}

/// Where one candidate directory sorts. Lower is better; see [`rank_directories`].
fn directory_rank(dir: &str) -> (u8, u32) {
    let mut components = dir.split('/');
    if components.clone().any(|component| component == "scalable") {
        return (0, 0);
    }
    match components.find_map(directory_pixels) {
        Some(pixels) if pixels >= TARGET_PIXELS => (1, pixels - TARGET_PIXELS),
        Some(pixels) => (2, TARGET_PIXELS - pixels),
        None => (3, 0),
    }
}

/// A script that finds and reads one icon per name, in a single pass.
///
/// The whole resolution in one child: walking the ranked directories in order
/// means the first file that exists is also the one the ranking would have
/// picked, so there is no reason to report candidates back and ask again. That
/// second round trip was most of the latency — a batch cost two spawns, and a
/// list being scrolled needs one batch per screen.
///
/// A file over [`MAX_ICON_BYTES`] is skipped rather than accepted, so an
/// application whose 512px art is a megabyte still gets its 64px tile.
///
/// Each section is the name, then the path, then the base64 — the path because
/// the extension is what the format is read from, and a name that matched
/// nothing has an empty section rather than no section at all.
pub fn fetch_script(dirs: &[String], names: &[&str]) -> Option<String> {
    let names: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| is_lookup_name(name))
        .collect();
    if names.is_empty() || dirs.is_empty() {
        return None;
    }
    let mut script = String::from("set --");
    for dir in dirs.iter().filter(|dir| !dir.contains('\'')) {
        script.push_str(&format!(" '{dir}'"));
    }
    script.push_str("; for n in");
    for name in &names {
        script.push_str(&format!(" '{name}'"));
    }
    // `break 2` leaves both the extension loop and the directory loop: one
    // answer per name, and the search stops at it.
    script.push_str(&format!(
        "; do printf '{SEPARATOR}%s\\n' \"$n\"; for d in \"$@\"; do \
         for e in svg png; do f=\"$d/$n.$e\"; \
         if [ -f \"$f\" ] && [ \"$(wc -c < \"$f\")\" -le {MAX_ICON_BYTES} ]; then \
         printf '%s\\n' \"$f\"; base64 \"$f\" | tr -d '\\n'; printf '\\n'; break 2; \
         fi; done; done; done; true"
    ));
    Some(script)
}

/// A script that base64s each path, skipping anything too big to be an icon.
///
/// Only absolute `Icon=` values reach this; a name goes through
/// [`fetch_script`], which does the same reading as part of the search.
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

    /// The whole ranking is expressed as directory order, because the script
    /// takes the first file it finds. Scalable first; then the smallest size at
    /// or above the target, because downscaling looks like the icon and
    /// upscaling looks like a mistake; then the largest below it; then whatever
    /// promises no size at all.
    #[test]
    fn directories_rank_scalable_first_then_by_how_well_the_size_fits() {
        let dirs = [
            "/i/hicolor/48x48/apps",
            "/usr/share/pixmaps",
            "/i/hicolor/512x512/apps",
            "/i/hicolor/scalable/apps",
            "/i/hicolor/256x256/apps",
            "/i/hicolor/96x96/apps",
        ];
        assert_eq!(
            rank_directories(&dirs),
            vec![
                "/i/hicolor/scalable/apps".to_string(),
                "/i/hicolor/256x256/apps".to_string(),
                "/i/hicolor/512x512/apps".to_string(),
                "/i/hicolor/96x96/apps".to_string(),
                "/i/hicolor/48x48/apps".to_string(),
                "/usr/share/pixmaps".to_string(),
            ]
        );
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

    /// KDE-style themes put the category before the size. Reading the size from
    /// anywhere in the path rather than from a fixed position is what covers
    /// both layouts with one rule.
    #[test]
    fn both_directory_layouts_are_read() {
        assert_eq!(directory_rank("/i/breeze/apps/128x128"), (1, 0));
        assert_eq!(directory_rank("/i/hicolor/128x128/apps"), (1, 0));
        assert_eq!(directory_rank("/i/breeze/apps/scalable"), (0, 0));
        // `apps/128` is not an `NxN` name, so it promises nothing.
        assert_eq!(directory_rank("/i/breeze/apps/128"), (3, 0));
    }

    /// The earlier root is the higher-precedence one, so directories that rank
    /// alike must stay in the order the icon path put them.
    #[test]
    fn ties_keep_the_icon_path_order() {
        let dirs = [
            "/home/me/.local/share/icons/hicolor/128x128/apps",
            "/usr/share/icons/hicolor/128x128/apps",
        ];
        assert_eq!(rank_directories(&dirs), vec![dirs[0], dirs[1]]);
        assert!(rank_directories(&[]).is_empty());
        // A path that could not be interpolated safely is not a directory.
        assert!(rank_directories(&["relative/icons", "/it's/icons"]).is_empty());
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
    fn a_fetch_needs_both_a_directory_and_a_name() {
        let dirs = vec!["/usr/share/icons/hicolor/128x128/apps".to_string()];
        assert!(fetch_script(&dirs, &["firefox"]).is_some());
        assert!(fetch_script(&[], &["firefox"]).is_none());
        assert!(fetch_script(&dirs, &[]).is_none());
        // Every name refused leaves nothing to ask for.
        assert!(fetch_script(&dirs, &["a b"]).is_none());
    }

    #[test]
    fn a_directory_sweep_needs_a_root() {
        let roots = vec!["/usr/share/icons".to_string()];
        assert!(directories_script(&roots, &[]).is_some());
        assert!(directories_script(&[], &[]).is_none());
        // A root that would need escaping is refused, and refusing every root
        // leaves nothing to sweep.
        assert!(directories_script(&["/it's/icons".to_string()], &[]).is_none());
    }

    /// The script stops at the first file it finds, so its own loop order is
    /// the ranking. `break 2` is what leaves both loops rather than only the
    /// extension one, which would go on to test every remaining directory.
    #[test]
    fn a_fetch_stops_at_the_first_hit() {
        let dirs = vec!["/a".to_string(), "/b".to_string()];
        let script = fetch_script(&dirs, &["x"]).expect("builds");
        assert!(script.contains("set -- '/a' '/b'"));
        assert!(script.contains("break 2"));
        // Both formats, scalable first within a directory.
        assert!(script.contains("for e in svg png"));
    }

    /// The whole point of the separator is that a name comes back as a section
    /// even when it matched nothing — an absent section and an empty one would
    /// otherwise be told apart only by counting.
    #[test]
    fn sections_survive_empty_bodies_and_leading_noise() {
        let output = format!(
            "sh: warning\n{SEPARATOR}firefox\n/i/h/128x128/apps/firefox.png\nAAAA\n{SEPARATOR}nope\n"
        );
        let parsed = sections(&output);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "firefox");
        assert_eq!(
            parsed[0].1,
            vec!["/i/h/128x128/apps/firefox.png", "AAAA"],
            "path first, then the base64 that names its format"
        );
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
