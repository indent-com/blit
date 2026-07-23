//! `blit lsp` — language intelligence client (docs/design/lsp.md).
//!
//! Thin by design: attach to a workspace, send queries, apply pushed
//! state, print. Positions are 1-based `path:line:col` on the command
//! line (the `--vimgrep` convention) and 0-based byte columns on the
//! wire; the conversion lives here and nowhere else. Every invocation
//! attaches to warm daemon-owned backends, so first calls may answer
//! `WARMING` (exit 2, retryable) while later ones are instant.

use std::path::Path;

use crate::fs::handshake;
use crate::transport::{Transport, read_message, write_frame};
use blit_remote::S2C_QUIT;
use blit_remote::lsp::{
    FEATURE_LSP, LSP_CLOSED_CLIENT_REQUEST, LSP_DIAG_FULL, LSP_OPEN_DIAGS, LSP_OPEN_WATCH,
    LSP_PHASE_FAILED, LSP_PHASE_INDEXING, LSP_PHASE_INITIALIZING, LSP_PHASE_READY,
    LSP_PHASE_SPAWNING, LSP_PROGRESS_UNKNOWN, LSP_QUERY_DOC_SYMBOLS, LSP_QUERY_REFERENCES,
    LSP_QUERY_WS_SYMBOLS, LSP_REFS_INCLUDE_DECLARATION, LSP_RESP_TRUNCATED, LSP_STATUS_NOT_FOUND,
    LSP_STATUS_OK, LSP_STATUS_WARMING, LSP_STREAM_DIAG, LSP_STREAM_STATE, LspDiagMirror,
    LspQueryRecord, LspQueryRequest, LspServersRecord, LspStateMirror, S2C_LSP_CLOSED,
    S2C_LSP_DIAG, S2C_LSP_OPENED, S2C_LSP_QUERY, S2C_LSP_SERVERS, S2C_LSP_STATE, S2C_LSP_STOPPED,
    lsp_query_records, lsp_servers_records, lsp_status_text, msg_lsp_ack, msg_lsp_open,
    msg_lsp_query, msg_lsp_servers, msg_lsp_stop, parse_lsp_closed, parse_lsp_diag,
    parse_lsp_opened, parse_lsp_query_resp, parse_lsp_servers_resp, parse_lsp_state,
    parse_lsp_stopped,
};
use tokio::io::{AsyncRead, AsyncWrite};

const OPEN_NONCE: u16 = 1;
const REQ_NONCE: u16 = 2;

/// Parse `PATH:LINE:COL` (1-based) into the wire's 0-based line and
/// byte column.
fn parse_spec(spec: &str) -> Result<(String, u32, u32), String> {
    let err = || format!("expected PATH:LINE:COL, got {spec}");
    let (rest, col) = spec.rsplit_once(':').ok_or_else(err)?;
    let (path, line) = rest.rsplit_once(':').ok_or_else(err)?;
    let line: u32 = line.parse().map_err(|_| err())?;
    let col: u32 = col.parse().map_err(|_| err())?;
    if path.is_empty() || line == 0 || col == 0 {
        return Err(err());
    }
    Ok((client_abs(path), line - 1, col - 1))
}

/// Resolve a path against the **client's** working directory so the
/// server's cwd is irrelevant. The blit server is a long-lived daemon
/// whose cwd is wherever it was first auto-started (transport.rs), and
/// it resolves `LSP_OPEN.path` / query paths against its own cwd — so a
/// relative `--root .` or `main.rs` from another repo would point at the
/// daemon's directory, not the user's. Absolutizing here (lexically, no
/// filesystem access) makes `blit lsp` work from any directory. An
/// already-absolute path is returned unchanged.
fn client_abs(path: &str) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| std::path::PathBuf::from(path))
        .to_string_lossy()
        .into_owned()
}

fn phase_text(phase: u8) -> &'static str {
    match phase {
        LSP_PHASE_SPAWNING => "spawning",
        LSP_PHASE_INITIALIZING => "initializing",
        LSP_PHASE_INDEXING => "indexing",
        LSP_PHASE_READY => "ready",
        LSP_PHASE_FAILED => "failed",
        _ => "unknown",
    }
}

