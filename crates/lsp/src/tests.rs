//! Engine tests against a scripted in-process fake LSP server — the
//! quirk harness (docs/design/lsp.md "Server implementation"): quirk
//! handling is tested deterministically, not against whatever
//! rust-analyzer does today.

use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use blit_remote::lsp::*;
use serde_json::{Value, json};

use crate::attach::Attachment;
use crate::backend::Backend;
use crate::discovery::{MarkerGroup, RootPolicy, ServerSpec};
use crate::rpc;
use crate::{Budgets, Sink, testutil};

fn test_spec() -> ServerSpec {
    ServerSpec {
        id: "fake".into(),
        command: vec!["fake".into()],
        groups: vec![MarkerGroup {
            markers: &["marker"],
            policy: RootPolicy::Nearest,
        }],
        extensions: vec!["rs".into()],
        init: None,
        settings: Some(json!({ "answer": 42 })),
    }
}

fn test_budgets() -> Budgets {
    Budgets {
        query_timeout: Duration::from_secs(5),
        init_timeout: Duration::from_secs(5),
        // Short quiescence grace so wait_ready tests stay fast.
        ready_grace: Duration::from_millis(80),
        ..Budgets::default()
    }
}

fn tmp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blit-lsp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn collector() -> (Sink, Receiver<Vec<u8>>) {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    (Arc::new(move |msg| tx.send(msg).is_ok()), rx)
}

/// Wait for a message satisfying `pick`, discarding others.
fn wait_for<T>(rx: &Receiver<Vec<u8>>, mut pick: impl FnMut(&[u8]) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for message");
        let msg = rx.recv_timeout(left).expect("channel closed or timed out");
        if let Some(t) = pick(&msg) {
            return t;
        }
    }
}

/// The scripted fake server: handles the lifecycle and a fixed set of
/// query methods; forwards a copy of every received method name to
/// `seen`; sends the notifications/requests in `extra` right after
/// `initialized` arrives.
#[derive(Clone)]
struct FakeCfg {
    encoding: &'static str,
    /// `(json payloads)` sent after the `initialized` notification.
    after_init: Vec<Value>,
    seen: Option<Sender<String>>,
}

fn fake_server(
    cfg: FakeCfg,
) -> impl FnMut(BufReader<Box<dyn Read + Send>>, Box<dyn Write + Send>) + Clone + Send + 'static {
    move |mut reader, mut writer| {
        let cfg = cfg.clone();
        let mut next_req_id = 1000i64;
        while let Some(msg) = rpc::read_msg(&mut reader) {
            match msg {
                rpc::RpcMsg::Request { id, method, params } => {
                    if let Some(seen) = &cfg.seen {
                        let _ = seen.send(method.clone());
                    }
                    let reply = match method.as_str() {
                        "initialize" => rpc::response(
                            &id,
                            json!({
                                "capabilities": {
                                    "positionEncoding": cfg.encoding,
                                    "definitionProvider": true,
                                    "referencesProvider": true,
                                    "hoverProvider": true,
                                    "documentSymbolProvider": true,
                                    "workspaceSymbolProvider": true,
                                    "renameProvider": true,
                                },
                                "serverInfo": { "name": "fake" },
                            }),
                        ),
                        "shutdown" => rpc::response(&id, Value::Null),
                        "textDocument/definition" => {
                            let uri = params["textDocument"]["uri"].as_str().unwrap().to_string();
                            // One target on line 1 spanning the 'é' —
                            // characters 1..2 in UTF-16.
                            rpc::response(
                                &id,
                                json!([ { "uri": uri, "range": {
                                    "start": { "line": 1, "character": 1 },
                                    "end": { "line": 1, "character": 2 },
                                } } ]),
                            )
                        }
                        "textDocument/documentSymbol" => rpc::response(
                            &id,
                            json!([{
                                "name": "Outer",
                                "kind": 5,
                                "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 3, "character": 0 } },
                                "selectionRange": { "start": { "line": 0, "character": 0 },
                                                    "end": { "line": 0, "character": 5 } },
                                "children": [{
                                    "name": "inner",
                                    "kind": 12,
                                    "range": { "start": { "line": 1, "character": 0 },
                                               "end": { "line": 2, "character": 0 } },
                                    "selectionRange": { "start": { "line": 1, "character": 0 },
                                                        "end": { "line": 1, "character": 5 } },
                                }],
                            }]),
                        ),
                        "textDocument/rename" => {
                            let uri = params["textDocument"]["uri"].as_str().unwrap().to_string();
                            // UTF-16 units 2..4 are exactly the 𝄞
                            // character: bytes 3..7.
                            rpc::response(
                                &id,
                                json!({ "changes": { uri: [
                                    { "range": { "start": { "line": 1, "character": 2 },
                                                 "end": { "line": 1, "character": 4 } },
                                      "newText": "renamed" },
                                ] } }),
                            )
                        }
                        _ => rpc::error_response(&id, -32601, "unhandled in fake"),
                    };
                    let _ = rpc::write_msg(writer.as_mut(), &reply);
                }
                rpc::RpcMsg::Notification { method, .. } => {
                    if let Some(seen) = &cfg.seen {
                        let _ = seen.send(method.clone());
                    }
                    if method == "initialized" {
                        for payload in &cfg.after_init {
                            let mut payload = payload.clone();
                            if payload.get("id") == Some(&json!("FRESH")) {
                                next_req_id += 1;
                                payload["id"] = json!(next_req_id);
                            }
                            let _ = rpc::write_msg(writer.as_mut(), &payload);
                        }
                    }
                    if method == "exit" {
                        return;
                    }
                }
                rpc::RpcMsg::Response { .. } => {}
            }
        }
    }
}

