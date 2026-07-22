//! Position transcoding and URI handling (docs/design/lsp.md
//! "Positions, paths, and text").
//!
//! The wire speaks 0-based lines with UTF-8 byte columns; each backend
//! speaks its negotiated `positionEncoding`. Conversion runs against the
//! exact text the backend holds (the open set) or disk bytes, so no
//! client ever learns UTF-16 exists. `file://` URIs are built and parsed
//! in exactly this one place.

use std::path::{Path, PathBuf};

use blit_remote::lsp::LspHash;

/// The encoding a backend negotiated at initialize.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    /// The LSP default; the only one every server supports.
    Utf16,
    Utf32,
}

impl PositionEncoding {
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "utf-8" => Some(PositionEncoding::Utf8),
            "utf-16" => Some(PositionEncoding::Utf16),
            "utf-32" => Some(PositionEncoding::Utf32),
            _ => None,
        }
    }
}

/// BLAKE3 truncated to 128 bits, the fs family's content address.
pub fn hash_bytes(bytes: &[u8]) -> LspHash {
    let full = blake3::hash(bytes);
    let mut out = [0u8; 16];
    out.copy_from_slice(&full.as_bytes()[..16]);
    out
}

/// The byte range of `line` (0-based) in `text`, excluding the
/// terminator. Lines beyond the end yield the empty range at EOF, so
/// conversions clamp instead of failing.
fn line_range(text: &str, line: u32) -> (usize, usize) {
    let mut start = 0usize;
    let mut remaining = line;
    let bytes = text.as_bytes();
    while remaining > 0 {
        match bytes[start..].iter().position(|&b| b == b'\n') {
            Some(nl) => start += nl + 1,
            None => return (text.len(), text.len()),
        }
        remaining -= 1;
    }
    let end = bytes[start..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|nl| start + nl)
        .unwrap_or(text.len());
    let end = if end > start && bytes[end - 1] == b'\r' {
        end - 1
    } else {
        end
    };
    (start, end)
}

/// Wire byte column → backend-encoding column within `line` of `text`.
pub fn col_to_encoding(text: &str, line: u32, byte_col: u32, enc: PositionEncoding) -> u32 {
    let (start, end) = line_range(text, line);
    let line_text = &text[start..end];
    let target = (byte_col as usize).min(line_text.len());
    match enc {
        PositionEncoding::Utf8 => target as u32,
        PositionEncoding::Utf16 => {
            let mut units = 0u32;
            for (off, ch) in line_text.char_indices() {
                if off >= target {
                    break;
                }
                units += ch.len_utf16() as u32;
            }
            units
        }
        PositionEncoding::Utf32 => line_text[..floor_char_boundary(line_text, target)]
            .chars()
            .count() as u32,
    }
}

/// Backend-encoding column → wire byte column within `line` of `text`.
pub fn col_from_encoding(text: &str, line: u32, col: u32, enc: PositionEncoding) -> u32 {
    let (start, end) = line_range(text, line);
    let line_text = &text[start..end];
    match enc {
        PositionEncoding::Utf8 => (col as usize).min(line_text.len()) as u32,
        PositionEncoding::Utf16 => {
            let mut units = 0u32;
            for (off, ch) in line_text.char_indices() {
                if units >= col {
                    return off as u32;
                }
                units += ch.len_utf16() as u32;
            }
            line_text.len() as u32
        }
        PositionEncoding::Utf32 => {
            for (count, (off, _)) in line_text.char_indices().enumerate() {
                if count as u32 >= col {
                    return off as u32;
                }
            }
            line_text.len() as u32
        }
    }
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// `file://` URI for an absolute path. Percent-encodes everything
/// outside the unreserved set plus `/`; Windows drives become
/// `file:///C:/…`.
pub fn path_to_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    let text = path.to_string_lossy();
    #[cfg(windows)]
    let text = {
        let t = text.replace('\\', "/");
        if !t.starts_with('/') {
            format!("/{t}")
        } else {
            t
        }
    };
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(*byte as char)
            }
            // Windows drive colon must stay literal for servers that
            // compare URIs textually.
            b':' => uri.push(':'),
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// Parse a `file://` URI back to a path. Returns `None` for other
/// schemes (untitled:, jdt:, …) — those locations are dropped rather
/// than mis-projected.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Strip an authority (file://localhost/…); an empty authority is
    // the common case (file:///…).
    let path_part = match rest.find('/') {
        Some(0) => rest,
        Some(slash) => &rest[slash..],
        None => return None,
    };
    let mut bytes = Vec::with_capacity(path_part.len());
    let mut iter = path_part.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hi = iter.next()?;
            let lo = iter.next()?;
            let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
            bytes.push(hex(hi)? * 16 + hex(lo)?);
        } else {
            bytes.push(b);
        }
    }
    let text = String::from_utf8(bytes).ok()?;
    #[cfg(windows)]
    {
        // `/C:/…` → `C:/…`.
        let trimmed = text
            .strip_prefix('/')
            .filter(|t| t.as_bytes().get(1) == Some(&b':'))
            .unwrap_or(&text);
        Some(PathBuf::from(trimmed))
    }
    #[cfg(not(windows))]
    Some(PathBuf::from(text))
}

