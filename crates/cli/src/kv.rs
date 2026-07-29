//! `blit kv` — the server's key/value store (docs/design/kv.md).
//!
//! A prefix-watchable store the server already keeps for the web app's
//! settings; the doc notes it doubles as "a handy host-local scratch space
//! for scripts". Four operations, matching the wire family: fetch one key,
//! put or delete one key (compare-and-swap by default), and list a prefix.
//!
//! `ls` opens a subscription and prints the first coherent snapshot, then
//! stops — the same shape as `blit fs sync --once`. With `--watch` it stays
//! and streams changes instead.

use crate::transport::{Transport, read_message, write_frame};
use blit_remote::kv::{
    FEATURE_KV, KV_PUT_DELETE, KV_PUT_DURABLE, KV_PUT_NO_CAS, KV_STATUS_CONFLICT,
    KV_STATUS_NOT_FOUND, KV_STATUS_OK, KvMirror, KvPut, S2C_KV_CLOSED, S2C_KV_DONE, S2C_KV_OPENED,
    S2C_KV_UPDATE, kv_status_text, msg_kv_ack, msg_kv_fetch, msg_kv_open, msg_kv_put, msg_kv_stop,
    parse_kv_done, parse_kv_opened, parse_kv_value,
};
use blit_remote::{S2C_HELLO, S2C_QUIT, S2C_READY};
use tokio::io::AsyncRead;

const REQ_NONCE: u16 = 1;

/// Handshake, then refuse early if the server has no kv store — an old
/// server drops the opcode silently and the request would never answer.
async fn require_kv(
    reader: &mut (impl AsyncRead + Unpin),
    fragment_buf: &mut Vec<u8>,
) -> Result<(), String> {
    let mut features = 0u32;
    loop {
        let data = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            read_message(reader, fragment_buf),
        )
        .await
        .map_err(|_| "timeout waiting for server".to_string())?
        .ok_or_else(|| "server closed connection".to_string())?;
        if data.is_empty() {
            continue;
        }
        match data[0] {
            S2C_HELLO if data.len() >= 7 => {
                features = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
            }
            S2C_QUIT => return Err("server is shutting down".into()),
            S2C_READY => {
                if features & FEATURE_KV == 0 {
                    return Err(
                        "server does not support the kv store (upgrade blit on the remote)".into(),
                    );
                }
                return Ok(());
            }
            _ => {}
        }
    }
}

fn parse_hash(text: &str) -> Result<u128, String> {
    u128::from_str_radix(text.trim_start_matches("0x"), 16)
        .map_err(|_| format!("not a hex hash: {text}"))
}

/// `blit kv get KEY` — the value's bytes to stdout.
pub async fn cmd_get(transport: Transport, key: String) -> Result<i32, String> {
    use std::io::Write as _;
    let (mut reader, mut writer) = transport.split();
    let mut fb = Vec::new();
    require_kv(&mut reader, &mut fb).await?;
    if !write_frame(&mut writer, &msg_kv_fetch(REQ_NONCE, &key)).await {
        return Err("connection closed".into());
    }
    loop {
        let Some(data) = read_message(&mut reader, &mut fb).await else {
            return Err("connection closed".into());
        };
        if data.first() == Some(&S2C_QUIT) {
            return Err("server is shutting down".into());
        }
        let Some((nonce, status, _hash, value)) = parse_kv_value(&data) else {
            continue;
        };
        if nonce != REQ_NONCE {
            continue;
        }
        if status == KV_STATUS_NOT_FOUND {
            // Absent is not an error, just an empty answer — exit 1 so
            // `if blit kv get k >/dev/null; then` reads naturally.
            return Ok(1);
        }
        if status != KV_STATUS_OK {
            return Err(format!("{key}: {}", kv_status_text(status)));
        }
        // Bytes unaltered: a value may be anything, including a PNG.
        std::io::stdout()
            .write_all(&value)
            .map_err(|e| format!("writing stdout: {e}"))?;
        return Ok(0);
    }
}