fn symbol_kind_text(kind: u8) -> &'static str {
    const NAMES: [&str; 27] = [
        "unknown",
        "file",
        "module",
        "namespace",
        "package",
        "class",
        "method",
        "property",
        "field",
        "constructor",
        "enum",
        "interface",
        "function",
        "variable",
        "constant",
        "string",
        "number",
        "boolean",
        "array",
        "object",
        "key",
        "null",
        "enum-member",
        "struct",
        "event",
        "operator",
        "type-parameter",
    ];
    NAMES.get(kind as usize).copied().unwrap_or("unknown")
}

fn severity_text(severity: u8) -> &'static str {
    match severity {
        1 => "error",
        2 => "warning",
        3 => "info",
        4 => "hint",
        _ => "note",
    }
}

struct Session<R, W> {
    reader: R,
    writer: W,
    fragment_buf: Vec<u8>,
    lsp_id: u16,
}

/// Handshake, attach, and wait for `LSP_OPENED`.
async fn open_lsp<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    mut writer: W,
    path: &str,
    flags: u8,
) -> Result<(Session<R, W>, String), String> {
    let mut fragment_buf: Vec<u8> = Vec::new();
    let features = handshake(&mut reader, &mut fragment_buf).await?;
    if features & FEATURE_LSP == 0 {
        return Err(
            "server does not support language intelligence (upgrade blit on the remote)".into(),
        );
    }
    if !write_frame(&mut writer, &msg_lsp_open(OPEN_NONCE, flags, 0, path)).await {
        return Err("connection closed".into());
    }
    loop {
        let Some(data) = read_message(&mut reader, &mut fragment_buf).await else {
            return Err("connection closed".into());
        };
        if data.first() != Some(&S2C_LSP_OPENED) {
            if data.first() == Some(&S2C_QUIT) {
                return Err("server is shutting down".into());
            }
            continue;
        }
        let opened = parse_lsp_opened(&data).ok_or("malformed LSP_OPENED")?;
        if opened.nonce != OPEN_NONCE {
            continue;
        }
        if opened.status != LSP_STATUS_OK {
            return Err(format!(
                "open failed: {} ({})",
                if opened.detail.is_empty() {
                    lsp_status_text(opened.status)
                } else {
                    opened.detail
                },
                lsp_status_text(opened.status)
            ));
        }
        let root = opened.root.to_string();
        return Ok((
            Session {
                reader,
                writer,
                fragment_buf,
                lsp_id: opened.lsp_id,
            },
            root,
        ));
    }
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Session<R, W> {
    async fn recv(&mut self) -> Result<Vec<u8>, String> {
        loop {
            let Some(data) = read_message(&mut self.reader, &mut self.fragment_buf).await else {
                return Err("connection closed".into());
            };
            if data.is_empty() {
                continue;
            }
            if data[0] == S2C_QUIT {
                return Err("server is shutting down".into());
            }
            if data[0] == S2C_LSP_CLOSED
                && let Some((lsp_id, reason)) = parse_lsp_closed(&data)
                && lsp_id == self.lsp_id
                && reason != LSP_CLOSED_CLIENT_REQUEST
            {
                return Err(format!("attachment closed by server (reason {reason})"));
            }
            return Ok(data);
        }
    }

    /// One query, one response.
    async fn query(
        &mut self,
        kind: u8,
        flags: u8,
        line: u32,
        col: u32,
        path: &str,
        arg: &str,
    ) -> Result<(u8, u8, String, Vec<u8>), String> {
        let msg = msg_lsp_query(&LspQueryRequest {
            nonce: REQ_NONCE,
            lsp_id: self.lsp_id,
            kind,
            flags,
            line,
            col,
            path,
            arg,
        });
        if !write_frame(&mut self.writer, &msg).await {
            return Err("connection closed".into());
        }
        loop {
            let data = self.recv().await?;
            if data[0] != S2C_LSP_QUERY {
                continue;
            }
            let Some(resp) = parse_lsp_query_resp(&data) else {
                return Err("malformed query response from server".into());
            };
            if resp.nonce != REQ_NONCE {
                continue;
            }
            return Ok((resp.status, resp.flags, resp.detail, resp.records));
        }
    }
}

/// Move a session's reader into a task forwarding whole messages over
/// a channel. `tokio::select!` over a channel receiver is cancel-safe;
/// selecting directly over a frame read is not — a timer firing
/// mid-frame would corrupt the stream.
fn spawn_reader(
    session: Session<Box<dyn AsyncRead + Unpin + Send>, Box<dyn AsyncWrite + Unpin + Send>>,
) -> (
    Box<dyn AsyncWrite + Unpin + Send>,
    u16,
    tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let Session {
        mut reader,
        writer,
        mut fragment_buf,
        lsp_id,
    } = session;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(data) = read_message(&mut reader, &mut fragment_buf).await {
            if tx.send(data).is_err() {
                return;
            }
        }
    });
    (writer, lsp_id, rx)
}

