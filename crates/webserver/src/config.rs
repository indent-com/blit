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
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
            PathBuf::from(home).join(".config")
        });
    base.join("blit").join("blit.conf")
}

pub fn read_config() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let path = config_path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return map,
    };
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

pub fn write_config(map: &HashMap<String, String>) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut lines: Vec<String> = map.iter().map(|(k, v)| format!("{k} = {v}")).collect();
    lines.sort();
    lines.push(String::new());
    let _ = std::fs::write(&path, lines.join("\n"));
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
                    let dominated = file_name.as_ref().is_none_or(|name| {
                        event.paths.iter().any(|p| p.file_name() == Some(name))
                    });
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
                        if let Some(rest) = text.strip_prefix("set ") {
                            if let Some((k, v)) = rest.split_once(' ') {
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