/// `blit kv put KEY [VALUE]` / `blit kv rm KEY`.
#[allow(clippy::too_many_arguments)]
pub async fn cmd_put(
    transport: Transport,
    key: String,
    value: Option<String>,
    delete: bool,
    if_hash: Option<String>,
    force: bool,
    durable: bool,
    json: bool,
) -> Result<i32, String> {
    use std::io::Read as _;
    let value: Vec<u8> = if delete {
        Vec::new()
    } else if let Some(v) = value {
        v.into_bytes()
    } else {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| format!("reading stdin: {e}"))?;
        buf
    };

    let mut flags = 0u8;
    if delete {
        flags |= KV_PUT_DELETE;
    }
    if durable {
        flags |= KV_PUT_DURABLE;
    }
    // Compare-and-swap unless told otherwise, so a concurrent write is a
    // conflict rather than a silent clobber.
    let base = if let Some(h) = &if_hash {
        parse_hash(h)?
    } else {
        flags |= KV_PUT_NO_CAS;
        0
    };
    if force {
        flags |= KV_PUT_NO_CAS;
    }

    let (mut reader, mut writer) = transport.split();
    let mut fb = Vec::new();
    require_kv(&mut reader, &mut fb).await?;
    let req = KvPut {
        nonce: REQ_NONCE,
        flags,
        base,
        key: key.clone(),
        value,
    };
    if !write_frame(&mut writer, &msg_kv_put(&req)).await {
        return Err("connection closed".into());
    }
    loop {
        let Some(data) = read_message(&mut reader, &mut fb).await else {
            return Err("connection closed".into());
        };
        if data.first() == Some(&S2C_QUIT) {
            return Err("server is shutting down".into());
        }
        if data.first() != Some(&S2C_KV_DONE) {
            continue;
        }
        let Some((nonce, status, hash, mtime)) = parse_kv_done(&data) else {
            continue;
        };
        if nonce != REQ_NONCE {
            continue;
        }
        if status == KV_STATUS_CONFLICT {
            eprintln!("blit: {key} changed under us (current hash {hash:032x})");
            return Ok(1);
        }
        if status != KV_STATUS_OK {
            return Err(format!("{key}: {}", kv_status_text(status)));
        }
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "key": key,
                    "hash": format!("{hash:032x}"),
                    "mtime_ns": mtime,
                })
            );
        }
        return Ok(0);
    }
}

/// `blit kv ls [PREFIX]` — the keys under a prefix.
///
/// Applies updates into a `KvMirror` and acks, which is the family's
/// complete client obligation, then prints the map once the snapshot is
/// coherent. `--watch` keeps going and prints each later change instead.
pub async fn cmd_ls(
    transport: Transport,
    prefix: String,
    watch: bool,
    values: bool,
    json: bool,
) -> Result<i32, String> {
    let (mut reader, mut writer) = transport.split();
    let mut fb = Vec::new();
    require_kv(&mut reader, &mut fb).await?;
    // `inline_max` carries values, and **0 means no limit** (the server
    // reads it that way — crates/server/src/kv.rs). So --values asks for 0,
    // and a plain listing asks for 1 byte, which is as close to
    // metadata-only as the wire allows: there is no "never inline".
    let inline_max = if values { 0 } else { 1 };
    if !write_frame(&mut writer, &msg_kv_open(REQ_NONCE, 0, inline_max, &prefix)).await {
        return Err("connection closed".into());
    }

    let mut kv_id: Option<u16> = None;
    let mut mirror = KvMirror::new();
    let mut printed = false;
    loop {
        let Some(data) = read_message(&mut reader, &mut fb).await else {
            return Err("connection closed".into());
        };
        if data.is_empty() {
            continue;
        }
        match data[0] {
            S2C_QUIT => return Err("server is shutting down".into()),
            S2C_KV_OPENED => {
                if let Some((nonce, id, status, detail)) = parse_kv_opened(&data)
                    && nonce == REQ_NONCE
                {
                    if status != KV_STATUS_OK {
                        let why = if detail.is_empty() {
                            kv_status_text(status).to_string()
                        } else {
                            detail
                        };
                        return Err(format!("cannot watch '{prefix}': {why}"));
                    }
                    kv_id = Some(id);
                }
            }
            S2C_KV_CLOSED => return Err("server closed the subscription".into()),
            S2C_KV_UPDATE => {
                let Some(id) = kv_id else { continue };
                if data.len() >= 3 && u16::from_le_bytes([data[1], data[2]]) != id {
                    continue;
                }
                let before: Vec<String> = if printed {
                    mirror.live.keys().cloned().collect()
                } else {
                    Vec::new()
                };
                let Some(update_id) = mirror.apply_update(&data) else {
                    continue;
                };
                // Ack is not optional: the server bounds unacked updates.
                let _ = write_frame(&mut writer, &msg_kv_ack(id, update_id)).await;

                if !mirror.snapshot_done {
                    continue;
                }
                if !printed {
                    printed = true;
                    for (key, e) in &mirror.live {
                        print_entry(key, e, values, json, "upsert");
                    }
                    if !watch {
                        let _ = write_frame(&mut writer, &msg_kv_stop(id)).await;
                        return Ok(0);
                    }
                } else {
                    // Live phase: report what this update actually changed,
                    // by diffing against the keys we held before applying.
                    for (key, e) in &mirror.live {
                        if !before.contains(key) {
                            print_entry(key, e, values, json, "upsert");
                        }
                    }
                    for key in &before {
                        if !mirror.live.contains_key(key) {
                            if json {
                                println!("{}", serde_json::json!({"type": "delete", "key": key}));
                            } else {
                                println!("- {key}");
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn print_entry(key: &str, e: &blit_remote::kv::KvEntry, values: bool, json: bool, kind: &str) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "type": kind,
                "key": key,
                "hash": format!("{:032x}", e.hash),
                "size": e.size,
                "mtime_ns": e.mtime_ns,
                "value": e.value.as_deref().map(String::from_utf8_lossy),
            })
        );
    } else if values && let Some(v) = &e.value {
        // TSV, value last because it is the part that may be long.
        println!("{key}\t{}\t{}", e.size, String::from_utf8_lossy(v));
    } else {
        println!("{key}\t{}", e.size);
    }
}