/// The `Session::recv` screening for channel-delivered messages.
fn screen(data: &[u8], lsp_id: u16) -> Result<(), String> {
    if data.first() == Some(&S2C_QUIT) {
        return Err("server is shutting down".into());
    }
    if data.first() == Some(&S2C_LSP_CLOSED)
        && let Some((id, reason)) = parse_lsp_closed(data)
        && id == lsp_id
        && reason != LSP_CLOSED_CLIENT_REQUEST
    {
        return Err(format!("attachment closed by server (reason {reason})"));
    }
    Ok(())
}

/// Shared refusal handling: `WARMING` gets the retry hint and exit 2;
/// other non-OK statuses report and exit 2.
/// `Ok(true)` = proceed, `Ok(false)` = no result (exit 1). `detail`
/// carries the server's own reason where it has one (e.g. the upstream
/// error message on a failure), so a failed query reads as the real
/// cause, not a bare "error".
fn check_status(status: u8, detail: &str) -> Result<bool, String> {
    match status {
        LSP_STATUS_OK => Ok(true),
        LSP_STATUS_NOT_FOUND => Ok(false),
        LSP_STATUS_WARMING => Err(format!(
            "language server warming up — retry, or run `blit lsp wait` ({})",
            lsp_status_text(status)
        )),
        _ if !detail.is_empty() => Err(format!("query failed: {detail}")),
        _ => Err(format!("query failed: {}", lsp_status_text(status))),
    }
}

fn location_json(record: &LspQueryRecord<'_>) -> Option<String> {
    match record {
        LspQueryRecord::Location {
            line,
            col,
            end_line,
            end_col,
            path,
            ..
        } => Some(
            serde_json::json!({
                "type": "location",
                "path": path,
                "line": line + 1,
                "col": col + 1,
                "endLine": end_line + 1,
                "endCol": end_col + 1,
            })
            .to_string(),
        ),
        _ => None,
    }
}

/// `blit lsp def|refs|hover|rename` — position queries.
pub async fn cmd_position(
    transport: Transport,
    root: String,
    kind: u8,
    spec: String,
    arg: String,
    include_declaration: bool,
    json: bool,
) -> Result<i32, String> {
    let (path, line, col) = parse_spec(&spec)?;
    let (reader, writer) = transport.split();
    let (mut session, _) = open_lsp(reader, writer, &client_abs(&root), 0).await?;
    let flags = if kind == LSP_QUERY_REFERENCES && include_declaration {
        LSP_REFS_INCLUDE_DECLARATION
    } else {
        0
    };
    let (status, resp_flags, detail, records) =
        session.query(kind, flags, line, col, &path, &arg).await?;
    if !check_status(status, &detail)? {
        return Ok(1);
    }
    let mut found = 0;
    for record in lsp_query_records(&records) {
        match &record {
            LspQueryRecord::Location {
                line, col, path, ..
            } => {
                if json {
                    println!("{}", location_json(&record).unwrap());
                } else {
                    println!("{path}:{}:{}", line + 1, col + 1);
                }
                found += 1;
            }
            LspQueryRecord::Markup { format, text } => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "type": "hover",
                            "format": if *format == 1 { "markdown" } else { "plaintext" },
                            "text": text,
                        })
                    );
                } else {
                    println!("{text}");
                }
                found += 1;
            }
            LspQueryRecord::Edit {
                line,
                col,
                end_line,
                end_col,
                new_text,
                path,
                ..
            } => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "type": "edit",
                            "path": path,
                            "line": line + 1,
                            "col": col + 1,
                            "endLine": end_line + 1,
                            "endCol": end_col + 1,
                            "newText": new_text,
                        })
                    );
                } else {
                    println!(
                        "{path}:{}:{}-{}:{} -> {new_text}",
                        line + 1,
                        col + 1,
                        end_line + 1,
                        end_col + 1
                    );
                }
                found += 1;
            }
            LspQueryRecord::Symbol { .. } => {}
        }
    }
    if resp_flags & LSP_RESP_TRUNCATED != 0 && !json {
        eprintln!(
            "… truncated at {found} results (raise BLIT_LSP_ENTRIES_MAX / BLIT_LSP_BYTES_MAX)"
        );
    }
    Ok(if found == 0 { 1 } else { 0 })
}