fn start(tag: &str, cfg: FakeCfg) -> (PathBuf, Arc<Backend>) {
    let root = tmp_root(tag);
    (
        root.clone(),
        testutil::pipe_backend(test_spec(), root, test_budgets(), fake_server(cfg)),
    )
}

fn attach(root: &Path, backend: &Arc<Backend>, flags: u8, sink: Sink) -> Attachment {
    Attachment::start(
        1,
        root.to_path_buf(),
        vec![backend.clone()],
        vec![(test_spec(), root.to_path_buf())],
        flags,
        1,
        sink,
        &test_budgets(),
    )
}

#[test]
fn state_reaches_ready_with_caps() {
    let (root, backend) = start(
        "ready",
        FakeCfg {
            encoding: "utf-16",
            after_init: vec![],
            seen: None,
        },
    );
    let (sink, rx) = collector();
    let att = attach(&root, &backend, LSP_OPEN_WATCH, sink);
    let mut mirror = LspStateMirror::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "never reached READY");
        let msg = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        if msg.first() == Some(&S2C_LSP_STATE)
            && let Some(state_id) = mirror.apply_state(&msg)
        {
            att.ack(LSP_STREAM_STATE, state_id);
            let server = &mirror.servers[&1];
            assert_eq!(server.id, "fake");
            if server.phase == LSP_PHASE_READY {
                assert_eq!(server.caps & LSP_CAP_DEFINITION, LSP_CAP_DEFINITION);
                assert_eq!(server.caps & LSP_CAP_RENAME, LSP_CAP_RENAME);
                break;
            }
        }
    }
}

/// READY means quiescent, not merely initialized: an active
/// `$/progress` token holds the phase at INDEXING well past the grace
/// window, so `blit lsp wait` cannot return mid-warmup.
#[test]
fn active_progress_holds_off_ready() {
    let (_root, backend) = start(
        "hold",
        FakeCfg {
            encoding: "utf-16",
            after_init: vec![json!({
                "jsonrpc": "2.0",
                "method": "$/progress",
                "params": { "token": "warm", "value": {
                    "kind": "begin", "title": "indexing", "percentage": 5,
                } },
            })],
            seen: None,
        },
    );
    assert_holds_indexing(&backend);
}

/// The last progress `end` starts the grace clock; READY follows once
/// the session stays idle through it.
#[test]
fn progress_end_promotes_ready_after_grace() {
    let progress = |kind: Value| {
        json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": { "token": "warm", "value": kind },
        })
    };
    let (_root, backend) = start(
        "grace",
        FakeCfg {
            encoding: "utf-16",
            after_init: vec![
                progress(json!({ "kind": "begin", "title": "indexing" })),
                progress(json!({ "kind": "end" })),
            ],
            seen: None,
        },
    );
    wait_ready(&backend);
}

