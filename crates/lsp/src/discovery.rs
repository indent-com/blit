//! Zero-config language-server discovery (docs/design/lsp.md
//! "Sessions and discovery").
//!
//! A compiled-in table maps root markers to PATH commands; each entry
//! declares how the upward marker walk chooses among nested matches,
//! always bounded above by the git root. Commands come only from this
//! table or the user's `blit.conf` — repository contents select which
//! entry applies, never what runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How an entry's upward marker walk chooses among nested matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootPolicy {
    /// The closest ancestor holding the marker (clangd, tsconfig).
    Nearest,
    /// The farthest ancestor holding the marker, bounded by the git
    /// root (rust-analyzer: the outermost `Cargo.toml` is the cargo
    /// workspace).
    Outermost,
}

/// One marker group: the first group with any match wins, then its
/// policy picks the directory (gopls: outermost `go.work`, else nearest
/// `go.mod`).
#[derive(Clone, Debug)]
pub struct MarkerGroup {
    pub markers: &'static [&'static str],
    pub policy: RootPolicy,
}

/// One discovery-table entry.
#[derive(Clone, Debug)]
pub struct ServerSpec {
    /// Stable id (`rust-analyzer`, `gopls`); the wire `SERVER.id`.
    pub id: String,
    /// argv, resolved on PATH at open.
    pub command: Vec<String>,
    pub groups: Vec<MarkerGroup>,
    /// File extensions routed to this server.
    pub extensions: Vec<String>,
    /// Verbatim JSON handed to `initializationOptions` (from config).
    pub init: Option<serde_json::Value>,
    /// Verbatim JSON answering `workspace/configuration` (from config).
    pub settings: Option<serde_json::Value>,
}

fn builtin_table() -> Vec<ServerSpec> {
    let entry =
        |id: &str, command: &[&str], groups: Vec<MarkerGroup>, extensions: &[&str]| ServerSpec {
            id: id.to_string(),
            command: command.iter().map(|s| s.to_string()).collect(),
            groups,
            extensions: extensions.iter().map(|s| s.to_string()).collect(),
            init: None,
            settings: None,
        };
    let group =
        |markers: &'static [&'static str], policy: RootPolicy| MarkerGroup { markers, policy };
    vec![
        entry(
            "rust-analyzer",
            &["rust-analyzer"],
            vec![group(&["Cargo.toml"], RootPolicy::Outermost)],
            &["rs"],
        ),
        entry(
            "gopls",
            &["gopls"],
            vec![
                group(&["go.work"], RootPolicy::Outermost),
                group(&["go.mod"], RootPolicy::Nearest),
            ],
            &["go"],
        ),
        entry(
            "typescript-language-server",
            &["typescript-language-server", "--stdio"],
            vec![group(
                &["tsconfig.json", "jsconfig.json", "package.json"],
                RootPolicy::Nearest,
            )],
            &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"],
        ),
        entry(
            "pyright",
            &["pyright-langserver", "--stdio"],
            vec![group(
                &["pyproject.toml", "setup.py", "requirements.txt", "Pipfile"],
                RootPolicy::Nearest,
            )],
            &["py", "pyi"],
        ),
        entry(
            "clangd",
            &["clangd"],
            vec![group(
                &["compile_commands.json", "compile_flags.txt", ".clangd"],
                RootPolicy::Nearest,
            )],
            &["c", "cc", "cpp", "cxx", "h", "hh", "hpp", "m", "mm"],
        ),
        entry(
            "zls",
            &["zls"],
            vec![group(&["build.zig"], RootPolicy::Nearest)],
            &["zig"],
        ),
        entry(
            "ruby-lsp",
            &["ruby-lsp"],
            vec![group(&["Gemfile"], RootPolicy::Nearest)],
            &["rb"],
        ),
    ]
}