/// `blit lsp symbols` — document outline or workspace search.
pub async fn cmd_symbols(
    transport: Transport,
    root: String,
    query: Option<String>,
    file: Option<String>,
    json: bool,
) -> Result<i32, String> {
    let (reader, writer) = transport.split();
    let (mut session, _) = open_lsp(reader, writer, &client_abs(&root), 0).await?;
    // Empty symbols are surprising (unlike a grep-style def/refs miss),
    // and the common cause is that no server providing symbols is
    // running for this workspace — point the user at `blit lsp list`.
    let hint_empty = || {
        if !json {
            eprintln!("no symbols — see running servers with `blit lsp list`");
        }
    };
    let (status, resp_flags, detail, records) = match file {
        Some(file) => {
            session
                .query(LSP_QUERY_DOC_SYMBOLS, 0, 0, 0, &client_abs(&file), "")
                .await?
        }
        None => {
            session
                .query(
                    LSP_QUERY_WS_SYMBOLS,
                    0,
                    0,
                    0,
                    "",
                    query.as_deref().unwrap_or(""),
                )
                .await?
        }
    };
    if !check_status(status, &detail)? {
        hint_empty();
        return Ok(1);
    }
    let mut found = 0;
    for record in lsp_query_records(&records) {
        if let LspQueryRecord::Symbol {
            sym_kind,
            depth,
            line,
            col,
            name,
            path,
            ..
        } = record
        {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "type": "symbol",
                        "name": name,
                        "kind": symbol_kind_text(sym_kind),
                        "depth": depth,
                        "path": path,
                        "line": line + 1,
                        "col": col + 1,
                    })
                );
            } else {
                let indent = "  ".repeat(depth as usize);
                println!(
                    "{indent}{} {name} — {path}:{}:{}",
                    symbol_kind_text(sym_kind),
                    line + 1,
                    col + 1
                );
            }
            found += 1;
        }
    }
    if found == 0 {
        hint_empty();
        return Ok(1);
    }
    if resp_flags & LSP_RESP_TRUNCATED != 0 && !json {
        eprintln!(
            "… truncated at {found} symbols (raise BLIT_LSP_ENTRIES_MAX / \
             BLIT_LSP_BYTES_MAX, or narrow the query)"
        );
    }
    Ok(0)
}

fn print_diags(mirror: &LspDiagMirror, filter: Option<&str>, json: bool) -> (String, usize) {
    let mut out = String::new();
    let mut count = 0;
    for (path, file) in &mirror.files {
        if let Some(filter) = filter
            && path != filter
            && !path.starts_with(&format!("{filter}/"))
        {
            continue;
        }
        for d in &file.diags {
            count += 1;
            if json {
                out.push_str(
                    &serde_json::json!({
                        "type": "diagnostic",
                        "path": path,
                        "line": d.line + 1,
                        "col": d.col + 1,
                        "severity": severity_text(d.severity),
                        "code": d.code,
                        "source": d.source,
                        "message": d.msg,
                    })
                    .to_string(),
                );
                out.push('\n');
            } else {
                let code = if d.code.is_empty() {
                    String::new()
                } else {
                    format!("[{}] ", d.code)
                };
                out.push_str(&format!(
                    "{path}:{}:{}: {}: {code}{}\n",
                    d.line + 1,
                    d.col + 1,
                    severity_text(d.severity),
                    d.msg
                ));
            }
        }
    }
    (out, count)
}