/// A server that reports quiescence explicitly (rust-analyzer's
/// experimental serverStatus) overrides the grace heuristic in both
/// directions: `quiescent:false` pins INDEXING past any idle window…
#[test]
fn server_status_nonquiescent_holds_indexing() {
    let (_root, backend) = start(
        "status-busy",
        FakeCfg {
            encoding: "utf-16",
            after_init: vec![json!({
                "jsonrpc": "2.0",
                "method": "experimental/serverStatus",
                "params": { "health": "ok", "quiescent": false },
            })],
            seen: None,
        },
    );
    assert_holds_indexing(&backend);
}

/// Wait for the warmup signal to land (phase INDEXING), then outlast
/// the grace window several times over and check it stuck.
fn assert_holds_indexing(backend: &Arc<Backend>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "never reached INDEXING");
        if backend.shared.info.lock().unwrap().phase == LSP_PHASE_INDEXING {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(test_budgets().ready_grace * 6);
    assert_eq!(
        backend.shared.info.lock().unwrap().phase,
        LSP_PHASE_INDEXING
    );
}

/// …and `quiescent:true` promotes to READY without waiting out the
/// grace window.
#[test]
fn server_status_quiescent_promotes_ready() {
    let status = |quiescent: bool| {
        json!({
            "jsonrpc": "2.0",
            "method": "experimental/serverStatus",
            "params": { "health": "ok", "quiescent": quiescent },
        })
    };
    let (_root, backend) = start(
        "status-ready",
        FakeCfg {
            encoding: "utf-16",
            after_init: vec![status(false), status(true)],
            seen: None,
        },
    );
    wait_ready(&backend);
}

#[test]
fn query_before_ready_answers_warming() {
    // A server that never answers initialize.
    let silent = |mut reader: BufReader<Box<dyn Read + Send>>, _writer: Box<dyn Write + Send>| {
        while rpc::read_msg(&mut reader).is_some() {}
    };
    let root = tmp_root("warming");
    std::fs::write(root.join("a.rs"), "fn main() {}\n").unwrap();
    let backend = testutil::pipe_backend(test_spec(), root.clone(), test_budgets(), silent);
    let (sink, rx) = collector();
    let att = attach(&root, &backend, 0, sink.clone());
    att.query(7, LSP_QUERY_DEFINITION, 0, 0, 0, "a.rs", "", sink);
    let (nonce, status) = wait_for(&rx, |msg| {
        parse_lsp_query_resp(msg).map(|r| (r.nonce, r.status))
    });
    assert_eq!((nonce, status), (7, LSP_STATUS_WARMING));
}

#[test]
fn definition_transcodes_utf16_to_bytes() {
    let (root, backend) = start(
        "def",
        FakeCfg {
            encoding: "utf-16",
            after_init: vec![],
            seen: None,
        },
    );
    // Line 1 is "aé𝄞b": UTF-16 char 1..2 covers é = bytes 1..3.
    std::fs::write(root.join("a.rs"), "x\naé𝄞b\n").unwrap();
    let (sink, rx) = collector();
    let att = attach(&root, &backend, 0, sink.clone());
    wait_ready(&backend);
    att.query(3, LSP_QUERY_DEFINITION, 0, 0, 0, "a.rs", "", sink);
    let (status, records) = wait_for(&rx, |msg| {
        parse_lsp_query_resp(msg)
            .filter(|r| r.nonce == 3)
            .map(|r| (r.status, r.records))
    });
    assert_eq!(status, LSP_STATUS_OK);
    let locations: Vec<_> = lsp_query_records(&records).collect();
    match &locations[..] {
        [
            LspQueryRecord::Location {
                line,
                col,
                end_col,
                path,
                hash,
                ..
            },
        ] => {
            assert_eq!((*line, *col, *end_col), (1, 1, 3));
            assert_eq!(*path, "a.rs");
            assert_ne!(*hash, LSP_HASH_NONE);
        }
        other => panic!("unexpected records: {other:?}"),
    }
}

