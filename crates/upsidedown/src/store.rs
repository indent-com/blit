//! Shared state (docs/upsidedown.md § Redis).
//!
//! `REDIS_URL=memory://` selects an in-process store for tests and
//! single-process development; anything else is a Redis URL.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEL_IF_EQ: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('DEL', KEYS[1])
else
  return 0
end
"#;

const REFRESH_IF_EQ: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('EXPIRE', KEYS[1], ARGV[2])
else
  return 0
end
"#;

type MemMap = Arc<Mutex<HashMap<String, (String, Option<Instant>)>>>;

/// A live subscription to session-registration events (see
/// [`Store::subscribe_sessions`]).  Backend-agnostic: yields `sid`s.
pub struct SessionEvents(tokio::sync::mpsc::Receiver<String>);

impl SessionEvents {
    /// Next registered `sid`, or `None` when the subscription ends.
    pub async fn next_sid(&mut self) -> Option<String> {
        self.0.recv().await
    }
}

/// Pub/sub channel carrying the `sid` of each session whose uplink just
/// registered, so `/attach` wakes the instant its session becomes routable.
const SESSION_CHANNEL: &str = "session-up";

#[derive(Clone)]
pub enum Store {
    Redis {
        cm: redis::aio::ConnectionManager,
        client: redis::Client,
    },
    Mem {
        map: MemMap,
        events: tokio::sync::broadcast::Sender<String>,
    },
}

impl Store {
    pub async fn open(url: &str) -> Result<Store, String> {
        if url == "memory://" {
            return Ok(Store::mem());
        }
        let client = redis::Client::open(url).map_err(|e| format!("redis: {e}"))?;
        let cm = client
            .get_connection_manager()
            .await
            .map_err(|e| format!("redis: {e}"))?;
        Ok(Store::Redis { cm, client })
    }

    pub fn mem() -> Store {
        Store::Mem {
            map: Default::default(),
            events: tokio::sync::broadcast::Sender::new(64),
        }
    }

    pub async fn get(&self, key: &str) -> Result<Option<String>, String> {
        match self {
            Store::Redis { cm, .. } => {
                use redis::AsyncCommands;
                let mut c = cm.clone();
                c.get(key).await.map_err(|e| format!("redis GET: {e}"))
            }
            Store::Mem { map, .. } => {
                let mut map = map.lock().unwrap();
                match map.get(key) {
                    Some((_, Some(deadline))) if *deadline <= Instant::now() => {
                        map.remove(key);
                        Ok(None)
                    }
                    Some((v, _)) => Ok(Some(v.clone())),
                    None => Ok(None),
                }
            }
        }
    }

    pub async fn set(&self, key: &str, val: &str) -> Result<(), String> {
        match self {
            Store::Redis { cm, .. } => {
                use redis::AsyncCommands;
                let mut c = cm.clone();
                c.set(key, val).await.map_err(|e| format!("redis SET: {e}"))
            }
            Store::Mem { map, .. } => {
                map.lock()
                    .unwrap()
                    .insert(key.to_string(), (val.to_string(), None));
                Ok(())
            }
        }
    }

    pub async fn set_ex(&self, key: &str, val: &str, ttl_secs: u64) -> Result<(), String> {
        match self {
            Store::Redis { cm, .. } => {
                use redis::AsyncCommands;
                let mut c = cm.clone();
                c.set_ex(key, val, ttl_secs)
                    .await
                    .map_err(|e| format!("redis SETEX: {e}"))
            }
            Store::Mem { map, .. } => {
                map.lock().unwrap().insert(
                    key.to_string(),
                    (
                        val.to_string(),
                        Some(Instant::now() + Duration::from_secs(ttl_secs)),
                    ),
                );
                Ok(())
            }
        }
    }

    pub async fn del(&self, key: &str) -> Result<(), String> {
        match self {
            Store::Redis { cm, .. } => {
                use redis::AsyncCommands;
                let mut c = cm.clone();
                c.del(key).await.map_err(|e| format!("redis DEL: {e}"))
            }
            Store::Mem { map, .. } => {
                map.lock().unwrap().remove(key);
                Ok(())
            }
        }
    }