/// Explain an empty diagnostic set from the current server state, so
/// "nothing" isn't confused with "clean". Three cases: some server is
/// still warming (the usual cause on a big project — say so and point at
/// `blit lsp wait`); every server is ready but some diagnose only open
/// documents (tsserver/pyright/clangd cover only opened/edited files);
/// or every server is ready and whole-project — the workspace really is
/// clean.
fn empty_diagnostics_reason(state: &LspStateMirror) -> String {
    if state.servers.is_empty() {
        return "no language servers running for this workspace".into();
    }
    let mut warming: Vec<&str> = state
        .servers
        .values()
        .filter(|s| !matches!(s.phase, LSP_PHASE_READY | LSP_PHASE_FAILED))
        .map(|s| s.id.as_str())
        .collect();
    warming.sort_unstable();
    if !warming.is_empty() {
        return format!(
            "no diagnostics yet — still indexing: {} (run `blit lsp wait`, then retry)",
            warming.join(", ")
        );
    }
    // Every server is ready. Open-doc-only servers only diagnose files
    // that have been opened; a whole-project server that found nothing
    // means the workspace is clean.
    let mut open_doc_only: Vec<&str> = state
        .servers
        .values()
        .filter(|s| matches!(s.id.as_str(), "typescript-language-server" | "pyright" | "clangd"))
        .map(|s| s.id.as_str())
        .collect();
    open_doc_only.sort_unstable();
    if open_doc_only.is_empty() {
        "no diagnostics — the workspace is clean".into()
    } else {
        format!(
            "no diagnostics — {} report only files you've opened or edited; \
             name one (`blit lsp diag PATH`) or edit it to check it",
            open_doc_only.join(", ")
        )
    }
}

/// `blit lsp diagnostics` — the workspace's current diagnostic state:
/// print once, `--wait` for backends to settle first, or `--watch` the
/// stream.
pub async fn cmd_diagnostics(
    transport: Transport,
    root: String,
    path: Option<String>,
    watch: bool,
    wait: bool,
    json: bool,
) -> Result<i32, String> {
    let (reader, writer) = transport.split();
    let (session, workdir) = open_lsp(
        reader,
        writer,
        &client_abs(&root),
        LSP_OPEN_WATCH | LSP_OPEN_DIAGS,
    )
    .await?;
    if !json {
        eprintln!("workspace {workdir}");
    }
    let (mut writer, lsp_id, mut rx) = spawn_reader(session);
    // Naming a file is an open-set admission signal (docs/design/lsp.md
    // "Document truth"): open-doc-only servers (clangd, tsserver,
    // pyright) diagnose only opened documents, so once the backends are
    // ready, nudge the file open with a throwaway outline query.
    let nudge = path
        .as_deref()
        .filter(|p| Path::new(p).extension().is_some())
        .map(str::to_string);
    let mut nudged = false;
    let mut state = LspStateMirror::new();
    let mut diags = LspDiagMirror::new();
    let mut got_full = false;
    let mut last: Option<String> = None;
    // `--wait` quiescence is a CLI heuristic by design (docs/design/
    // lsp.md): every backend ready, then one quiet settle window.
    let mut quiet_since = tokio::time::Instant::now();
    loop {
        let settle = tokio::time::sleep_until(quiet_since + std::time::Duration::from_millis(700));
        let data = tokio::select! {
            data = rx.recv() => data.ok_or("connection closed")?,
            _ = settle, if got_full && !watch => {
                let ready = state.servers.values().all(|s| s.phase == LSP_PHASE_READY);
                if !wait || ready {
                    let (out, count) = print_diags(&diags, path.as_deref(), json);
                    print!("{out}");
                    if count == 0 && diags.files.is_empty() && !json {
                        eprintln!("{}", empty_diagnostics_reason(&state));
                    }
                    return Ok(if count == 0 { 0 } else { 1 });
                }
                // Waiting for a not-yet-ready backend. A failed one will
                // never settle — don't hang forever.
                if let Some(server) = state
                    .servers
                    .values()
                    .find(|s| s.phase == LSP_PHASE_FAILED)
                {
                    return Err(format!("{} failed: {}", server.id, server.msg));
                }
                // Rebase the settle timer so the next tick is a real
                // ~700ms poll, not an already-expired deadline (which
                // would busy-spin at 100% CPU).
                quiet_since = tokio::time::Instant::now();
                continue;
            }
        };
        if data.is_empty() {
            continue;
        }
        screen(&data, lsp_id)?;
        match data[0] {
            S2C_LSP_STATE => {
                if parse_lsp_state(&data).is_some_and(|(id, ..)| id == lsp_id)
                    && let Some(state_id) = state.apply_state(&data)
                {
                    let ack = msg_lsp_ack(lsp_id, LSP_STREAM_STATE, state_id);
                    if !write_frame(&mut writer, &ack).await {
                        return Err("connection closed".into());
                    }
                    if !nudged
                        && let Some(file) = &nudge
                        && !state.servers.is_empty()
                        && state.servers.values().all(|s| s.phase == LSP_PHASE_READY)
                    {
                        nudged = true;
                        let msg = msg_lsp_query(&LspQueryRequest {
                            nonce: REQ_NONCE,
                            lsp_id,
                            kind: LSP_QUERY_DOC_SYMBOLS,
                            flags: 0,
                            line: 0,
                            col: 0,
                            path: file,
                            arg: "",
                        });
                        if !write_frame(&mut writer, &msg).await {
                            return Err("connection closed".into());
                        }
                        // Give the resulting publish a settle window.
                        quiet_since = tokio::time::Instant::now();
                    }
                }
            }
            S2C_LSP_DIAG => {
                let Some((diag_id, _, flags, _)) = parse_lsp_diag(&data) else {
                    continue;
                };
                if diag_id != lsp_id {
                    continue;
                }
                let Some(update_id) = diags.apply_diag(&data) else {
                    return Err("malformed diagnostics from server".into());
                };
                let ack = msg_lsp_ack(lsp_id, LSP_STREAM_DIAG, update_id);
                if !write_frame(&mut writer, &ack).await {
                    return Err("connection closed".into());
                }
                if flags & LSP_DIAG_FULL != 0 {
                    got_full = true;
                }
                quiet_since = tokio::time::Instant::now();
                if watch {
                    let (rendered, _) = print_diags(&diags, path.as_deref(), json);
                    if last.as_deref() != Some(rendered.as_str()) {
                        if !json && last.is_some() {
                            println!("—");
                        }
                        print!("{rendered}");
                        if last.is_none() && !json {
                            eprintln!("watching (ctrl-c to stop)…");
                        }
                        last = Some(rendered);
                    }
                }
            }
            _ => {}
        }
    }
}

