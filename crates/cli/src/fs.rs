//! `blit fs` — filesystem state sync client (docs/fs-watch.md).
//!
//! The complete client obligation: apply updates to a map, ack. Everything
//! here beyond that is presentation.

use crate::transport::{Transport, read_message, write_frame};
use blit_remote::fs::{
    FEATURE_FS_SYNC, FEATURE_FS_WRITE, FS_CLOSED_BACKEND_FAILED, FS_CLOSED_CLIENT_REQUEST,
    FS_CLOSED_PERMISSION_LOST, FS_CLOSED_RESOURCE_LIMIT, FS_CLOSED_ROOT_GONE, FS_DONE_CONFLICT,
    FS_DONE_OK, FS_ENTRY_DIR, FS_ENTRY_FILE, FS_ENTRY_SYMLINK, FS_ENTRY_TYPE_MASK, FS_OP_MKDIR,
    FS_OP_MKPARENTS, FS_OP_REMOVE, FS_OP_RENAME, FS_STATUS_OK, FS_SYNC_CONTENT, FS_SYNC_RECURSIVE,
    FS_UPDATE_SYNC, FS_WRITE_CONTENT_FULL, FS_WRITE_DURABLE, FS_WRITE_MKPARENTS, FS_WRITE_NO_CAS,
    FsMirror, FsOp, FsRecord, FsWrite, S2C_FS_CLOSED, S2C_FS_DONE, S2C_FS_SYNCED, S2C_FS_UPDATE,
    fs_done_status_text, fs_records, fs_update_records, msg_fs_ack, msg_fs_op, msg_fs_sync,
    msg_fs_write, parse_fs_done,
};
use blit_remote::{S2C_HELLO, S2C_QUIT, S2C_READY};
use tokio::io::{AsyncRead, AsyncWrite};

const SYNC_NONCE: u16 = 1;
const REQ_NONCE: u16 = 2;

pub async fn cmd_sync(
    transport: Transport,
    path: String,
    content: bool,
    no_recursive: bool,
    once: bool,
    json: bool,
) -> Result<(), String> {
    let (mut reader, mut writer) = transport.split();
    let mut fragment_buf: Vec<u8> = Vec::new();

    let features = handshake(&mut reader, &mut fragment_buf).await?;
    if features & FEATURE_FS_SYNC == 0 {
        return Err("server does not support filesystem sync (upgrade blit on the remote)".into());
    }

    let mut flags = 0u8;
    if !no_recursive {
        flags |= FS_SYNC_RECURSIVE;
    }
    if content {
        flags |= FS_SYNC_CONTENT;
    }
    if !write_frame(&mut writer, &msg_fs_sync(SYNC_NONCE, flags, 0, 0, &path)).await {
        return Err("connection closed".into());
    }

    let mut mirror = FsMirror::new();
    let mut sync_id: Option<u16> = None;
    let mut ready = false;
    loop {
        let Some(data) = read_message(&mut reader, &mut fragment_buf).await else {
            return Err("connection closed".into());
        };
        if data.is_empty() {
            continue;
        }
        match data[0] {
            S2C_FS_SYNCED if data.len() >= 8 => {
                let nonce = u16::from_le_bytes([data[1], data[2]]);
                if nonce != SYNC_NONCE {
                    continue;
                }
                let id = u16::from_le_bytes([data[3], data[4]]);
                let status = data[5];
                let detail_len = u16::from_le_bytes([data[6], data[7]]) as usize;
                let detail = data
                    .get(8..8 + detail_len)
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                if status != FS_STATUS_OK {
                    return Err(format!("sync failed: {}", status_text(status, &detail)));
                }
                sync_id = Some(id);
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"type": "synced", "sync_id": id, "root": detail})
                    );
                } else {
                    eprintln!("syncing {detail}");
                }
            }
            S2C_FS_UPDATE if data.len() >= 8 => {
                let Some(id) = sync_id else { continue };
                if u16::from_le_bytes([data[1], data[2]]) != id {
                    continue;
                }
                let update_flags = data[7];
                // JSON consumers replay events into their own map, so the
                // staging boundaries must be visible: a server may restage
                // (RESET … SYNC) at any time instead of sending a diff.
                if json && update_flags & blit_remote::fs::FS_UPDATE_RESET != 0 {
                    println!("{}", serde_json::json!({"type": "reset"}));
                }
                // Decode records for event display before the mirror
                // consumes the message (live phase only; the snapshot is
                // printed whole at SYNC).
                let events = if ready || json {
                    fs_update_records(&data).map(|records| {
                        let mut out = Vec::new();
                        for record in fs_records(&records) {
                            out.push(describe(&record, &mirror, json));
                        }
                        out
                    })
                } else {
                    None
                };
                let Some(update_id) = mirror.apply_update(&data) else {
                    return Err("malformed update from server".into());
                };
                if !write_frame(&mut writer, &msg_fs_ack(id, update_id)).await {
                    return Err("connection closed".into());
                }
                for line in events.into_iter().flatten() {
                    println!("{line}");
                }
                if update_flags & FS_UPDATE_SYNC != 0 {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({"type": "sync", "entries": mirror.live.len()})
                        );
                    }
                    if !ready {
                        ready = true;
                        if !json {
                            print_snapshot(&mirror);
                        }
                        if once {
                            return Ok(());
                        }
                        if !json {
                            eprintln!("watching for changes (ctrl-c to stop)…");
                        }
                    }
                }
            }
            S2C_FS_CLOSED if data.len() >= 4 => {
                let Some(id) = sync_id else { continue };
                if u16::from_le_bytes([data[1], data[2]]) != id {
                    continue;
                }
                let reason = data[3];
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"type": "closed", "reason": reason_text(reason)})
                    );
                }
                return match reason {
                    FS_CLOSED_CLIENT_REQUEST => Ok(()),
                    r => Err(format!("sync closed: {}", reason_text(r))),
                };
            }
            S2C_QUIT => return Err("server is shutting down".into()),
            _ => {}
        }
    }
}