    /// Delete `key` only if it currently holds `val` (compare-and-delete),
    /// so a stale worker can never clobber a fresh binding.
    pub async fn del_if_eq(&self, key: &str, val: &str) -> Result<bool, String> {
        match self {
            Store::Redis { cm, .. } => {
                let mut c = cm.clone();
                let n: i64 = redis::Script::new(DEL_IF_EQ)
                    .key(key)
                    .arg(val)
                    .invoke_async(&mut c)
                    .await
                    .map_err(|e| format!("redis EVAL: {e}"))?;
                Ok(n == 1)
            }
            Store::Mem { map, .. } => {
                let mut map = map.lock().unwrap();
                if map.get(key).is_some_and(|(v, _)| v == val) {
                    map.remove(key);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }

    /// Re-arm `key`'s TTL only if it currently holds `val`.
    pub async fn refresh_if_eq(&self, key: &str, val: &str, ttl_secs: u64) -> Result<bool, String> {
        match self {
            Store::Redis { cm, .. } => {
                let mut c = cm.clone();
                let n: i64 = redis::Script::new(REFRESH_IF_EQ)
                    .key(key)
                    .arg(val)
                    .arg(ttl_secs)
                    .invoke_async(&mut c)
                    .await
                    .map_err(|e| format!("redis EVAL: {e}"))?;
                Ok(n == 1)
            }
            Store::Mem { map, .. } => {
                let mut map = map.lock().unwrap();
                if let Some(entry) = map.get_mut(key)
                    && entry.0 == val
                {
                    entry.1 = Some(Instant::now() + Duration::from_secs(ttl_secs));
                    return Ok(true);
                }
                Ok(false)
            }
        }
    }

    /// Announce that `sid`'s uplink just registered, waking any `/attach`
    /// waiting on that session.  Best-effort: a lost notification only means
    /// the waiter falls back to its next store re-check.
    pub async fn publish_session(&self, sid: &str) {
        match self {
            Store::Redis { cm, .. } => {
                let mut c = cm.clone();
                let _: Result<(), _> = redis::cmd("PUBLISH")
                    .arg(SESSION_CHANNEL)
                    .arg(sid)
                    .query_async(&mut c)
                    .await;
            }
            Store::Mem { events, .. } => {
                let _ = events.send(sid.to_string());
            }
        }
    }

    /// Subscribe to session-registration events.  Returns a receiver of
    /// `sid`s; the underlying subscription is torn down when it is dropped.
    /// Subscribe *before* the final store re-check to avoid missing an event
    /// published in the gap.
    pub async fn subscribe_sessions(&self) -> Result<SessionEvents, String> {
        match self {
            Store::Redis { client, .. } => {
                use futures_util::StreamExt;
                let mut pubsub = client
                    .get_async_pubsub()
                    .await
                    .map_err(|e| format!("redis pubsub: {e}"))?;
                pubsub
                    .subscribe(SESSION_CHANNEL)
                    .await
                    .map_err(|e| format!("redis subscribe: {e}"))?;
                let (tx, rx) = tokio::sync::mpsc::channel(16);
                tokio::spawn(async move {
                    let mut stream = pubsub.into_on_message();
                    while let Some(msg) = stream.next().await {
                        if let Ok(sid) = msg.get_payload::<String>()
                            && tx.send(sid).await.is_err()
                        {
                            break; // receiver dropped
                        }
                    }
                });
                Ok(SessionEvents(rx))
            }
            Store::Mem { events, .. } => {
                let mut sub = events.subscribe();
                let (tx, rx) = tokio::sync::mpsc::channel(16);
                tokio::spawn(async move {
                    while let Ok(sid) = sub.recv().await {
                        if tx.send(sid).await.is_err() {
                            break;
                        }
                    }
                });
                Ok(SessionEvents(rx))
            }
        }
    }

    /// All live `(key, value)` pairs whose key starts with `prefix`.
    pub async fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>, String> {
        match self {
            Store::Redis { cm, .. } => {
                use redis::AsyncCommands;
                let mut c = cm.clone();
                let keys: Vec<String> = {
                    let mut iter = c
                        .scan_match::<_, String>(format!("{prefix}*"))
                        .await
                        .map_err(|e| format!("redis SCAN: {e}"))?;
                    let mut keys = Vec::new();
                    while let Some(k) = iter.next_item().await {
                        keys.push(k);
                    }
                    keys
                };
                let mut out = Vec::new();
                for key in keys {
                    if let Some(v) = self.get(&key).await? {
                        out.push((key, v));
                    }
                }
                Ok(out)
            }
            Store::Mem { map, .. } => {
                let now = Instant::now();
                Ok(map
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(k, (_, dl))| {
                        k.starts_with(prefix) && dl.map(|d| d > now).unwrap_or(true)
                    })
                    .map(|(k, (v, _))| (k.clone(), v.clone()))
                    .collect())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mem_store_semantics() {
        let s = Store::mem();
        s.set_ex("worker:w1", "a", 60).await.unwrap();
        s.set("session:x", "w1").await.unwrap();
        assert_eq!(s.get("session:x").await.unwrap().as_deref(), Some("w1"));

        // compare-and-delete misses on wrong value, hits on right one
        assert!(!s.del_if_eq("session:x", "w2").await.unwrap());
        assert!(s.del_if_eq("session:x", "w1").await.unwrap());
        assert_eq!(s.get("session:x").await.unwrap(), None);

        // refresh only when equal
        s.set_ex("session:y", "w1", 60).await.unwrap();
        assert!(s.refresh_if_eq("session:y", "w1", 60).await.unwrap());
        assert!(!s.refresh_if_eq("session:y", "w2", 60).await.unwrap());

        let workers = s.scan_prefix("worker:").await.unwrap();
        assert_eq!(workers, vec![("worker:w1".to_string(), "a".to_string())]);
    }

    #[tokio::test]
    async fn mem_store_expiry() {
        let s = Store::mem();
        s.set_ex("k", "v", 0).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(s.get("k").await.unwrap(), None);
        assert!(s.scan_prefix("k").await.unwrap().is_empty());
    }
}