/// `blit lsp wait` — block until every backend of the workspace is
/// ready (exit 0) or failed (exit 2).
pub async fn cmd_wait(
    transport: Transport,
    root: String,
    timeout_secs: u64,
) -> Result<i32, String> {
    let (reader, writer) = transport.split();
    let (session, workdir) = open_lsp(reader, writer, &client_abs(&root), LSP_OPEN_WATCH).await?;
    eprintln!("workspace {workdir}");
    let (mut writer, lsp_id, mut rx) = spawn_reader(session);
    let mut state = LspStateMirror::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut shown = String::new();
    loop {
        let data = tokio::select! {
            data = rx.recv() => data.ok_or("connection closed")?,
            _ = tokio::time::sleep_until(deadline) => {
                return Err(format!("timed out after {timeout_secs}s"));
            }
        };
        if data.is_empty() {
            continue;
        }
        screen(&data, lsp_id)?;
        if data[0] != S2C_LSP_STATE {
            continue;
        }
        if !parse_lsp_state(&data).is_some_and(|(id, ..)| id == lsp_id) {
            continue;
        }
        let Some(state_id) = state.apply_state(&data) else {
            return Err("malformed state from server".into());
        };
        let ack = msg_lsp_ack(lsp_id, LSP_STREAM_STATE, state_id);
        if !write_frame(&mut writer, &ack).await {
            return Err("connection closed".into());
        }
        let mut all_ready = !state.servers.is_empty();
        for server in state.servers.values() {
            let progress = if server.progress_pct == LSP_PROGRESS_UNKNOWN {
                String::new()
            } else {
                format!(" {}%", server.progress_pct)
            };
            let line = format!("{} {}{progress}", server.id, phase_text(server.phase));
            if line != shown {
                eprintln!("{line}");
                shown = line;
            }
            match server.phase {
                LSP_PHASE_READY => {}
                LSP_PHASE_FAILED => {
                    return Err(format!("{} failed: {}", server.id, server.msg));
                }
                _ => all_ready = false,
            }
        }
        if all_ready {
            return Ok(0);
        }
    }
}

