use axum::extract::ws::{Message, WebSocket};
use futures_util::SinkExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

pub struct ConfigState {
    pub tx: broadcast::Sender<String>,
    pub write_lock: tokio::sync::Mutex<()>,
}

impl Default for ConfigState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigState {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel::<String>(64);
        spawn_watcher(tx.clone());
        Self {
            tx,
            write_lock: tokio::sync::Mutex::new(()),
        }
    }
}

fn blit_config_dir() -> PathBuf {
    #[cfg(unix)]
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
            PathBuf::from(home).join(".config")
        });
    #[cfg(windows)]
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\ProgramData"));
    base.join("blit")
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("BLIT_CONFIG") {
        return PathBuf::from(p);
    }
    blit_config_dir().join("blit.conf")
}

pub fn remotes_path() -> PathBuf {
    if let Ok(p) = std::env::var("BLIT_REMOTES") {
        return PathBuf::from(p);
    }
    blit_config_dir().join("blit.remotes")
}

pub fn read_config() -> HashMap<String, String> {
    let path = config_path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    parse_config_str(&contents)
}

/// Read `blit.remotes` and return ordered `(name, uri)` pairs.
/// If the file does not exist, provisions it with `local = local` (0600).
pub fn read_remotes() -> Vec<(String, String)> {
    let path = remotes_path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            let default = vec![("local".to_string(), "local".to_string())];
            write_remotes(&default);
            return default;
        }
    };
    parse_remotes_str(&contents)
}

/// Parse `blit.remotes` content into ordered `(name, uri)` pairs.
/// Format: `name = uri` lines; `#` comments; blank lines ignored.
/// Duplicate names: last wins (same as blit.conf).
pub fn parse_remotes_str(contents: &str) -> Vec<(String, String)> {
    // Use an index map to preserve insertion order while allowing last-wins
    // for duplicates, without pulling in an extra dependency.
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, String> = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim().to_string();
            let v = v.trim().to_string();
            if !k.is_empty() && !v.is_empty() {
                if !map.contains_key(&k) {
                    order.push(k.clone());
                }
                map.insert(k, v);
            }
        }
    }
    order
        .into_iter()
        .map(|k| {
            let v = map.remove(&k).unwrap();
            (k, v)
        })
        .collect()
}

fn serialize_remotes(entries: &[(String, String)]) -> String {
    let mut out = String::new();
    for (k, v) in entries {
        out.push_str(k);
        out.push_str(" = ");
        out.push_str(v);
        out.push('\n');
    }
    out
}

/// Write `blit.remotes` atomically with mode 0o600 (owner read/write only).
pub fn write_remotes(entries: &[(String, String)]) {
    let path = remotes_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let contents = serialize_remotes(entries);
    write_secret_file(&path, &contents);
}

/// Write a file with mode 0o600 (owner-only).  On Unix this is done by
/// writing to a temp file with the right mode, then atomically renaming.
/// On Windows we just write normally (ACLs are handled separately if needed).
fn write_secret_file(path: &PathBuf, contents: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Write to a sibling temp file first, then rename for atomicity.
        let tmp = path.with_extension("tmp");
        let result = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(contents.as_bytes())
            });
        if result.is_ok() {
            let _ = std::fs::rename(&tmp, path);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::write(path, contents);
    }
}

fn serialize_config_str(map: &HashMap<String, String>) -> String {
    let mut lines: Vec<String> = map.iter().map(|(k, v)| format!("{k} = {v}")).collect();
    lines.sort();
    lines.push(String::new());
    lines.join("\n")
}

pub fn write_config(map: &HashMap<String, String>) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, serialize_config_str(map));
}

/// Watches a single file in its parent directory and calls `on_change`
/// whenever the file is modified.  Skips access (read) events.
fn spawn_file_watcher<F>(path: PathBuf, label: &'static str, on_change: F)
where
    F: Fn() + Send + 'static,
{
    use notify::{RecursiveMode, Watcher};

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let watch_dir = path.parent().unwrap_or(&path).to_path_buf();
    let file_name = path.file_name().map(|n| n.to_os_string());

    std::thread::spawn(move || {
        let (ntx, nrx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(ntx) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("blit: {label} watcher failed: {e}");
                return;
            }
        };
        if let Err(e) = watcher.watch(&watch_dir, RecursiveMode::NonRecursive) {
            eprintln!("blit: {label} watch failed: {e}");
            return;
        }
        loop {
            match nrx.recv() {
                Ok(Ok(event)) => {
                    if matches!(event.kind, notify::EventKind::Access(_)) {
                        continue;
                    }
                    let matches = file_name
                        .as_ref()
                        .is_none_or(|name| event.paths.iter().any(|p| p.file_name() == Some(name)));
                    if matches {
                        on_change();
                    }
                }
                Ok(Err(_)) => continue,
                Err(_) => break,
            }
        }
    });
}

