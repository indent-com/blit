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
//! So the shape is: rank every directory an icon could be in, once, and then ask
//! for the first candidate that exists — one `FS_READ` per name carrying
//! `dir/name.ext` in preference order with `FS_READ_FIRST` set, which is one
//! message and no child process. This used to build shell scripts, because the
//! fs family had no one-shot read and a sync session per file is the wrong
//! shape; `FS_READ` (docs/design/fs-read.md) is that read.
//!
//! Everything here is pure string and byte work so it can be tested natively;
//! the host only ever answers the paths these produce.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The pixel size a raster icon is ranked against.
///
/// The panel draws roughly a 2em square, so 128 covers it on a 2x display with
/// nothing left over. Bigger files are a worse trade than a slightly soft one:
/// they cross a channel, and a 512x512 PNG is twenty times the bytes for pixels
/// that get thrown away on the way to the element.
const TARGET_PIXELS: u32 = 128;

/// Largest file worth carrying. A handful of themes ship megabyte SVGs with
/// every gradient the artist owned; that is not a panel icon.
///
/// Some applications have nothing under this, and then the row keeps its letter
/// tile: Steam writes one full-size PNG into *every* size bucket, so
/// `hicolor/16x16/apps/steam_icon_327030.png` is 604 KB and the 96x96 copy is
/// 617 KB — the search finds five candidates and skips all of them. Around
/// three entries in two hundred on a games machine.
///
/// Raising this is not the fix it looks like. The ceiling is 640 KiB, because
/// base64 grows a file by a third and the result has to fit one channel message
/// (`CHANNEL_MAX_PAYLOAD`, 1 MiB) — so the cap could just cover Steam's files
/// and no more, at nearly 900 KB on the wire per row that used one. The fix is
/// to stop carrying full-size artwork at all: decode and scale to
/// [`TARGET_PIXELS`] before it crosses, which needs a decoder in the guest,
/// there being no image tool in a shipped server's PATH to shell out to.
pub const MAX_ICON_BYTES: u32 = 128 * 1024;

/// Whether an `Icon=` value is a plain name this can look up.
///
/// A name is joined onto every directory on the icon path, so the rule is that
/// it must be one path component and nothing else: an `Icon=` with a slash, a
/// quote or a control character in it is not a thing that exists.
pub fn is_lookup_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// Whether a path is one this will read.
///
/// Absolute `Icon=` values are legal and common in third-party packages. The
/// bound is the protocol's, not a shell's: a path is a length-prefixed field
/// now, so the only rule left is that it be absolute and of a sane size.
pub fn is_readable_path(path: &str) -> bool {
    path.starts_with('/') && !path.contains('\n') && path.len() <= 1024
}

/// Whether a directory found under an icon root is one icons live in.
///
/// `rel` is root-relative, as `FS_INDEX` reports it. Both layouts in the wild
/// put the category last or second — `theme/size/apps`, `theme/apps/size` — and
/// the flat roots have no category at all, so the test is simply that some
/// component says `apps`. A directory called that under an icon root, holding
/// something other than application icons, is not a thing that happens.
pub fn is_icon_dir(rel: &str) -> bool {
    rel.split('/').any(|component| component == "apps")
}