/// Wire path for an absolute filesystem path: workspace-root-relative
/// and escaped when under `root`, escaped absolute otherwise (a
/// definition can land outside the workspace — the stdlib, a registry
/// checkout).
pub fn wire_path(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) => blit_fssync::escape_path(rel),
        Err(_) => blit_fssync::escape_path(path),
    }
}

/// Resolve a wire path (as produced by [`wire_path`]) against `root`.
pub fn resolve_wire(root: &Path, wire: &str) -> Option<PathBuf> {
    let bytes = blit_fssync::unescape_to_bytes(wire)?;
    let text = String::from_utf8(bytes).ok()?;
    let path = Path::new(&text);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(root.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_transcoding_roundtrips_past_non_ascii() {
        // "aé𝄞b" — é is 2 UTF-8 bytes / 1 UTF-16 unit; 𝄞 is 4 bytes /
        // 2 units.
        let text = "x\naé𝄞b\ny";
        // Byte col of 'b' on line 1: 1 + 2 + 4 = 7.
        assert_eq!(col_to_encoding(text, 1, 7, PositionEncoding::Utf16), 4);
        assert_eq!(col_from_encoding(text, 1, 4, PositionEncoding::Utf16), 7);
        // Clamped past end of line.
        assert_eq!(col_to_encoding(text, 1, 99, PositionEncoding::Utf16), 5);
        // Line past EOF.
        assert_eq!(col_to_encoding(text, 9, 0, PositionEncoding::Utf16), 0);
    }

    #[test]
    fn crlf_lines_exclude_the_cr() {
        let text = "ab\r\ncd\r\n";
        assert_eq!(col_to_encoding(text, 1, 99, PositionEncoding::Utf16), 2);
    }

    #[test]
    fn uri_roundtrip() {
        let path = Path::new("/tmp/a b/λ.rs");
        let uri = path_to_uri(path);
        assert_eq!(uri, "file:///tmp/a%20b/%CE%BB.rs");
        assert_eq!(uri_to_path(&uri), Some(path.to_path_buf()));
        assert_eq!(uri_to_path("untitled:foo"), None);
        // An authority form still resolves.
        assert_eq!(
            uri_to_path("file://localhost/tmp/x"),
            Some(PathBuf::from("/tmp/x"))
        );
    }

    #[test]
    fn wire_paths_relativize_under_root() {
        let root = Path::new("/work");
        assert_eq!(wire_path(root, Path::new("/work/src/a.rs")), "src/a.rs");
        assert_eq!(wire_path(root, Path::new("/other/b.rs")), "/other/b.rs");
        assert_eq!(
            resolve_wire(root, "src/a.rs"),
            Some(PathBuf::from("/work/src/a.rs"))
        );
        assert_eq!(
            resolve_wire(root, "/other/b.rs"),
            Some(PathBuf::from("/other/b.rs"))
        );
    }
}