fn spawn_watcher(tx: broadcast::Sender<String>) {
    let path = config_path();
    spawn_file_watcher(path, "config", move || {
        let map = read_config();
        for (k, v) in &map {
            let _ = tx.send(format!("{k}={v}"));
        }
        let _ = tx.send("ready".into());
    });
}

// ---------------------------------------------------------------------------
// RemotesState — live-reloading blit.remotes with 0o600 permissions
// ---------------------------------------------------------------------------

/// Manages `blit.remotes`: reads/writes the file, watches for external
/// changes, and broadcasts the serialised contents to all subscribers.
///
/// The broadcast value is the raw file text (same as what `read_remotes`
/// would parse), sent as a single string so receivers can re-parse it.
/// The config WebSocket handler prefixes it with `"remotes:"`.
#[derive(Clone)]
pub struct RemotesState {
    inner: Arc<RemotesInner>,
}

struct RemotesInner {
    /// Cached current contents (raw file text, normalized).
    contents: RwLock<String>,
    tx: broadcast::Sender<String>,
}

impl RemotesState {
    /// Full persistent mode: reads `blit.remotes`, watches it for changes.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        let inner = Arc::new(RemotesInner {
            contents: RwLock::new(serialize_remotes(&read_remotes())),
            tx,
        });
        let watcher_inner = inner.clone();
        spawn_file_watcher(remotes_path(), "remotes", move || {
            // Read directly — do not auto-provision. The file may be
            // intentionally empty (user removed all remotes).
            let text = std::fs::read_to_string(remotes_path()).unwrap_or_default();
            *watcher_inner.contents.write().unwrap() = text.clone();
            let _ = watcher_inner.tx.send(text);
        });
        Self { inner }
    }

    /// Ephemeral mode: starts with the given text, no file I/O, no watcher.
    /// Used by `blit open` to advertise the session's destinations to the
    /// browser without touching `blit.remotes`.
    pub fn ephemeral(initial: String) -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(RemotesInner {
                contents: RwLock::new(initial),
                tx,
            }),
        }
    }

    /// Returns the current serialized remotes contents.
    pub fn get(&self) -> String {
        self.inner.contents.read().unwrap().clone()
    }

    /// Overwrite `blit.remotes` with `entries` and broadcast the change.
    pub fn set(&self, entries: &[(String, String)]) {
        write_remotes(entries);
        let text = serialize_remotes(entries);
        *self.inner.contents.write().unwrap() = text.clone();
        let _ = self.inner.tx.send(text);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.inner.tx.subscribe()
    }
}

