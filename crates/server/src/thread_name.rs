//! Diagnostic names for server-owned operating-system threads.
//!
//! Thread names are deliberately observational: callers retain the full
//! logical name for logs, while [`ThreadNames::os`] is compacted to the
//! platform limit and may be discarded if the operating system rejects it.

/// A full diagnostic name and its platform-sized operating-system form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadNames {
    /// Uncompacted name suitable for logs and status output.
    pub logical: String,
    /// Name suitable for [`std::thread::Builder::name`].
    pub os: String,
}

/// Build the RFC-defined name for one extension attempt thread.
///
/// `label` is an invocation label or durable extension name, never an argument
/// or other secret-bearing value. Path-like labels contribute only their final
/// component. An empty label falls back to the first eight hexadecimal digits
/// of the module hash.
pub fn extension_thread_names(
    label: Option<&str>,
    module_hash: &[u8; 32],
    extension_id: u64,
) -> ThreadNames {
    let label = sanitize_extension_label(label, module_hash);
    let short_id = extension_id as u16;
    let logical = format!("blit-ext:{label}#{short_id:04x}");
    let os = compact_extension_name(&label, short_id, platform_thread_name_limit());
    ThreadNames { logical, os }
}

/// Maximum thread-name bytes accepted by the current platform, excluding NUL.
///
/// Rust ultimately delegates to platform thread APIs. Use their smallest
/// common limits conservatively so naming failure never blocks an extension.
pub const fn platform_thread_name_limit() -> usize {
    if cfg!(any(
        target_os = "android",
        target_os = "freebsd",
        target_os = "linux",
        target_os = "netbsd",
        target_os = "openbsd"
    )) {
        15
    } else {
        63
    }
}

fn sanitize_extension_label(label: Option<&str>, module_hash: &[u8; 32]) -> String {
    let leaf = label
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or_default();
    let leaf = leaf
        .get(..leaf.len().saturating_sub(5))
        .filter(|_| {
            leaf.as_bytes()
                .get(leaf.len().saturating_sub(5)..)
                .is_some_and(|tail| tail.eq_ignore_ascii_case(b".wasm"))
        })
        .unwrap_or(leaf);

    let mut sanitized = String::with_capacity(leaf.len());
    let mut separator = false;
    for ch in leaf.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch.to_ascii_lowercase());
            separator = false;
        } else if !sanitized.is_empty() && !separator {
            sanitized.push('-');
            separator = true;
        }
    }
    while sanitized.ends_with('-') {
        sanitized.pop();
    }
    if !sanitized.is_empty() {
        return sanitized;
    }

    let mut fallback = String::with_capacity(8);
    for byte in &module_hash[..4] {
        use std::fmt::Write as _;
        let _ = write!(fallback, "{byte:02x}");
    }
    fallback
}

fn compact_extension_name(label: &str, short_id: u16, limit: usize) -> String {
    let logical = format!("blit-ext:{label}#{short_id:04x}");
    if logical.len() <= limit {
        return logical;
    }

    // Linux's 15-byte limit leaves three useful label bytes with this form:
    // `blit-e:bui-7f2a`. Keep both the component marker and stable ID suffix.
    let prefix = "blit-e:";
    let suffix = format!("-{short_id:04x}");
    let label_bytes = limit.saturating_sub(prefix.len() + suffix.len());
    let mut compact = String::with_capacity(limit);
    compact.push_str(prefix);
    compact.extend(
        label
            .as_bytes()
            .iter()
            .take(label_bytes)
            .map(|&b| char::from(b)),
    );
    compact.push_str(&suffix);
    compact
}

#[cfg(test)]
mod tests {
    use super::{compact_extension_name, extension_thread_names, sanitize_extension_label};

    const HASH: [u8; 32] = [0xab; 32];

    #[test]
    fn logical_name_keeps_label_and_stable_id_suffix() {
        let names = extension_thread_names(Some("Builder"), &HASH, 0x1234_7f2a);
        assert_eq!(names.logical, "blit-ext:builder#7f2a");
        assert!(names.os.ends_with("7f2a"));
    }

    #[test]
    fn linux_sized_compaction_keeps_component_label_and_id() {
        assert_eq!(
            compact_extension_name("builder", 0x7f2a, 15),
            "blit-e:bui-7f2a"
        );
    }

    #[test]
    fn short_names_are_not_compacted() {
        assert_eq!(compact_extension_name("x", 0x0001, 63), "blit-ext:x#0001");
    }

    #[test]
    fn labels_are_ascii_and_separators_are_collapsed() {
        assert_eq!(
            sanitize_extension_label(Some("Build 🚀 -- Release__NOW"), &HASH),
            "build-release-now"
        );
    }

    #[test]
    fn path_labels_reveal_only_the_final_component() {
        assert_eq!(
            sanitize_extension_label(Some("/srv/private/customer/Runner.WASM"), &HASH),
            "runner"
        );
        assert_eq!(
            sanitize_extension_label(Some(r"C:\\secret\\Worker.wasm"), &HASH),
            "worker"
        );
    }

    #[test]
    fn empty_labels_fall_back_to_module_hash() {
        assert_eq!(sanitize_extension_label(Some("\0/"), &HASH), "abababab");
    }
}
