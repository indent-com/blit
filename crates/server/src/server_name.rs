use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// A filesystem- and socket-safe name for one blit server instance.
///
/// Names deliberately use a small portable alphabet because the same value is
/// a Unix socket suffix, a Windows named-pipe suffix, and a path component on
/// every supported platform.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ServerName(String);

impl ServerName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ServerName {
    fn default() -> Self {
        Self("default".to_owned())
    }
}

impl fmt::Display for ServerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ServerName {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() {
            return Err("server name must not be empty".to_owned());
        }
        if value.len() > 64 {
            return Err("server name must be at most 64 characters".to_owned());
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(
                "server name may contain only ASCII letters, digits, '-', '_', and '.'".to_owned(),
            );
        }
        if value.ends_with('.') {
            return Err("server name must not end with '.'".to_owned());
        }
        let windows_stem = value
            .split('.')
            .next()
            .unwrap_or(value)
            .to_ascii_uppercase();
        let windows_device = matches!(windows_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || windows_stem
                .strip_prefix("COM")
                .or_else(|| windows_stem.strip_prefix("LPT"))
                .is_some_and(|number| {
                    number.len() == 1 && matches!(number.as_bytes()[0], b'1'..=b'9')
                });
        if windows_device {
            return Err("server name must not be a reserved Windows device name".to_owned());
        }
        Ok(Self(value.to_owned()))
    }
}

/// Put a server-owned path below this instance's directory.
pub(crate) fn server_path(base: &Path, name: &ServerName, leaf: &str) -> PathBuf {
    base.join("blit")
        .join("instances")
        .join(name.as_str())
        .join(leaf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_portable_path_components() {
        for name in ["dev", "work-tree_2", "release.1"] {
            assert_eq!(name.parse::<ServerName>().unwrap().as_str(), name);
        }
        for name in [
            "",
            ".",
            "..",
            "two words",
            "a/b",
            "café",
            "work.",
            "CON",
            "com1.dev",
        ] {
            assert!(name.parse::<ServerName>().is_err(), "accepted {name:?}");
        }
        assert!("x".repeat(65).parse::<ServerName>().is_err());
    }

    #[test]
    fn default_and_named_layouts_are_isolated() {
        let base = Path::new("/state");
        let default = ServerName::default();
        let dev: ServerName = "dev".parse().unwrap();
        let test: ServerName = "test".parse().unwrap();
        assert_eq!(
            server_path(base, &default, "kv.redb"),
            base.join("blit/instances/default/kv.redb")
        );
        assert_eq!(
            server_path(base, &dev, "kv.redb"),
            base.join("blit/instances/dev/kv.redb")
        );
        assert_ne!(
            server_path(base, &dev, "extensions.redb"),
            server_path(base, &test, "extensions.redb")
        );
    }
}