pub(crate) async fn handshake(
    reader: &mut (impl AsyncRead + Unpin),
    fragment_buf: &mut Vec<u8>,
) -> Result<u32, String> {
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
            S2C_READY => return Ok(features),
            S2C_QUIT => return Err("server is shutting down".to_string()),
            _ => {}
        }
    }
}

fn kind_char(entry_flags: u8) -> char {
    match entry_flags & FS_ENTRY_TYPE_MASK {
        FS_ENTRY_FILE => 'f',
        FS_ENTRY_DIR => 'd',
        FS_ENTRY_SYMLINK => 'l',
        _ => 'o',
    }
}

fn kind_name(entry_flags: u8) -> &'static str {
    match entry_flags & FS_ENTRY_TYPE_MASK {
        FS_ENTRY_FILE => "file",
        FS_ENTRY_DIR => "dir",
        FS_ENTRY_SYMLINK => "symlink",
        _ => "other",
    }
}

fn display_path(path: &str) -> &str {
    if path.is_empty() { "." } else { path }
}

/// One display line per record. Uses the mirror's pre-apply state to
/// distinguish additions from modifications; JSON events mirror the wire.
fn describe(record: &FsRecord<'_>, mirror: &FsMirror, json: bool) -> String {
    match record {
        FsRecord::Upsert {
            path,
            entry_flags,
            size,
            mtime_ns,
            mode,
            hash,
            ..
        } => {
            if json {
                let mut v = serde_json::json!({
                    "type": "upsert",
                    "path": path,
                    "kind": kind_name(*entry_flags),
                    "size": size,
                    "mtime_ns": mtime_ns,
                    "mode": mode,
                });
                if *hash != 0 {
                    v["hash"] = serde_json::Value::String(format!("{hash:032x}"));
                }
                v.to_string()
            } else {
                let marker = if mirror.live.contains_key(*path) {
                    '~'
                } else {
                    '+'
                };
                format!(
                    "{marker} {} {}",
                    kind_char(*entry_flags),
                    display_path(path)
                )
            }
        }
        FsRecord::Delete { path } => {
            if json {
                serde_json::json!({"type": "delete", "path": path}).to_string()
            } else {
                format!("- {}", display_path(path))
            }
        }
        FsRecord::Move { from, to } => {
            if json {
                serde_json::json!({"type": "move", "from": from, "to": to}).to_string()
            } else {
                format!("> {} -> {}", display_path(from), display_path(to))
            }
        }
    }
}

fn print_snapshot(mirror: &FsMirror) {
    for (path, node) in &mirror.live {
        println!(
            "{} {:>12} {}",
            kind_char(node.entry_flags),
            node.size,
            display_path(path)
        );
    }
}

fn status_text(status: u8, detail: &str) -> String {
    use blit_remote::fs::{
        FS_STATUS_NOT_FOUND, FS_STATUS_PERMISSION_DENIED, FS_STATUS_RESOURCE_LIMIT,
    };
    let name = match status {
        FS_STATUS_NOT_FOUND => "not found",
        FS_STATUS_PERMISSION_DENIED => "permission denied",
        FS_STATUS_RESOURCE_LIMIT => "resource limit",
        _ => "error",
    };
    if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {detail}")
    }
}