/// The effective table: built-ins shadowed or extended by `blit.conf`
/// `lsp.<id>.*` keys (docs/design/lsp.md: `.init` and `.settings` are
/// verbatim JSON, never interpreted).
pub fn table() -> Vec<ServerSpec> {
    let mut entries = builtin_table();
    let overrides = config_overrides();
    for (id, over) in overrides {
        match entries.iter_mut().find(|e| e.id == id) {
            Some(entry) => over.apply(entry),
            None => {
                if let Some(command) = &over.command {
                    let mut entry = ServerSpec {
                        id: id.clone(),
                        command: command.clone(),
                        groups: Vec::new(),
                        extensions: Vec::new(),
                        init: None,
                        settings: None,
                    };
                    over.apply(&mut entry);
                    entries.push(entry);
                }
            }
        }
    }
    entries
}

#[derive(Default)]
struct Override {
    command: Option<Vec<String>>,
    roots: Option<Vec<String>>,
    root_policy: Option<RootPolicy>,
    extensions: Option<Vec<String>>,
    init: Option<serde_json::Value>,
    settings: Option<serde_json::Value>,
}

impl Override {
    fn apply(&self, entry: &mut ServerSpec) {
        if let Some(command) = &self.command {
            entry.command = command.clone();
        }
        if let Some(roots) = &self.roots {
            let leaked: Vec<&'static str> = roots
                .iter()
                .map(|s| &*Box::leak(s.clone().into_boxed_str()))
                .collect();
            entry.groups = vec![MarkerGroup {
                markers: Box::leak(leaked.into_boxed_slice()),
                policy: self.root_policy.unwrap_or(RootPolicy::Nearest),
            }];
        } else if let Some(policy) = self.root_policy {
            for group in &mut entry.groups {
                group.policy = policy;
            }
        }
        if let Some(extensions) = &self.extensions {
            entry.extensions = extensions.clone();
        }
        if self.init.is_some() {
            entry.init = self.init.clone();
        }
        if self.settings.is_some() {
            entry.settings = self.settings.clone();
        }
    }
}

fn config_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir).join("blit/blit.conf"));
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".config/blit/blit.conf"))
}

/// Parse `lsp.<id>.<key> = value` lines from `blit.conf`.
fn config_overrides() -> HashMap<String, Override> {
    let mut overrides: HashMap<String, Override> = HashMap::new();
    let Some(path) = config_path() else {
        return overrides;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return overrides;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let Some(rest) = key.strip_prefix("lsp.") else {
            continue;
        };
        let Some((id, field)) = rest.rsplit_once('.') else {
            continue;
        };
        let over = overrides.entry(id.to_string()).or_default();
        match field {
            // Whitespace-split argv; quoting can come later if a real
            // command needs it.
            "command" => over.command = Some(value.split_whitespace().map(String::from).collect()),
            "roots" => over.roots = Some(value.split_whitespace().map(String::from).collect()),
            "root_policy" => {
                over.root_policy = match value {
                    "outermost" => Some(RootPolicy::Outermost),
                    "nearest" => Some(RootPolicy::Nearest),
                    _ => None,
                }
            }
            "extensions" => {
                over.extensions = Some(value.split_whitespace().map(String::from).collect())
            }
            "init" => over.init = serde_json::from_str(value).ok(),
            "settings" => over.settings = serde_json::from_str(value).ok(),
            _ => {}
        }
    }
    overrides
}