#[test]
fn diagnostics_full_replay_reaches_late_joiner() {
    let root = tmp_root("diag");
    std::fs::write(root.join("a.rs"), "x\naé𝄞b\n").unwrap();
    let uri = crate::text::path_to_uri(&root.join("a.rs"));
    let (root2, backend) = {
        let cfg = FakeCfg {
            encoding: "utf-16",
            after_init: vec![json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": { "uri": uri, "diagnostics": [ {
                    "range": { "start": { "line": 1, "character": 1 },
                               "end": { "line": 1, "character": 2 } },
                    "severity": 1,
                    "code": "E1",
                    "message": "bad é",
                } ] },
            })],
            seen: None,
        };
        (
            root.clone(),
            testutil::pipe_backend(test_spec(), root.clone(), test_budgets(), fake_server(cfg)),
        )
    };
    let check = |att: &Attachment, rx: &Receiver<Vec<u8>>| {
        let mut mirror = LspDiagMirror::new();
        loop {
            let msg = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("no diag update");
            if msg.first() != Some(&S2C_LSP_DIAG) {
                continue;
            }
            let update_id = mirror.apply_diag(&msg).unwrap();
            att.ack(LSP_STREAM_DIAG, update_id);
            if let Some(file) = mirror.files.get("a.rs") {
                let d = &file.diags[0];
                assert_eq!((d.line, d.col, d.end_col), (1, 1, 3));
                assert_eq!(d.msg, "bad é");
                assert_ne!(file.hash, LSP_HASH_NONE);
                return;
            }
        }
    };
    let (sink1, rx1) = collector();
    let att1 = attach(&root2, &backend, LSP_OPEN_DIAGS, sink1);
    check(&att1, &rx1);
    // A late joiner gets the same state from the cache replay, without
    // the server republishing.
    let (sink2, rx2) = collector();
    let att2 = attach(&root2, &backend, LSP_OPEN_DIAGS, sink2);
    check(&att2, &rx2);
}

#[test]
fn rename_returns_edit_plan_and_applyedit_is_refused() {
    let (seen_tx, _seen_rx) = std::sync::mpsc::channel();
    let (root, backend) = start(
        "rename",
        FakeCfg {
            encoding: "utf-16",
            after_init: vec![json!({
                "jsonrpc": "2.0",
                "id": "FRESH",
                "method": "workspace/applyEdit",
                "params": { "edit": { "changes": {} } },
            })],
            seen: Some(seen_tx),
        },
    );
    std::fs::write(root.join("a.rs"), "x\naé𝄞b\n").unwrap();
    let (sink, rx) = collector();
    let att = attach(&root, &backend, LSP_OPEN_WATCH, sink.clone());
    wait_ready(&backend);
    att.query(9, LSP_QUERY_RENAME, 0, 1, 3, "a.rs", "renamed", sink);
    let (status, records) = wait_for(&rx, |msg| {
        parse_lsp_query_resp(msg)
            .filter(|r| r.nonce == 9)
            .map(|r| (r.status, r.records))
    });
    assert_eq!(status, LSP_STATUS_OK);
    let edits: Vec<_> = lsp_query_records(&records).collect();
    match &edits[..] {
        [
            LspQueryRecord::Edit {
                line,
                col,
                end_col,
                new_text,
                path,
                ..
            },
        ] => {
            assert_eq!((*line, *col, *end_col), (1, 3, 7));
            assert_eq!(*new_text, "renamed");
            assert_eq!(*path, "a.rs");
        }
        other => panic!("unexpected records: {other:?}"),
    }
    // The applyEdit sent after initialized was refused and counted.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "refused_edits never surfaced");
        if backend.shared.info.lock().unwrap().refused_edits >= 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn doc_symbols_flatten_with_depth() {
    let (root, backend) = start(
        "sym",
        FakeCfg {
            encoding: "utf-16",
            after_init: vec![],
            seen: None,
        },
    );
    std::fs::write(root.join("a.rs"), "struct O;\nfn i() {}\n\n\n").unwrap();
    let (sink, rx) = collector();
    let att = attach(&root, &backend, 0, sink.clone());
    wait_ready(&backend);
    att.query(5, LSP_QUERY_DOC_SYMBOLS, 0, 0, 0, "a.rs", "", sink);
    let records = wait_for(&rx, |msg| {
        parse_lsp_query_resp(msg)
            .filter(|r| r.nonce == 5)
            .map(|r| r.records)
    });
    let symbols: Vec<_> = lsp_query_records(&records).collect();
    match &symbols[..] {
        [
            LspQueryRecord::Symbol {
                name: outer,
                depth: 0,
                sym_kind: 5,
                ..
            },
            LspQueryRecord::Symbol {
                name: inner,
                depth: 1,
                sym_kind: 12,
                ..
            },
        ] => {
            assert_eq!((*outer, *inner), ("Outer", "inner"));
        }
        other => panic!("unexpected records: {other:?}"),
    }
}

#[test]
fn child_exit_restarts_with_backoff() {
    // First session dies right after initialize; the respawned one
    // lives.
    let root = tmp_root("restart");
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempts2 = attempts.clone();
    let serve = move |mut reader: BufReader<Box<dyn Read + Send>>,
                      mut writer: Box<dyn Write + Send>| {
        let n = attempts2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        while let Some(msg) = rpc::read_msg(&mut reader) {
            if let rpc::RpcMsg::Request { id, method, .. } = msg
                && method == "initialize"
            {
                if n == 0 {
                    return; // die mid-handshake
                }
                let _ = rpc::write_msg(
                    writer.as_mut(),
                    &rpc::response(&id, json!({ "capabilities": {} })),
                );
            }
        }
    };
    let backend = testutil::pipe_backend(test_spec(), root, test_budgets(), serve);
    wait_ready(&backend);
    assert!(attempts.load(std::sync::atomic::Ordering::SeqCst) >= 2);
}

/// A queued or in-flight query must always get its one response — even
/// when the backend is stopped underneath it — or the connection's
/// nonce would leak forever (docs/design/lsp.md: one response per
/// nonce in every outcome).
#[test]
fn stop_answers_pending_query() {
    let (root, backend) = start(
        "stopq",
        FakeCfg {
            encoding: "utf-16",
            after_init: vec![],
            seen: None,
        },
    );
    std::fs::write(root.join("a.rs"), "fn x() {}\n").unwrap();
    wait_ready(&backend);

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let sink: Sink = Arc::new(move |msg| tx.send(msg).is_ok());
    backend.send(crate::backend::Cmd::Query {
        sub: 1,
        nonce: 7,
        kind: LSP_QUERY_HOVER,
        flags: 0,
        line: 0,
        col: 0,
        path: Some(root.join("a.rs")),
        arg: String::new(),
        wire_root: root.clone(),
        sink,
    });
    backend.send(crate::backend::Cmd::Stop);

    let msg = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("pending query answered on stop");
    let nonce = parse_lsp_query_resp(&msg).unwrap().nonce;
    assert_eq!(nonce, 7);

    // Once stopped the backend is terminally gone, and further sends are
    // refused so the attachment can respawn on a later query.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !backend.is_gone() {
        assert!(Instant::now() < deadline, "backend never went gone");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!backend.send(crate::backend::Cmd::Stop));
}