fn reason_text(reason: u8) -> &'static str {
    match reason {
        FS_CLOSED_CLIENT_REQUEST => "client request",
        FS_CLOSED_ROOT_GONE => "root deleted or renamed away",
        FS_CLOSED_PERMISSION_LOST => "permission lost",
        FS_CLOSED_BACKEND_FAILED => "backend failure",
        FS_CLOSED_RESOURCE_LIMIT => "resource limit",
        _ => "unknown",
    }
}

// ── Writes (docs/design/fs-write.md) ─────────────────────────────────────

/// Absolutize `--root` against the client's cwd so the daemon's cwd is
/// irrelevant (the same fix `blit lsp` carries).
fn client_abs(path: &str) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| std::path::PathBuf::from(path))
        .to_string_lossy()
        .into_owned()
}

/// A client-minted path is valid UTF-8, so its wire form is itself except
/// a literal `%`, which escapes to `%25` (docs/design/fs-write.md "Paths").
fn escape_wire(path: &str) -> String {
    path.trim_start_matches("./").replace('%', "%25")
}

fn parse_hash(hex: &str) -> Result<u128, String> {
    u128::from_str_radix(hex.trim_start_matches("0x"), 16)
        .map_err(|_| format!("invalid hash (expected hex): {hex}"))
}

fn parse_mode(mode: Option<&str>) -> Result<u32, String> {
    match mode {
        None => Ok(0),
        Some(m) => u32::from_str_radix(m.trim_start_matches("0o"), 8)
            .map_err(|_| format!("invalid mode: {m}")),
    }
}

/// Handshake, require `FEATURE_FS_WRITE`, and open a (non-recursive,
/// content-less) sync of `root` to obtain a `sync_id` the write routes to.
async fn open_write_root(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    fragment_buf: &mut Vec<u8>,
    root: &str,
) -> Result<u16, String> {
    let features = handshake(reader, fragment_buf).await?;
    if features & FEATURE_FS_WRITE == 0 {
        return Err(
            "server does not support filesystem writes (BLIT_FS_WRITE=0 on the remote, or upgrade blit)"
                .into(),
        );
    }
    if !write_frame(writer, &msg_fs_sync(SYNC_NONCE, 0, 0, 0, root)).await {
        return Err("connection closed".into());
    }
    loop {
        let Some(data) = read_message(reader, fragment_buf).await else {
            return Err("connection closed".into());
        };
        if data.is_empty() {
            continue;
        }
        if data[0] == S2C_QUIT {
            return Err("server is shutting down".into());
        }
        if data[0] == S2C_FS_SYNCED && data.len() >= 8 {
            if u16::from_le_bytes([data[1], data[2]]) != SYNC_NONCE {
                continue;
            }
            let id = u16::from_le_bytes([data[3], data[4]]);
            let status = data[5];
            let detail_len = u16::from_le_bytes([data[6], data[7]]) as usize;
            let detail = data
                .get(8..8 + detail_len)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            if status != FS_STATUS_OK {
                return Err(format!("open failed: {}", status_text(status, &detail)));
            }
            return Ok(id);
        }
    }
}

async fn await_fs_done(
    reader: &mut (impl AsyncRead + Unpin),
    fragment_buf: &mut Vec<u8>,
    nonce: u16,
) -> Result<(u8, u128, u64), String> {
    loop {
        let Some(data) = read_message(reader, fragment_buf).await else {
            return Err("connection closed".into());
        };
        if data.is_empty() {
            continue;
        }
        if data[0] == S2C_QUIT {
            return Err("server is shutting down".into());
        }
        if data[0] == S2C_FS_DONE
            && let Some((n, status, hash, mtime)) = parse_fs_done(&data)
            && n == nonce
        {
            return Ok((status, hash, mtime));
        }
    }
}