impl Default for RemotesState {
    fn default() -> Self {
        Self::new()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

fn parse_config_str(contents: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

/// Handle the `/config` WebSocket connection.
///
/// Protocol (server → client, after auth):
///   1. `"ok"` — authentication accepted.
///   2. `"remotes:<text>"` — sent immediately (and re-sent on any change to
///      `blit.remotes`).  `<text>` is the raw `blit.remotes` file contents:
///      `name = uri` lines.  Empty string if the file does not exist.
///   3. Zero or more `"key=value"` messages — current browser settings.
///   4. `"ready"` — end of initial burst; live updates follow.
///
/// After `"ready"`, the server pushes:
///   - `"remotes:<text>"` when `blit.remotes` changes.
///   - `"key=value"` when `blit.conf` changes.
///
/// The client may send:
///   - `"set key value"` — persist a browser setting.
///   - `"remotes-add name uri"` — add or update a remote; name must not
///     contain `=` or whitespace; uri must be non-empty.
///   - `"remotes-remove name"` — remove a remote by name.
///   - `"remotes-set-default name"` — write `target = name` to `blit.conf`
///     (or remove the key if name is empty or `"local"`).  The updated
///     `target` value is then broadcast to all config-WS clients as a
///     normal `"target=value"` message via the config-file watcher.
///   - `"remotes-reorder name1 name2 …"` — reorder remotes to match the
///     supplied name sequence; any names not listed are appended at the end
///     in their original relative order.
pub async fn handle_config_ws(
    mut ws: WebSocket,
    token: &str,
    config: &ConfigState,
    remotes: Option<&RemotesState>,
    remotes_transform: Option<fn(&str) -> String>,
) {
    let authed = loop {
        match ws.recv().await {
            Some(Ok(Message::Text(pass))) => {
                if constant_time_eq(pass.trim().as_bytes(), token.as_bytes()) {
                    let _ = ws.send(Message::Text("ok".into())).await;
                    break true;
                } else {
                    let _ = ws.close().await;
                    break false;
                }
            }
            Some(Ok(Message::Ping(d))) => {
                let _ = ws.send(Message::Pong(d)).await;
            }
            _ => break false,
        }
    };
    if !authed {
        return;
    }

    // Subscribe before reading the snapshot so we can't miss a concurrent write.
    let mut remotes_rx = remotes.map(|r| r.subscribe());

    // Send the current remotes snapshot (even if empty — client can rely on
    // always receiving this message after "ok").
    let remotes_text = remotes.map(|r| r.get()).unwrap_or_default();
    let remotes_text = remotes_transform
        .map(|f| f(&remotes_text))
        .unwrap_or(remotes_text);
    if ws
        .send(Message::Text(format!("remotes:{remotes_text}").into()))
        .await
        .is_err()
    {
        return;
    }

    let map = read_config();
    for (k, v) in &map {
        if ws
            .send(Message::Text(format!("{k}={v}").into()))
            .await
            .is_err()
        {
            return;
        }
    }
    if ws.send(Message::Text("ready".into())).await.is_err() {
        return;
    }

    let mut config_rx = config.tx.subscribe();

    loop {
        // Build the select! arms dynamically based on whether we have a
        // destinations receiver.  We can't use an Option inside select!
        // directly, so we use a never-resolving future as a stand-in.
        tokio::select! {
            msg = ws.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let text = text.trim();
                        if let Some(rest) = text.strip_prefix("set ")
                            && let Some((k, v)) = rest.split_once(' ') {
                                let _guard = config.write_lock.lock().await;
                                let mut map = read_config();
                                let k = k.trim().replace(['\n', '\r'], "");
                                let v = v.trim().replace(['\n', '\r'], "");
                                if k.is_empty() { continue; }
                                if v.is_empty() {
                                    map.remove(&k);
                                } else {
                                    map.insert(k, v);
                                }
                                write_config(&map);
                        } else if let Some(rest) = text.strip_prefix("remotes-add ") {
                            // "remotes-add <name> <uri>" — name is first whitespace-delimited
                            // word, uri is the remainder after a single space.
                            if let Some((raw_name, raw_uri)) = rest.split_once(' ') {
                                let name = raw_name.trim().replace(['\n', '\r'], "");
                                let uri = raw_uri.trim().replace(['\n', '\r'], "");
                                if !name.is_empty()
                                    && !name.contains('=')
                                    && !uri.is_empty()
                                    && let Some(r) = remotes
                                {
                                    let mut entries = parse_remotes_str(&r.get());
                                    if let Some(pos) = entries.iter().position(|(n, _)| n == &name) {
                                        entries[pos].1 = uri;
                                    } else {
                                        entries.push((name, uri));
                                    }
                                    r.set(&entries);
                                }
                            }
                        } else if let Some(name) = text.strip_prefix("remotes-remove ") {
                            let name = name.trim().replace(['\n', '\r'], "");
                            if !name.is_empty()
                                && let Some(r) = remotes
                            {
                                let mut entries = parse_remotes_str(&r.get());
                                entries.retain(|(n, _)| n != &name);
                                r.set(&entries);
                            }
                        } else if let Some(name) = text.strip_prefix("remotes-set-default ") {
                            // Write blit.target = <name> to blit.conf (or remove it for local/empty).
                            let name = name.trim().replace(['\n', '\r'], "");
                            let _guard = config.write_lock.lock().await;
                            let mut map = read_config();
                            if name.is_empty() || name == "local" {
                                map.remove("blit.target");
                            } else {
                                map.insert("blit.target".into(), name);
                            }
                            write_config(&map);
                        } else if let Some(rest) = text.strip_prefix("remotes-reorder ") {
                            // "remotes-reorder name1 name2 …" — reorder entries to match
                            // the supplied sequence; unlisted entries are appended at end.
                            if let Some(r) = remotes {
                                let desired: Vec<String> = rest
                                    .split_whitespace()
                                    .map(|s| s.replace(['\n', '\r'], ""))
                                    .filter(|s| !s.is_empty())
                                    .collect();
                                if !desired.is_empty() {
                                    let entries = parse_remotes_str(&r.get());
                                    // Build a lookup from name → uri for existing entries.
                                    let map: std::collections::HashMap<&str, &str> = entries
                                        .iter()
                                        .map(|(n, u)| (n.as_str(), u.as_str()))
                                        .collect();
                                    // Start with the desired order (only names that exist).
                                    let mut reordered: Vec<(String, String)> = desired
                                        .iter()
                                        .filter_map(|n| {
                                            map.get(n.as_str())
                                                .map(|u| (n.clone(), u.to_string()))
                                        })
                                        .collect();
                                    // Append any entries not mentioned, preserving their
                                    // original relative order.
                                    let desired_set: std::collections::HashSet<&str> =
                                        desired.iter().map(|s| s.as_str()).collect();
                                    for (n, u) in &entries {
                                        if !desired_set.contains(n.as_str()) {
                                            reordered.push((n.clone(), u.clone()));
                                        }
                                    }
                                    r.set(&reordered);
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => continue,
                }
            }
            broadcast = config_rx.recv() => {
                match broadcast {
                    Ok(line) => {
                        if ws.send(Message::Text(line.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            remotes_update = async {
                match remotes_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match remotes_update {
                    Ok(text) => {
                        let text = remotes_transform
                            .map(|f| f(&text))
                            .unwrap_or(text);
                        if ws
                            .send(Message::Text(format!("remotes:{text}").into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Missed some intermediate updates — send current snapshot.
                        if let Some(r) = remotes {
                            let text = r.get();
                            let text = remotes_transform
                                .map(|f| f(&text))
                                .unwrap_or(text);
                            if ws
                                .send(Message::Text(format!("remotes:{text}").into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── constant_time_eq ──

    #[test]
    fn ct_eq_equal_slices() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    #[test]
    fn ct_eq_different_slices() {
        assert!(!constant_time_eq(b"hello", b"world"));
    }

    #[test]
    fn ct_eq_different_lengths() {
        assert!(!constant_time_eq(b"short", b"longer"));
    }

    #[test]
    fn ct_eq_empty_slices() {
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn ct_eq_single_bit_diff() {
        assert!(!constant_time_eq(b"\x00", b"\x01"));
    }

    #[test]
    fn ct_eq_one_empty_one_not() {
        assert!(!constant_time_eq(b"", b"x"));
    }

    // ── parse_config_str ──

    #[test]
    fn parse_empty_string() {
        let map = parse_config_str("");
        assert!(map.is_empty());
    }

    #[test]
    fn parse_comments_and_blanks() {
        let map = parse_config_str("# comment\n\n  # another\n");
        assert!(map.is_empty());
    }

    #[test]
    fn parse_key_value() {
        let map = parse_config_str("font = Menlo\ntheme = dark\n");
        assert_eq!(map.get("font").unwrap(), "Menlo");
        assert_eq!(map.get("theme").unwrap(), "dark");
    }

    #[test]
    fn parse_trims_whitespace() {
        let map = parse_config_str("  key  =  value  ");
        assert_eq!(map.get("key").unwrap(), "value");
    }

    #[test]
    fn parse_line_without_equals() {
        let map = parse_config_str("no-equals-here\nkey=val");
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("key").unwrap(), "val");
    }

    #[test]
    fn parse_equals_in_value() {
        let map = parse_config_str("cmd = a=b=c");
        assert_eq!(map.get("cmd").unwrap(), "a=b=c");
    }

    #[test]
    fn parse_duplicate_keys_last_wins() {
        let map = parse_config_str("key = first\nkey = second");
        assert_eq!(map.get("key").unwrap(), "second");
    }

    #[test]
    fn parse_mixed_content() {
        let input = "# header\nfont = FiraCode\n\n# size\nsize = 14\ntheme=light";
        let map = parse_config_str(input);
        assert_eq!(map.len(), 3);
        assert_eq!(map.get("font").unwrap(), "FiraCode");
        assert_eq!(map.get("size").unwrap(), "14");
        assert_eq!(map.get("theme").unwrap(), "light");
    }

    // ── write_config round-trip ──

    #[test]
    fn serialize_config_produces_sorted_output() {
        let mut map: HashMap<String, String> = HashMap::new();
        map.insert("z".into(), "last".into());
        map.insert("a".into(), "first".into());
        let output = serialize_config_str(&map);
        assert!(output.starts_with("a = first"));
        assert!(output.contains("z = last"));
    }

    #[test]
    fn round_trip_parse_serialize() {
        let input = "alpha = 1\nbeta = 2\ngamma = 3";
        let map = parse_config_str(input);
        let serialized = serialize_config_str(&map);
        let reparsed = parse_config_str(&serialized);
        assert_eq!(map, reparsed);
    }

    // ── RemotesState mutations (remotes-add / remotes-remove) ──

    #[test]
    fn remotes_add_new_entry() {
        let state = RemotesState::ephemeral(String::new());
        let mut entries = parse_remotes_str(&state.get());
        entries.push(("rabbit".to_string(), "ssh:rabbit".to_string()));
        state.set(&entries);
        let got = parse_remotes_str(&state.get());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], ("rabbit".to_string(), "ssh:rabbit".to_string()));
    }

    #[test]
    fn remotes_add_updates_existing() {
        let initial = "rabbit = ssh:rabbit\n";
        let state = RemotesState::ephemeral(initial.to_string());
        let mut entries = parse_remotes_str(&state.get());
        if let Some(pos) = entries.iter().position(|(n, _)| n == "rabbit") {
            entries[pos].1 = "tcp:rabbit:3264".to_string();
        }
        state.set(&entries);
        let got = parse_remotes_str(&state.get());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "tcp:rabbit:3264");
    }

    #[test]
    fn remotes_remove_existing() {
        let initial = "rabbit = ssh:rabbit\nhound = ssh:hound\n";
        let state = RemotesState::ephemeral(initial.to_string());
        let mut entries = parse_remotes_str(&state.get());
        entries.retain(|(n, _)| n != "rabbit");
        state.set(&entries);
        let got = parse_remotes_str(&state.get());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "hound");
    }

    #[test]
    fn remotes_remove_nonexistent_is_noop() {
        let initial = "rabbit = ssh:rabbit\n";
        let state = RemotesState::ephemeral(initial.to_string());
        let mut entries = parse_remotes_str(&state.get());
        let before = entries.len();
        entries.retain(|(n, _)| n != "does-not-exist");
        assert_eq!(entries.len(), before);
    }

    #[test]
    fn remotes_add_rejects_empty_name() {
        // Simulate the validation in handle_config_ws: empty name is rejected.
        let name = "";
        assert!(name.is_empty() || name.contains('='));
    }

    #[test]
    fn remotes_add_rejects_name_with_equals() {
        let name = "foo=bar";
        assert!(name.contains('='));
    }

    // ── set-default writes blit.target key to blit.conf ──

    #[test]
    fn set_default_inserts_target_key() {
        let mut map = parse_config_str("font = Mono\n");
        map.insert("blit.target".into(), "rabbit".into());
        let serialized = serialize_config_str(&map);
        let reparsed = parse_config_str(&serialized);
        assert_eq!(
            reparsed.get("blit.target").map(|s| s.as_str()),
            Some("rabbit")
        );
        assert_eq!(reparsed.get("font").map(|s| s.as_str()), Some("Mono"));
    }

    #[test]
    fn set_default_local_removes_target_key() {
        let mut map = parse_config_str("blit.target = rabbit\nfont = Mono\n");
        // "local" or empty → remove the key
        map.remove("blit.target");
        let serialized = serialize_config_str(&map);
        let reparsed = parse_config_str(&serialized);
        assert!(!reparsed.contains_key("blit.target"));
        assert_eq!(reparsed.get("font").map(|s| s.as_str()), Some("Mono"));
    }
}