/// A query for a capability the backend does not advertise must answer
/// NOT_FOUND, never a bare OTHER "error" — routing checks the capability
/// before dispatching, so an unsupported request is never sent (the
/// nixd-workspace-symbols case from the field).
#[test]
fn query_without_capability_is_not_found() {
    let root = tmp_root("nocap");
    std::fs::write(root.join("a.rs"), "fn x() {}\n").unwrap();
    // A server advertising only hover — no workspace/document symbols,
    // no definition.
    let serve = |mut reader: BufReader<Box<dyn Read + Send>>, mut writer: Box<dyn Write + Send>| {
        while let Some(msg) = rpc::read_msg(&mut reader) {
            if let rpc::RpcMsg::Request { id, method, .. } = msg
                && method == "initialize"
            {
                let _ = rpc::write_msg(
                    writer.as_mut(),
                    &rpc::response(&id, json!({ "capabilities": { "hoverProvider": true } })),
                );
            }
        }
    };
    let backend = testutil::pipe_backend(test_spec(), root.clone(), test_budgets(), serve);
    wait_ready(&backend);
    let att = attach(&root, &backend, 0, dummy_sink());

    for (nonce, kind, path) in [
        (7, LSP_QUERY_WS_SYMBOLS, ""),
        (8, LSP_QUERY_DEFINITION, "a.rs"),
    ] {
        let (sink, rx) = collector();
        att.query(nonce, kind, 0, 0, 0, path, "", sink);
        let (n, status) = wait_for(&rx, |m| {
            parse_lsp_query_resp(m).map(|r| (r.nonce, r.status))
        });
        assert_eq!(
            (n, status),
            (nonce, LSP_STATUS_NOT_FOUND),
            "kind {kind} must be NOT_FOUND, not error"
        );
    }
}

fn dummy_sink() -> Sink {
    Arc::new(|_| true)
}

fn wait_ready(backend: &Arc<Backend>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "backend never became ready");
        if backend.shared.info.lock().unwrap().phase == LSP_PHASE_READY {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