/// `blit lsp list` — every live backend, daemon-wide.
pub async fn cmd_list(transport: Transport, json: bool) -> Result<i32, String> {
    let (mut reader, mut writer) = transport.split();
    let mut fragment_buf: Vec<u8> = Vec::new();
    let features = handshake(&mut reader, &mut fragment_buf).await?;
    if features & FEATURE_LSP == 0 {
        return Err(
            "server does not support language intelligence (upgrade blit on the remote)".into(),
        );
    }
    if !write_frame(&mut writer, &msg_lsp_servers(REQ_NONCE)).await {
        return Err("connection closed".into());
    }
    loop {
        let Some(data) = read_message(&mut reader, &mut fragment_buf).await else {
            return Err("connection closed".into());
        };
        if data.first() != Some(&S2C_LSP_SERVERS) {
            continue;
        }
        let Some((nonce, _, _, records)) = parse_lsp_servers_resp(&data) else {
            return Err("malformed LSP_SERVERS from server".into());
        };
        if nonce != REQ_NONCE {
            continue;
        }
        let mut n = 0;
        for record in lsp_servers_records(&records) {
            let LspServersRecord::Server {
                server_ref,
                phase,
                progress_pct,
                rss,
                id,
                msg,
                root,
                ..
            } = record;
            n += 1;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "type": "server",
                        "ref": server_ref,
                        "id": id,
                        "phase": phase_text(phase),
                        "progress": if progress_pct == LSP_PROGRESS_UNKNOWN {
                            None
                        } else {
                            Some(progress_pct)
                        },
                        "rssBytes": rss,
                        "root": root,
                        "message": msg,
                    })
                );
            } else {
                let rss = if rss == 0 {
                    String::new()
                } else {
                    format!(" {:.1} GiB", rss as f64 / (1 << 30) as f64)
                };
                let progress = if progress_pct == LSP_PROGRESS_UNKNOWN {
                    String::new()
                } else {
                    format!(" {progress_pct}%")
                };
                println!(
                    "{server_ref} {id} {}{progress}{rss} {root}",
                    phase_text(phase)
                );
            }
        }
        if n == 0 && !json {
            eprintln!("no language servers running");
        }
        return Ok(0);
    }
}

/// `blit lsp stop` — shut one backend down by ref.
pub async fn cmd_stop(transport: Transport, server_ref: u16) -> Result<i32, String> {
    let (mut reader, mut writer) = transport.split();
    let mut fragment_buf: Vec<u8> = Vec::new();
    let features = handshake(&mut reader, &mut fragment_buf).await?;
    if features & FEATURE_LSP == 0 {
        return Err(
            "server does not support language intelligence (upgrade blit on the remote)".into(),
        );
    }
    if !write_frame(&mut writer, &msg_lsp_stop(REQ_NONCE, server_ref)).await {
        return Err("connection closed".into());
    }
    loop {
        let Some(data) = read_message(&mut reader, &mut fragment_buf).await else {
            return Err("connection closed".into());
        };
        if data.first() != Some(&S2C_LSP_STOPPED) {
            continue;
        }
        let Some((nonce, status)) = parse_lsp_stopped(&data) else {
            return Err("malformed LSP_STOPPED from server".into());
        };
        if nonce != REQ_NONCE {
            continue;
        }
        if status != LSP_STATUS_OK {
            return Err(format!("stop failed: {}", lsp_status_text(status)));
        }
        return Ok(0);
    }
}

pub use blit_remote::lsp::{
    LSP_QUERY_DEFINITION as KIND_DEF, LSP_QUERY_HOVER as KIND_HOVER,
    LSP_QUERY_REFERENCES as KIND_REFS, LSP_QUERY_RENAME as KIND_RENAME,
};

#[cfg(test)]
mod tests {
    use super::{client_abs, parse_spec};
    use std::path::Path;

    #[test]
    fn spec_parsing() {
        // The path is absolutized against the client cwd; line/col
        // convert to 0-based.
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            parse_spec("src/main.rs:10:4"),
            Ok((cwd.join("src/main.rs").to_string_lossy().into_owned(), 9, 3))
        );
        // A path with colons still parses right-to-left.
        assert_eq!(
            parse_spec("a:b.rs:1:1"),
            Ok((cwd.join("a:b.rs").to_string_lossy().into_owned(), 0, 0))
        );
        // An absolute path is passed through unchanged.
        assert_eq!(parse_spec("/tmp/x.rs:2:3"), Ok(("/tmp/x.rs".into(), 1, 2)));
        assert!(parse_spec("main.rs:10").is_err());
        assert!(parse_spec("main.rs:0:1").is_err());
        assert!(parse_spec(":10:4").is_err());
    }

    #[test]
    fn client_abs_absolutizes_relative_paths() {
        let cwd = std::env::current_dir().unwrap();
        assert!(Path::new(&client_abs(".")).is_absolute());
        assert_eq!(client_abs("foo/bar"), cwd.join("foo/bar").to_string_lossy());
        // An absolute path is returned unchanged.
        assert_eq!(client_abs("/etc/hosts"), "/etc/hosts");
    }
}