/// The nearest ancestor of `path` (inclusive) containing `.git`, if any:
/// the upper bound of every marker walk and the attachment-root
/// fallback.
pub fn git_root(path: &Path) -> Option<PathBuf> {
    let mut dir = if path.is_dir() { path } else { path.parent()? };
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// Resolve one entry's root for `path` per its marker groups, bounded
/// above by `bound` (the git root when there is one).
pub fn resolve_root(spec: &ServerSpec, path: &Path, bound: Option<&Path>) -> Option<PathBuf> {
    let start = if path.is_dir() { path } else { path.parent()? };
    for group in &spec.groups {
        let mut found: Option<PathBuf> = None;
        let mut dir = start;
        loop {
            if group.markers.iter().any(|m| dir.join(m).exists()) {
                found = Some(dir.to_path_buf());
                if group.policy == RootPolicy::Nearest {
                    break;
                }
            }
            if bound.is_some_and(|b| dir == b) {
                break;
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => break,
            }
        }
        if found.is_some() {
            return found;
        }
    }
    None
}

/// A discovered `(spec, root)` pair for an open path.
pub struct Discovered {
    pub spec: ServerSpec,
    pub root: PathBuf,
    /// The command's binary was found on PATH.
    pub on_path: bool,
}

/// Everything discovery has to say about `path`: matching entries with
/// their roots and PATH availability, plus the attachment root (git
/// root, else the outermost backend root, else the path's directory).
pub fn discover(path: &Path) -> (Vec<Discovered>, PathBuf) {
    let bound = git_root(path);
    let mut found = Vec::new();
    for spec in table() {
        if let Some(root) = resolve_root(&spec, path, bound.as_deref()) {
            let on_path = spec.command.first().is_some_and(|bin| binary_on_path(bin));
            found.push(Discovered {
                spec,
                root,
                on_path,
            });
        }
    }
    let attachment_root = bound.unwrap_or_else(|| {
        found
            .iter()
            .map(|d| d.root.clone())
            .min_by_key(|r| r.components().count())
            .unwrap_or_else(|| {
                let dir = if path.is_dir() {
                    path
                } else {
                    path.parent().unwrap_or(path)
                };
                dir.to_path_buf()
            })
    });
    (found, attachment_root)
}

/// Route a file to the matching table entry by extension.
pub fn server_for_extension<'a>(specs: &'a [ServerSpec], path: &Path) -> Option<&'a ServerSpec> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    specs
        .iter()
        .find(|s| s.extensions.iter().any(|e| e == &ext))
}

/// LSP `languageId` for a path, from its extension.
pub fn language_id(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => "rust",
        "go" => "go",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "c" => "c",
        "h" | "cc" | "cpp" | "cxx" | "hh" | "hpp" => "cpp",
        "m" | "mm" => "objective-c",
        "zig" => "zig",
        "rb" => "ruby",
        _ => "plaintext",
    }
}

fn binary_on_path(bin: &str) -> bool {
    if bin.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(bin).exists();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let candidate = dir.join(bin);
        #[cfg(windows)]
        {
            candidate.exists() || candidate.with_extension("exe").exists()
        }
        #[cfg(not(windows))]
        candidate.exists()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn rust_analyzer_picks_outermost_cargo_toml() {
        let tmp = std::env::temp_dir().join(format!("blit-lsp-disc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        touch(&tmp.join(".git/HEAD"));
        touch(&tmp.join("Cargo.toml"));
        touch(&tmp.join("member/Cargo.toml"));
        touch(&tmp.join("member/src/lib.rs"));

        let table = builtin_table();
        let ra = table.iter().find(|s| s.id == "rust-analyzer").unwrap();
        let root = resolve_root(
            ra,
            &tmp.join("member/src/lib.rs"),
            git_root(&tmp.join("member/src")).as_deref(),
        );
        assert_eq!(root, Some(tmp.clone()));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn nearest_policy_stops_at_first_marker() {
        let tmp = std::env::temp_dir().join(format!("blit-lsp-disc2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        touch(&tmp.join("compile_commands.json"));
        touch(&tmp.join("sub/compile_commands.json"));
        touch(&tmp.join("sub/a.c"));

        let table = builtin_table();
        let clangd = table.iter().find(|s| s.id == "clangd").unwrap();
        let root = resolve_root(clangd, &tmp.join("sub/a.c"), None);
        assert_eq!(root, Some(tmp.join("sub")));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extension_routing() {
        let table = builtin_table();
        assert_eq!(
            server_for_extension(&table, Path::new("x/y.rs")).map(|s| s.id.as_str()),
            Some("rust-analyzer")
        );
        assert_eq!(
            server_for_extension(&table, Path::new("y.go")).map(|s| s.id.as_str()),
            Some("gopls")
        );
        assert!(server_for_extension(&table, Path::new("y.txt")).is_none());
    }
}