/// Every file that could hold `name`'s artwork, best directory first.
///
/// One `FS_READ` with `FS_READ_FIRST` over this list is the whole search: the
/// server stops at the first path that exists and is small enough, which — the
/// directories being ranked — is also the one the ranking would have chosen.
/// SVG before PNG within a directory, because a vector is the smaller file at
/// any size that matters here.
pub fn candidates(dirs: &[String], name: &str) -> Vec<String> {
    if !is_lookup_name(name) {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(dirs.len() * 2);
    for dir in dirs {
        out.push(format!("{dir}/{name}.svg"));
        out.push(format!("{dir}/{name}.png"));
    }
    out
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

/// Base64, because a data URL is what an `<img>` can be handed.
///
/// Written out rather than pulled in: it is the only encoder the extension needs
/// and it has no dependency of its own. Written over bytes rather than `char`s
/// because this runs in an interpreter — wasmi, not a JIT — where a
/// `String::push` per output character was the single most expensive thing the
/// extension did. A screenful of artwork is a megabyte of input, and the
/// character-at-a-time version spent seconds on it while the shell it replaced
/// had `base64(1)` doing the same work natively.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = alloc::vec![0u8; bytes.len().div_ceil(3) * 4];
    let mut at = 0;
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).map_or(0, |b| u32::from(*b));
        let b2 = chunk.get(2).map_or(0, |b| u32::from(*b));
        let word = (b0 << 16) | (b1 << 8) | b2;
        out[at] = ALPHABET[(word >> 18) as usize & 63];
        out[at + 1] = ALPHABET[(word >> 12) as usize & 63];
        out[at + 2] = if chunk.len() > 1 {
            ALPHABET[(word >> 6) as usize & 63]
        } else {
            b'='
        };
        out[at + 3] = if chunk.len() > 2 {
            ALPHABET[word as usize & 63]
        } else {
            b'='
        };
        at += 4;
    }
    // Every byte written came from the ASCII alphabet above.
    String::from_utf8(out).unwrap_or_default()
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
/// Wrap a file's bytes as a data URL, if the extension names a format a browser
/// will draw. XPM is deliberately absent: nothing renders it, and a pixmap-only
/// application is better served by the panel's own fallback.
pub fn data_url(path: &str, content: &[u8]) -> Option<String> {
    if content.is_empty() {
        return None;
    }
    let base64 = base64(content);
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
        assert!(rank_directories(&["relative/icons"]).is_empty());
    }

    #[test]
    fn only_names_that_need_no_escaping_are_looked_up() {
        assert!(is_lookup_name("org.gnome.Nautilus"));
        assert!(is_lookup_name("gimp-2.10"));
        assert!(!is_lookup_name(""));
        assert!(!is_lookup_name("../../etc/passwd"));
        assert!(!is_lookup_name("x/../y"));
        assert!(!is_lookup_name("with space"));
    }

    /// The directories are already ranked, so a `FIRST` read over this list is
    /// the whole search — the first hit is the one the ranking would have picked.
    #[test]
    fn candidates_are_every_directory_in_order_and_both_formats() {
        let dirs = vec!["/a".to_string(), "/b".to_string()];
        assert_eq!(
            candidates(&dirs, "x"),
            vec![
                "/a/x.svg".to_string(),
                "/a/x.png".to_string(),
                "/b/x.svg".to_string(),
                "/b/x.png".to_string(),
            ]
        );
        // A name that is not one path component asks for nothing at all.
        assert!(candidates(&dirs, "a b").is_empty());
        assert!(candidates(&dirs, "../../etc/passwd").is_empty());
        assert!(candidates(&[], "x").is_empty());
    }

    /// Both layouts in the wild, plus the flat roots that have no category.
    #[test]
    fn an_icon_directory_is_one_with_an_apps_component() {
        assert!(is_icon_dir("hicolor/128x128/apps"));
        assert!(is_icon_dir("Adwaita/apps/48x48"));
        assert!(is_icon_dir("apps"));
        assert!(!is_icon_dir("hicolor/128x128/mimetypes"));
        assert!(!is_icon_dir("hicolor"));
    }

    /// The three padding cases are where a hand-rolled encoder goes wrong: a
    /// full group, one spare byte, two spare bytes.
    #[test]
    fn base64_pads_every_tail_length() {
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
        assert_eq!(base64(b""), "");
        // A PNG signature, which is the first thing any of this will encode.
        assert_eq!(base64(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a]), "iVBORw0K");
    }

    #[test]
    fn a_data_url_needs_a_format_a_browser_draws() {
        assert_eq!(
            data_url("/i/x.png", b"abc").as_deref(),
            Some("data:image/png;base64,YWJj")
        );
        assert_eq!(
            data_url("/i/x.svg", b"abc").as_deref(),
            Some("data:image/svg+xml;base64,YWJj")
        );
        assert!(data_url("/i/x.xpm", b"abc").is_none());
        // An empty body is what an oversized or missing file leaves behind.
        assert!(data_url("/i/x.png", b"").is_none());
    }

    #[test]
    fn absolute_icon_paths_are_readable_and_relative_ones_are_not() {
        assert!(is_readable_path("/opt/app/icon.png"));
        assert!(!is_readable_path("icon.png"));
        // A quote is now just a character: nothing interpolates a path.
        assert!(is_readable_path("/opt/it's/icon.png"));
        assert!(!is_readable_path("/opt/two\nlines.png"));
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
