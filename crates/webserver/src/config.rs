use axum::extract::ws::{Message, WebSocket};
use futures_util::SinkExt;
use std::collections::HashMap;
use std::path::PathBuf;
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

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("BLIT_CONFIG") {
        return PathBuf::from(p);
    }
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
    base.join("blit").join("blit.conf")
}

pub fn read_config() -> HashMap<String, String> {
    let path = config_path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    parse_config_str(&contents)
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

fn spawn_watcher(tx: broadcast::Sender<String>) {
    use notify::{RecursiveMode, Watcher};

    let path = config_path();
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
                eprintln!("blit: config watcher failed: {e}");
                return;
            }
        };
        if let Err(e) = watcher.watch(&watch_dir, RecursiveMode::NonRecursive) {
            eprintln!("blit: config watch failed: {e}");
            return;
        }
        loop {
            match nrx.recv() {
                Ok(Ok(event)) => {
                    let dominated = file_name
                        .as_ref()
                        .is_none_or(|name| event.paths.iter().any(|p| p.file_name() == Some(name)));
                    if !dominated {
                        continue;
                    }
                    let map = read_config();
                    for (k, v) in &map {
                        let _ = tx.send(format!("{k}={v}"));
                    }
                    let _ = tx.send("ready".into());
                }
                Ok(Err(_)) => continue,
                Err(_) => break,
            }
        }
    });
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
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

pub async fn handle_config_ws(mut ws: WebSocket, token: &str, config: &ConfigState) {
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
}