/// Print an `FS_DONE` and map it to an exit code: 0 ok, 1 conflict, and an
/// `Err` (surfaced as a message + exit 1) for a real failure.
fn report_done(status: u8, hash: u128, mtime_ns: u64, json: bool) -> Result<i32, String> {
    match status {
        FS_DONE_OK => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"status": "ok", "hash": format!("{hash:032x}"), "mtimeNs": mtime_ns})
                );
            } else if hash != 0 {
                eprintln!("ok {hash:032x}");
            } else {
                eprintln!("ok");
            }
            Ok(0)
        }
        FS_DONE_CONFLICT => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"status": "conflict", "hash": format!("{hash:032x}")})
                );
            } else {
                eprintln!("conflict: on-disk hash is {hash:032x} (rebase and retry, or --force)");
            }
            Ok(1)
        }
        _ => Err(fs_done_status_text(status).to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_write(
    transport: Transport,
    path: String,
    root: String,
    if_hash: Option<String>,
    create: bool,
    force: bool,
    parents: bool,
    durable: bool,
    mode: Option<String>,
    json: bool,
) -> Result<i32, String> {
    use std::io::Read as _;
    let mut content = Vec::new();
    std::io::stdin()
        .read_to_end(&mut content)
        .map_err(|e| format!("reading stdin: {e}"))?;
    let mode = parse_mode(mode.as_deref())?;

    let mut flags = 0u8;
    if parents {
        flags |= FS_WRITE_MKPARENTS;
    }
    if durable {
        flags |= FS_WRITE_DURABLE;
    }
    // Precondition: --create is create-exclusive (base 0); --if-hash is
    // CAS; otherwise (or --force) an unconditional overwrite, matching a
    // shell `>` redirect.
    let base = if create {
        0
    } else if let Some(h) = &if_hash {
        parse_hash(h)?
    } else {
        flags |= FS_WRITE_NO_CAS;
        0
    };
    if force {
        flags |= FS_WRITE_NO_CAS;
    }

    let (mut reader, mut writer) = transport.split();
    let mut fb = Vec::new();
    let sync_id = open_write_root(&mut reader, &mut writer, &mut fb, &client_abs(&root)).await?;
    let req = FsWrite {
        nonce: REQ_NONCE,
        sync_id,
        flags,
        base,
        mode,
        content_kind: FS_WRITE_CONTENT_FULL,
        path: escape_wire(&path),
        content,
    };
    if !write_frame(&mut writer, &msg_fs_write(&req)).await {
        return Err("connection closed".into());
    }
    let (status, hash, mtime) = await_fs_done(&mut reader, &mut fb, REQ_NONCE).await?;
    report_done(status, hash, mtime, json)
}

#[allow(clippy::too_many_arguments)]
async fn run_op(
    transport: Transport,
    root: String,
    op: u8,
    a: String,
    b: String,
    base: u128,
    mode: u32,
    flags: u8,
    json: bool,
) -> Result<i32, String> {
    let (mut reader, mut writer) = transport.split();
    let mut fb = Vec::new();
    let sync_id = open_write_root(&mut reader, &mut writer, &mut fb, &client_abs(&root)).await?;
    let req = FsOp {
        nonce: REQ_NONCE,
        sync_id,
        op,
        flags,
        base,
        mode,
        a: escape_wire(&a),
        b: if b.is_empty() {
            String::new()
        } else {
            escape_wire(&b)
        },
    };
    if !write_frame(&mut writer, &msg_fs_op(&req)).await {
        return Err("connection closed".into());
    }
    let (status, hash, mtime) = await_fs_done(&mut reader, &mut fb, REQ_NONCE).await?;
    report_done(status, hash, mtime, json)
}

pub async fn cmd_mkdir(
    transport: Transport,
    path: String,
    root: String,
    parents: bool,
    mode: Option<String>,
    json: bool,
) -> Result<i32, String> {
    let mode = parse_mode(mode.as_deref())?;
    let flags = if parents { FS_OP_MKPARENTS } else { 0 };
    run_op(
        transport,
        root,
        FS_OP_MKDIR,
        path,
        String::new(),
        0,
        mode,
        flags,
        json,
    )
    .await
}

pub async fn cmd_rm(
    transport: Transport,
    path: String,
    root: String,
    if_hash: Option<String>,
    json: bool,
) -> Result<i32, String> {
    // base 0 = unconditional remove; a hash = "remove iff unchanged".
    let base = match &if_hash {
        Some(h) => parse_hash(h)?,
        None => 0,
    };
    run_op(
        transport,
        root,
        FS_OP_REMOVE,
        path,
        String::new(),
        base,
        0,
        0,
        json,
    )
    .await
}

pub async fn cmd_mv(
    transport: Transport,
    from: String,
    to: String,
    root: String,
    parents: bool,
    json: bool,
) -> Result<i32, String> {
    let flags = if parents { FS_OP_MKPARENTS } else { 0 };
    run_op(transport, root, FS_OP_RENAME, from, to, 0, 0, flags, json).await
}
