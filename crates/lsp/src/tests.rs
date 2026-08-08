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
            markers: vec!["marker".into()],
            policy: RootPolicy::Nearest,
        }],
        extensions: vec!["rs".into()],
        needs_open_doc: false,
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
                                    "completionProvider": { "triggerCharacters": ["."] },
                                    "signatureHelpProvider": { "triggerCharacters": ["("] },
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
                        "textDocument/completion" => rpc::response(
                            &id,
                            // Out of sortText order on purpose (zz before
                            // aa), with a UTF-16 edit range over the é on
                            // line 1 (units 1..2 = bytes 1..3) and one
                            // snippet item without a textEdit.
                            json!({ "isIncomplete": true, "items": [
                                { "label": "zz_last", "kind": 6, "sortText": "b",
                                  "detail": "u32",
                                  "textEdit": { "range": {
                                      "start": { "line": 1, "character": 1 },
                                      "end": { "line": 1, "character": 2 } },
                                    "newText": "zz_last" } },
                                { "label": "aa_first", "kind": 3, "sortText": "a",
                                  "preselect": true,
                                  "insertText": "aa_first(${1:x})",
                                  "insertTextFormat": 2,
                                  "tags": [1] },
                            ] }),
                        ),
                        "textDocument/signatureHelp" => rpc::response(
                            &id,
                            // The active parameter's label is UTF-16
                            // offsets 5..8 into "f(a: 𝄞x)" — 𝄞 is two
                            // units / four bytes, so bytes 5..10.
                            json!({
                                "activeSignature": 1,
                                "activeParameter": 0,
                                "signatures": [
                                    { "label": "f()" },
                                    { "label": "f(a: 𝄞x)",
                                      "documentation": { "kind": "markdown",
                                                         "value": "docs" },
                                      "parameters": [
                                          { "label": [5, 8] },
                                      ] },
                                ],
                            }),
                        ),
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

/// blit advertises `definition.linkSupport`, so rust-analyzer and gopls
/// answer with `LocationLink[]` (`targetUri` + `targetSelectionRange`,
/// with `targetRange` the fallback) rather than plain `Location[]`. That
/// is the branch real servers hit, so cover both the selection-range
/// jump target and the fallback when it is absent.
#[test]
fn location_link_uses_selection_range_with_target_fallback() {
    let root = tmp_root("loclink");
    // Line 0 is "x"; line 1 is "aé𝄞b" (é = bytes 1..3 in UTF-8).
    std::fs::write(root.join("a.rs"), "x\naé𝄞b\n").unwrap();
    let serve = |mut reader: BufReader<Box<dyn Read + Send>>, mut writer: Box<dyn Write + Send>| {
        while let Some(msg) = rpc::read_msg(&mut reader) {
            match msg {
                rpc::RpcMsg::Request { id, method, params } => {
                    let reply = match method.as_str() {
                        "initialize" => rpc::response(
                            &id,
                            json!({ "capabilities": {
                                "positionEncoding": "utf-16",
                                "definitionProvider": true,
                                "referencesProvider": true,
                            } }),
                        ),
                        "shutdown" => rpc::response(&id, Value::Null),
                        // targetSelectionRange (UTF-16 1..2 = é) is the
                        // jump target; targetRange spans the whole item.
                        "textDocument/definition" => {
                            let uri = params["textDocument"]["uri"].as_str().unwrap().to_string();
                            rpc::response(
                                &id,
                                json!([ {
                                    "targetUri": uri,
                                    "targetRange": { "start": { "line": 0, "character": 0 },
                                                     "end": { "line": 3, "character": 0 } },
                                    "targetSelectionRange": { "start": { "line": 1, "character": 1 },
                                                              "end": { "line": 1, "character": 2 } },
                                } ]),
                            )
                        }
                        // No targetSelectionRange: targetRange (line 0
                        // "x", UTF-16 0..1) is the jump target.
                        "textDocument/references" => {
                            let uri = params["textDocument"]["uri"].as_str().unwrap().to_string();
                            rpc::response(
                                &id,
                                json!([ {
                                    "targetUri": uri,
                                    "targetRange": { "start": { "line": 0, "character": 0 },
                                                     "end": { "line": 0, "character": 1 } },
                                } ]),
                            )
                        }
                        _ => rpc::error_response(&id, -32601, "unhandled in fake"),
                    };
                    let _ = rpc::write_msg(writer.as_mut(), &reply);
                }
                rpc::RpcMsg::Notification { method, .. } => {
                    if method == "exit" {
                        return;
                    }
                }
                rpc::RpcMsg::Response { .. } => {}
            }
        }
    };
    let backend = testutil::pipe_backend(test_spec(), root.clone(), test_budgets(), serve);
    wait_ready(&backend);
    let att = attach(&root, &backend, 0, dummy_sink());

    // Definition: the selection range transcodes é to bytes 1..3.
    let (sink, rx) = collector();
    att.query(3, LSP_QUERY_DEFINITION, 0, 0, 0, "a.rs", "", sink);
    let records = wait_for(&rx, |msg| {
        parse_lsp_query_resp(msg)
            .filter(|r| r.nonce == 3)
            .map(|r| r.records)
    });
    match &lsp_query_records(&records).collect::<Vec<_>>()[..] {
        [
            LspQueryRecord::Location {
                line,
                col,
                end_line,
                end_col,
                path,
                ..
            },
        ] => {
            assert_eq!((*line, *col, *end_line, *end_col), (1, 1, 1, 3));
            assert_eq!(*path, "a.rs");
        }
        other => panic!("unexpected definition records: {other:?}"),
    }

    // References: the targetRange fallback covers "x" = bytes 0..1.
    let (sink, rx) = collector();
    att.query(4, LSP_QUERY_REFERENCES, 0, 0, 0, "a.rs", "", sink);
    let records = wait_for(&rx, |msg| {
        parse_lsp_query_resp(msg)
            .filter(|r| r.nonce == 4)
            .map(|r| r.records)
    });
    match &lsp_query_records(&records).collect::<Vec<_>>()[..] {
        [
            LspQueryRecord::Location {
                line,
                col,
                end_line,
                end_col,
                path,
                ..
            },
        ] => {
            assert_eq!((*line, *col, *end_line, *end_col), (0, 0, 0, 1));
            assert_eq!(*path, "a.rs");
        }
        other => panic!("unexpected references records: {other:?}"),
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

/// A frozen (lz4 cold) cache entry is subscriber-indistinguishable
/// from a live one: a late joiner's FULL replay decodes it, and the
/// next publish for the path lands as an ordinary live entry.
#[test]
fn frozen_diag_entry_replays_and_republishes() {
    let root = tmp_root("frozen");
    std::fs::write(root.join("a.rs"), "fn x() {}\n").unwrap();
    let uri = crate::text::path_to_uri(&root.join("a.rs"));
    let publish = move |msg: &str| {
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": uri, "diagnostics": [ {
                "range": { "start": { "line": 0, "character": 0 },
                           "end": { "line": 0, "character": 1 } },
                "severity": 1,
                "message": msg,
            } ] },
        })
    };
    // A fake server that publishes "v1" after init and "v2" when a
    // hover query arrives.
    let serve = move |mut reader: BufReader<Box<dyn Read + Send>>,
                      mut writer: Box<dyn Write + Send>| {
        while let Some(msg) = rpc::read_msg(&mut reader) {
            match msg {
                rpc::RpcMsg::Request { id, method, .. } => {
                    let reply = match method.as_str() {
                        "initialize" => rpc::response(
                            &id,
                            json!({
                                "capabilities": {
                                    "positionEncoding": "utf-16",
                                    "hoverProvider": true,
                                },
                                "serverInfo": { "name": "fake" },
                            }),
                        ),
                        _ => rpc::response(&id, Value::Null),
                    };
                    let _ = rpc::write_msg(writer.as_mut(), &reply);
                    if method == "textDocument/hover" {
                        let _ = rpc::write_msg(writer.as_mut(), &publish("v2"));
                    }
                }
                rpc::RpcMsg::Notification { method, .. } => {
                    if method == "initialized" {
                        let _ = rpc::write_msg(writer.as_mut(), &publish("v1"));
                    }
                    if method == "exit" {
                        return;
                    }
                }
                rpc::RpcMsg::Response { .. } => {}
            }
        }
    };
    let backend = testutil::pipe_backend(test_spec(), root.clone(), test_budgets(), serve);
    // Wait for v1 to land, then freeze the entry in place.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "v1 publish never landed");
        if backend
            .shared
            .diags
            .lock()
            .unwrap()
            .contains_key(&root.join("a.rs"))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    {
        let mut diags = backend.shared.diags.lock().unwrap();
        crate::backend::freeze_cold_diags(&mut diags, Duration::ZERO);
        assert!(matches!(
            diags[&root.join("a.rs")].diags,
            crate::backend::Diags::Cold(_)
        ));
    }
    // A late joiner's FULL replay decodes the frozen entry.
    let (sink, rx) = collector();
    let att = attach(&root, &backend, LSP_OPEN_DIAGS, sink.clone());
    let mut mirror = LspDiagMirror::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no FULL replay of the frozen entry");
        let msg = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        if msg.first() != Some(&S2C_LSP_DIAG) {
            continue;
        }
        let update_id = mirror.apply_diag(&msg).unwrap();
        att.ack(LSP_STREAM_DIAG, update_id);
        if let Some(file) = mirror.files.get("a.rs") {
            assert_eq!(file.diags[0].msg, "v1");
            break;
        }
    }
    // A publish against the frozen entry: hover makes the server
    // republish, the cache entry goes live again, and the subscriber
    // sees the incremental.
    att.query(1, LSP_QUERY_HOVER, 0, 0, 0, "a.rs", "", sink);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "v2 publish never landed");
        let msg = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        if msg.first() != Some(&S2C_LSP_DIAG) {
            continue;
        }
        let update_id = mirror.apply_diag(&msg).unwrap();
        att.ack(LSP_STREAM_DIAG, update_id);
        if mirror.files["a.rs"].diags[0].msg == "v2" {
            break;
        }
    }
    let diags = backend.shared.diags.lock().unwrap();
    let a = &diags[&root.join("a.rs")];
    assert!(matches!(a.diags, crate::backend::Diags::Live(_)));
    assert_eq!(a.diags()[0].msg, "v2");
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

/// A document opened before a crash must be re-`didOpen`ed once the
/// backend comes back — even when the *first* respawn also dies during
/// its handshake, before the open set is ever repopulated. The reopen
/// list must survive that second respawn, not be clobbered by the
/// meanwhile-emptied open set.
#[test]
fn deferred_didopen_survives_a_respawn_that_dies_in_handshake() {
    use std::sync::atomic::{AtomicUsize, Ordering as O};
    let root = tmp_root("reopen");
    std::fs::write(root.join("a.rs"), "fn x() {}\n").unwrap();
    let uri = crate::text::path_to_uri(&root.join("a.rs"));

    let (open_tx, open_rx) = std::sync::mpsc::channel::<String>();
    let attempts = Arc::new(AtomicUsize::new(0));
    let serve = move |mut reader: BufReader<Box<dyn Read + Send>>,
                      mut writer: Box<dyn Write + Send>| {
        let n = attempts.fetch_add(1, O::SeqCst);
        // Session 2 dies mid-handshake: it never answers `initialize`,
        // so it never repopulates the open set.
        if n == 1 {
            return;
        }
        while let Some(msg) = rpc::read_msg(&mut reader) {
            match msg {
                rpc::RpcMsg::Request { id, method, .. } => {
                    match method.as_str() {
                        "initialize" => {
                            let _ = rpc::write_msg(
                                writer.as_mut(),
                                &rpc::response(
                                    &id,
                                    json!({ "capabilities": { "hoverProvider": true } }),
                                ),
                            );
                        }
                        "shutdown" => {
                            let _ =
                                rpc::write_msg(writer.as_mut(), &rpc::response(&id, Value::Null));
                        }
                        // Session 1 dies right after the query has opened
                        // the document, so a.rs is left in the open set.
                        "textDocument/hover" if n == 0 => return,
                        _ => {
                            let _ = rpc::write_msg(
                                writer.as_mut(),
                                &rpc::error_response(&id, -32601, "unhandled"),
                            );
                        }
                    }
                }
                rpc::RpcMsg::Notification { method, params } => {
                    // Only the recovered session's replay is observed.
                    if method == "textDocument/didOpen" && n >= 2 {
                        let sent = params["textDocument"]["uri"].as_str().unwrap_or_default();
                        let _ = open_tx.send(sent.to_string());
                    }
                    if method == "exit" {
                        return;
                    }
                }
                rpc::RpcMsg::Response { .. } => {}
            }
        }
    };
    let backend = testutil::pipe_backend(test_spec(), root.clone(), test_budgets(), serve);
    wait_ready(&backend);

    // Open a.rs via a query, then let session 1 die.
    let (sink, _rx) = collector();
    let att = attach(&root, &backend, 0, sink.clone());
    att.query(1, LSP_QUERY_HOVER, 0, 0, 0, "a.rs", "", sink);

    // The third session (after the mid-handshake death of the second)
    // must re-open a.rs from the preserved reopen list.
    let reopened = open_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("a.rs was never re-opened after the double crash");
    assert_eq!(reopened, uri);
    drop(att);
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

/// A `needs_open_doc` backend (tsserver's "No Project" quirk) must have
/// a document opened before `workspace/symbol`, or the query fails; blit
/// opens a representative file from the root first.
#[test]
fn ws_symbols_opens_a_project_doc_when_needed() {
    use std::sync::atomic::{AtomicBool, Ordering as O};
    let root = tmp_root("wsproj");
    std::fs::write(root.join("lib.rs"), "fn thing() {}\n").unwrap();
    let opened = Arc::new(AtomicBool::new(false));
    let opened2 = opened.clone();
    let serve = move |mut reader: BufReader<Box<dyn Read + Send>>,
                      mut writer: Box<dyn Write + Send>| {
        while let Some(msg) = rpc::read_msg(&mut reader) {
            match msg {
                rpc::RpcMsg::Request { id, method, .. } => {
                    let reply = match method.as_str() {
                        "initialize" => rpc::response(
                            &id,
                            json!({ "capabilities": { "workspaceSymbolProvider": true } }),
                        ),
                        "shutdown" => rpc::response(&id, Value::Null),
                        "workspace/symbol" if opened2.load(O::Relaxed) => rpc::response(
                            &id,
                            json!([{
                                "name": "thing", "kind": 12,
                                "location": { "uri": "file:///x/lib.rs", "range": {
                                    "start": { "line": 0, "character": 3 },
                                    "end": { "line": 0, "character": 8 } } }
                            }]),
                        ),
                        // No project until a document is open.
                        "workspace/symbol" => rpc::error_response(&id, -32000, "No Project"),
                        _ => rpc::error_response(&id, -32601, "unhandled"),
                    };
                    let _ = rpc::write_msg(writer.as_mut(), &reply);
                }
                rpc::RpcMsg::Notification { method, .. } => {
                    if method == "textDocument/didOpen" {
                        opened2.store(true, O::Relaxed);
                    }
                    if method == "exit" {
                        return;
                    }
                }
                rpc::RpcMsg::Response { .. } => {}
            }
        }
    };
    let mut spec = test_spec();
    spec.needs_open_doc = true;
    let backend = testutil::pipe_backend(spec, root.clone(), test_budgets(), serve);
    wait_ready(&backend);
    let (sink, rx) = collector();
    let att = attach(&root, &backend, 0, sink.clone());
    att.query(5, LSP_QUERY_WS_SYMBOLS, 0, 0, 0, "", "thing", sink);
    let records = wait_for(&rx, |m| {
        parse_lsp_query_resp(m)
            .filter(|r| r.nonce == 5)
            .map(|r| r.records)
    });
    let syms: Vec<_> = lsp_query_records(&records).collect();
    assert_eq!(
        syms.len(),
        1,
        "ws-symbols should succeed once a project doc is opened"
    );
}

/// `didChangeWatchedFiles` must relay a creation as `Created` (type 1),
/// a modification as `Changed` (2), and a removal as `Deleted` (3) — not
/// collapse everything to Changed/Deleted by `exists()` alone, or a
/// server that adds files only on `Created` (gopls) misses new files.
#[test]
fn watched_files_carry_the_change_type() {
    let root = tmp_root("watched");
    // Two files that exist before the backend starts, so the real
    // watcher stays quiet and the injected hints drive the test
    // deterministically across platforms.
    std::fs::write(root.join("created.rs"), "fn a() {}\n").unwrap();
    std::fs::write(root.join("changed.rs"), "fn b() {}\n").unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    let serve = move |mut reader: BufReader<Box<dyn Read + Send>>,
                      mut writer: Box<dyn Write + Send>| {
        while let Some(msg) = rpc::read_msg(&mut reader) {
            match msg {
                rpc::RpcMsg::Request { id, method, .. } => {
                    let reply = match method.as_str() {
                        "initialize" => rpc::response(&id, json!({ "capabilities": {} })),
                        "shutdown" => rpc::response(&id, Value::Null),
                        _ => rpc::error_response(&id, -32601, "unhandled"),
                    };
                    let _ = rpc::write_msg(writer.as_mut(), &reply);
                }
                rpc::RpcMsg::Notification { method, params } => {
                    if method == "workspace/didChangeWatchedFiles" {
                        let _ = tx.send(params);
                    }
                    if method == "exit" {
                        return;
                    }
                }
                rpc::RpcMsg::Response { .. } => {}
            }
        }
    };
    let backend = testutil::pipe_backend(test_spec(), root.clone(), test_budgets(), serve);
    wait_ready(&backend);

    let gone = root.join("gone.rs"); // never created → Deleted
    backend.send(crate::backend::Cmd::Dirty(vec![
        (root.join("created.rs"), true),
        (root.join("changed.rs"), false),
        (gone, false),
    ]));

    // Collect changes until all three files are seen.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut types: std::collections::HashMap<String, i64> = Default::default();
    while types.len() < 3 {
        assert!(
            Instant::now() < deadline,
            "watched-files event never arrived"
        );
        let params = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("no didChangeWatchedFiles");
        for change in params["changes"].as_array().into_iter().flatten() {
            let uri = change["uri"].as_str().unwrap_or_default();
            let name = uri.rsplit('/').next().unwrap_or_default().to_string();
            types.insert(name, change["type"].as_i64().unwrap_or(0));
        }
    }
    assert_eq!(types.get("created.rs"), Some(&1), "creation → Created");
    assert_eq!(types.get("changed.rs"), Some(&2), "modification → Changed");
    assert_eq!(types.get("gone.rs"), Some(&3), "missing path → Deleted");
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

/// The shared root watcher must drop VCS metadata and dependency/build
/// caches (`UNWATCHED_DIRS`) so a `cargo build` storm never reaches
/// `Cmd::Dirty` or `didChangeWatchedFiles` — but must NOT drop the wider
/// `SKIP_DIRS` set that `ensure_project_doc` avoids. Those two lists
/// answer different questions: skipping `dist/` when *picking* a
/// representative project file is right, skipping it when *watching* is a
/// correctness bug, because plenty of projects keep real sources there.
#[test]
fn watcher_filter_drops_only_never_source_subtrees() {
    let root = Path::new("/w");
    let keep = crate::backend::watched_path;
    assert!(keep(root, Path::new("/w/src/main.rs")));
    assert!(keep(root, Path::new("/w/Cargo.toml")));
    assert!(!keep(root, Path::new("/w/target/debug/build/x.d")));
    assert!(!keep(root, Path::new("/w/web/node_modules/p/index.js")));
    assert!(!keep(root, Path::new("/w/.git/index.lock")));
    assert!(!keep(root, Path::new("/w/.venv/lib/x.py")));
    assert!(!keep(root, Path::new("/w/.direnv/bin/x")));
    // Watched despite being in SKIP_DIRS: a picker avoids these, a
    // watcher must not, or an external edit there never refreshes.
    for build_ish in ["dist", "build", "out", "vendor"] {
        assert!(
            keep(root, &root.join(build_ish).join("app.ts")),
            "{build_ish}/ must still be watched"
        );
    }
    // Paths outside the root pass through (the old .git-only filter
    // kept them too).
    assert!(keep(root, Path::new("/elsewhere/x.rs")));
    // The stat-free filter drops files merely *named* like a skip dir.
    assert!(!keep(root, Path::new("/w/src/target")));
}

/// An empty publish for a path with no cached entry must be a complete
/// no-op — no disk read, no tombstone, no seq bump, no subscriber ping
/// — and a repeated clear with an unchanged hash must not re-insert.
#[test]
fn empty_publishes_skip_tombstones_and_pings() {
    let root = tmp_root("emptypub");
    std::fs::write(root.join("a.rs"), "fn x() {}\n").unwrap();
    std::fs::write(root.join("b.rs"), "fn y() {}\n").unwrap();
    let publish = |name: &str, diags: Value| {
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": crate::text::path_to_uri(&root.join(name)),
                "diagnostics": diags,
            },
        })
    };
    let diag = json!([{
        "range": { "start": { "line": 0, "character": 0 },
                   "end": { "line": 0, "character": 1 } },
        "severity": 1,
        "message": "bad",
    }]);
    let backend = testutil::pipe_backend(
        test_spec(),
        root.clone(),
        test_budgets(),
        fake_server(FakeCfg {
            encoding: "utf-16",
            after_init: vec![
                // Clear for a path never diagnosed: skipped entirely.
                publish("never.rs", json!([])),
                // Real entry, then its clear (tombstone), then a
                // duplicate clear: the duplicate is skipped.
                publish("a.rs", diag.clone()),
                publish("a.rs", json!([])),
                publish("a.rs", json!([])),
                // Sentinel proving everything above was processed.
                publish("b.rs", diag),
            ],
            seen: None,
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "sentinel publish never landed");
        if backend
            .shared
            .diags
            .lock()
            .unwrap()
            .contains_key(&root.join("b.rs"))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let diags = backend.shared.diags.lock().unwrap();
    // never.rs left no tombstone.
    assert!(!diags.contains_key(&root.join("never.rs")));
    // a.rs holds one tombstone (seq 2), not two.
    let a = &diags[&root.join("a.rs")];
    assert!(a.is_empty());
    assert_eq!(a.seq, 2);
    // Seqs: a.rs diag (1), a.rs clear (2), b.rs diag (3) — the skipped
    // publishes never bumped the counter.
    assert_eq!(
        backend
            .shared
            .diag_seq
            .load(std::sync::atomic::Ordering::Relaxed),
        3
    );
}

#[test]
fn completion_translates_sorts_and_flags() {
    let (root, backend) = start(
        "completion",
        FakeCfg {
            encoding: "utf-16",
            after_init: vec![],
            seen: None,
        },
    );
    std::fs::write(root.join("a.rs"), "x\naé𝄞b\n").unwrap();
    let (sink, rx) = collector();
    let att = attach(&root, &backend, 0, sink.clone());
    wait_ready(&backend);
    att.query(3, LSP_QUERY_COMPLETION, 0, 1, 1, "a.rs", "", sink);
    let (status, flags, records) = wait_for(&rx, |msg| {
        parse_lsp_query_resp(msg)
            .filter(|r| r.nonce == 3)
            .map(|r| (r.status, r.flags, r.records))
    });
    assert_eq!(status, LSP_STATUS_OK);
    assert_ne!(
        flags & LSP_RESP_INCOMPLETE,
        0,
        "isIncomplete → INCOMPLETE flag"
    );
    let recs: Vec<_> = lsp_query_records(&records).collect();
    match &recs[..] {
        [
            LspQueryRecord::Completion {
                label: l1,
                flags: f1,
                insert: i1,
                item_kind: k1,
                ..
            },
            LspQueryRecord::Completion {
                label: l2,
                insert: i2,
                detail,
                line,
                col,
                end_col,
                ..
            },
        ] => {
            // sortText order, not arrival order: "a" before "b".
            assert_eq!(*l1, "aa_first");
            assert_ne!(f1 & LSP_COMPLETION_SNIPPET, 0);
            assert_ne!(f1 & LSP_COMPLETION_PRESELECT, 0);
            assert_ne!(f1 & LSP_COMPLETION_DEPRECATED, 0, "tag 1 → DEPRECATED");
            assert_eq!(*i1, "aa_first(${1:x})");
            assert_eq!(*k1, 3);
            assert_eq!(*l2, "zz_last");
            // textEdit.newText == label → empty wire insert.
            assert_eq!(*i2, "");
            assert_eq!(*detail, "u32");
            // UTF-16 units 1..2 on "aé𝄞b" are the é: bytes 1..3.
            assert_eq!((*line, *col, *end_col), (1, 1, 3));
        }
        other => panic!("unexpected records: {other:?}"),
    }
}

#[test]
fn signature_help_active_first_with_param_bytes() {
    let (root, backend) = start(
        "sig",
        FakeCfg {
            encoding: "utf-16",
            after_init: vec![],
            seen: None,
        },
    );
    std::fs::write(root.join("a.rs"), "x\n").unwrap();
    let (sink, rx) = collector();
    let att = attach(&root, &backend, 0, sink.clone());
    wait_ready(&backend);
    att.query(4, LSP_QUERY_SIGNATURE, 0, 0, 0, "a.rs", "", sink);
    let (status, records) = wait_for(&rx, |msg| {
        parse_lsp_query_resp(msg)
            .filter(|r| r.nonce == 4)
            .map(|r| (r.status, r.records))
    });
    assert_eq!(status, LSP_STATUS_OK);
    let recs: Vec<_> = lsp_query_records(&records).collect();
    match &recs[..] {
        [
            LspQueryRecord::Signature {
                flags: f1,
                active_param,
                param_start,
                param_end,
                label: l1,
                doc,
            },
            LspQueryRecord::Signature {
                flags: f2,
                label: l2,
                param_start: ps2,
                param_end: pe2,
                ..
            },
        ] => {
            // activeSignature 1 is emitted first, flagged ACTIVE.
            assert_ne!(f1 & LSP_SIGNATURE_ACTIVE, 0);
            assert_eq!(*l1, "f(a: 𝄞x)");
            assert_eq!(*active_param, 0);
            // UTF-16 offsets 5..8 into the label → bytes 5..10.
            assert_eq!((*param_start, *param_end), (5, 10));
            assert_eq!(*doc, "docs");
            assert_eq!(f2 & LSP_SIGNATURE_ACTIVE, 0);
            assert_eq!(*l2, "f()");
            assert_eq!((*ps2, *pe2), (0, 0));
        }
        other => panic!("unexpected records: {other:?}"),
    }
}

/// A minimal server that records every notification (method, params),
/// for observing document sync during buffer-overlay tests.
fn doc_recording_server(
    docs: Sender<(String, Value)>,
) -> impl FnMut(BufReader<Box<dyn Read + Send>>, Box<dyn Write + Send>) + Clone + Send + 'static {
    move |mut reader, mut writer| {
        let docs = docs.clone();
        while let Some(msg) = rpc::read_msg(&mut reader) {
            match msg {
                rpc::RpcMsg::Request { id, method, .. } => {
                    let reply = match method.as_str() {
                        "initialize" => rpc::response(
                            &id,
                            json!({ "capabilities": {
                                "positionEncoding": "utf-8",
                                "definitionProvider": true,
                            } }),
                        ),
                        "shutdown" => rpc::response(&id, Value::Null),
                        _ => rpc::error_response(&id, -32601, "unhandled in fake"),
                    };
                    let _ = rpc::write_msg(writer.as_mut(), &reply);
                }
                rpc::RpcMsg::Notification { method, params } => {
                    if method == "exit" {
                        return;
                    }
                    let _ = docs.send((method, params));
                }
                rpc::RpcMsg::Response { .. } => {}
            }
        }
    }
}

fn wait_doc<T>(
    rx: &Receiver<(String, Value)>,
    mut pick: impl FnMut(&str, &Value) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for doc event");
        let (method, params) = rx.recv_timeout(left).expect("channel closed or timed out");
        if let Some(t) = pick(&method, &params) {
            return t;
        }
    }
}

#[test]
fn buffer_overlay_overrides_disk_until_release() {
    let root = tmp_root("overlay");
    std::fs::write(root.join("a.rs"), "disk v1\n").unwrap();
    let (doc_tx, doc_rx) = std::sync::mpsc::channel();
    let backend = testutil::pipe_backend(
        test_spec(),
        root.clone(),
        test_budgets(),
        doc_recording_server(doc_tx),
    );
    let (sink, _rx) = collector();
    let att = attach(&root, &backend, 0, sink);
    wait_ready(&backend);
    // The overlay write opens the doc with buffer bytes, not disk.
    att.buffer("a.rs", Some(b"buffer v1\n".to_vec()));
    let text = wait_doc(&doc_rx, |m, p| {
        (m == "textDocument/didOpen")
            .then(|| p["textDocument"]["text"].as_str().unwrap().to_string())
    });
    assert_eq!(text, "buffer v1\n");
    // A disk change while overlaid: watched-files events still flow,
    // but no content didChange (the overlay is the byte source).
    std::fs::write(root.join("a.rs"), "disk v2\n").unwrap();
    backend.send(crate::backend::Cmd::Dirty(vec![(root.join("a.rs"), false)]));
    wait_doc(&doc_rx, |m, _| {
        (m == "workspace/didChangeWatchedFiles").then_some(())
    });
    // Versions are engine-minted and sequential, and a content didChange
    // is written before the watched-files notification — so the next
    // didChange being version 2 with the buffer text proves the disk
    // flush did not slip one in.
    att.buffer("a.rs", Some(b"buffer v2\n".to_vec()));
    let change = |m: &str, p: &Value| {
        (m == "textDocument/didChange").then(|| {
            (
                p["textDocument"]["version"].as_i64().unwrap(),
                p["contentChanges"][0]["text"].as_str().unwrap().to_string(),
            )
        })
    };
    let (version, text) = wait_doc(&doc_rx, change);
    assert_eq!((version, text.as_str()), (2, "buffer v2\n"));
    // Release reverts to disk truth with one didChange.
    att.buffer("a.rs", None);
    let (version, text) = wait_doc(&doc_rx, change);
    assert_eq!((version, text.as_str()), (3, "disk v2\n"));
}

#[test]
fn first_empty_overlay_write_still_syncs() {
    let root = tmp_root("overlay-empty");
    std::fs::write(root.join("a.rs"), "disk\n").unwrap();
    let (doc_tx, doc_rx) = std::sync::mpsc::channel();
    let backend = testutil::pipe_backend(
        test_spec(),
        root.clone(),
        test_budgets(),
        doc_recording_server(doc_tx),
    );
    let (sink, rx) = collector();
    let att = attach(&root, &backend, 0, sink.clone());
    wait_ready(&backend);
    // Open the doc from disk via a query first.
    att.query(9, LSP_QUERY_DEFINITION, 0, 0, 0, "a.rs", "", sink);
    let text = wait_doc(&doc_rx, |m, p| {
        (m == "textDocument/didOpen")
            .then(|| p["textDocument"]["text"].as_str().unwrap().to_string())
    });
    assert_eq!(text, "disk\n");
    wait_for(&rx, |msg| {
        parse_lsp_query_resp(msg)
            .filter(|r| r.nonce == 9)
            .map(|_| ())
    });
    // The FIRST overlay write happens to be an empty buffer: it must
    // still sync — the open doc holds disk content, and a fresh
    // overlay is never "unchanged".
    att.buffer("a.rs", Some(Vec::new()));
    let (version, text) = wait_doc(&doc_rx, |m, p| {
        (m == "textDocument/didChange").then(|| {
            (
                p["textDocument"]["version"].as_i64().unwrap(),
                p["contentChanges"][0]["text"].as_str().unwrap().to_string(),
            )
        })
    });
    assert_eq!((version, text.as_str()), (2, ""));
}

/// A settled disk write to a handled file is a save. Check-on-save
/// servers (rust-analyzer's flycheck, gopls) rerun their external checker
/// only on didSave — `didChangeWatchedFiles` refreshes their VFS but
/// publishes nothing — so without this their diagnostics stay frozen at
/// whatever the startup check produced, for the life of the backend.
#[test]
fn disk_write_notifies_did_save() {
    let root = tmp_root("didsave");
    std::fs::write(root.join("a.rs"), "v1\n").unwrap();
    let (doc_tx, doc_rx) = std::sync::mpsc::channel();
    let backend = testutil::pipe_backend(
        test_spec(),
        root.clone(),
        test_budgets(),
        doc_recording_server(doc_tx),
    );
    let (sink, _rx) = collector();
    let _att = attach(&root, &backend, 0, sink);
    wait_ready(&backend);
    std::fs::write(root.join("a.rs"), "v2\n").unwrap();
    backend.send(crate::backend::Cmd::Dirty(vec![(root.join("a.rs"), false)]));
    let uri = wait_doc(&doc_rx, |m, p| {
        (m == "textDocument/didSave")
            .then(|| p["textDocument"]["uri"].as_str().unwrap().to_string())
    });
    assert!(uri.ends_with("/a.rs"), "unexpected didSave uri: {uri}");
}

/// The editor's case: an overlaid document still gets didSave when its
/// bytes land on disk. The overlay suppresses content sync (the buffer is
/// the byte source), but the external checker reads disk, and Ctrl+S is
/// precisely when it should rerun.
#[test]
fn overlaid_doc_still_notifies_did_save() {
    let root = tmp_root("didsave-overlay");
    std::fs::write(root.join("a.rs"), "v1\n").unwrap();
    let (doc_tx, doc_rx) = std::sync::mpsc::channel();
    let backend = testutil::pipe_backend(
        test_spec(),
        root.clone(),
        test_budgets(),
        doc_recording_server(doc_tx),
    );
    let (sink, _rx) = collector();
    let att = attach(&root, &backend, 0, sink);
    wait_ready(&backend);
    att.buffer("a.rs", Some(b"v2\n".to_vec()));
    wait_doc(&doc_rx, |m, _| (m == "textDocument/didOpen").then_some(()));
    std::fs::write(root.join("a.rs"), "v2\n").unwrap();
    backend.send(crate::backend::Cmd::Dirty(vec![(root.join("a.rs"), false)]));
    wait_doc(&doc_rx, |m, _| (m == "textDocument/didSave").then_some(()));
}

/// A deleted file is not a save — it is a didClose. Sending didSave for a
/// path that no longer exists would ask the checker to read missing bytes.
#[test]
fn deleted_file_does_not_notify_did_save() {
    let root = tmp_root("didsave-delete");
    std::fs::write(root.join("a.rs"), "v1\n").unwrap();
    std::fs::write(root.join("b.rs"), "v1\n").unwrap();
    let (doc_tx, doc_rx) = std::sync::mpsc::channel();
    let backend = testutil::pipe_backend(
        test_spec(),
        root.clone(),
        test_budgets(),
        doc_recording_server(doc_tx),
    );
    let (sink, _rx) = collector();
    let _att = attach(&root, &backend, 0, sink);
    wait_ready(&backend);
    std::fs::remove_file(root.join("a.rs")).unwrap();
    backend.send(crate::backend::Cmd::Dirty(vec![(root.join("a.rs"), false)]));
    // b.rs's save is the barrier: it is queued after a.rs's flush, so
    // once it arrives, any didSave for a.rs would already have been seen.
    std::fs::write(root.join("b.rs"), "v2\n").unwrap();
    backend.send(crate::backend::Cmd::Dirty(vec![(root.join("b.rs"), false)]));
    let uri = wait_doc(&doc_rx, |m, p| {
        (m == "textDocument/didSave")
            .then(|| p["textDocument"]["uri"].as_str().unwrap().to_string())
    });
    assert!(uri.ends_with("/b.rs"), "didSave for a deleted file: {uri}");
}

#[test]
fn detach_releases_overlays_to_disk() {
    let root = tmp_root("overlay-detach");
    std::fs::write(root.join("a.rs"), "disk v1\n").unwrap();
    let (doc_tx, doc_rx) = std::sync::mpsc::channel();
    let backend = testutil::pipe_backend(
        test_spec(),
        root.clone(),
        test_budgets(),
        doc_recording_server(doc_tx),
    );
    let (sink, _rx) = collector();
    let att = attach(&root, &backend, 0, sink);
    wait_ready(&backend);
    att.buffer("a.rs", Some(b"buffer\n".to_vec()));
    wait_doc(&doc_rx, |m, _| (m == "textDocument/didOpen").then_some(()));
    // Disconnect (Attachment drop) releases the overlay: the document
    // reverts to disk truth.
    drop(att);
    let text = wait_doc(&doc_rx, |m, p| {
        (m == "textDocument/didChange")
            .then(|| p["contentChanges"][0]["text"].as_str().unwrap().to_string())
    });
    assert_eq!(text, "disk v1\n");
}

#[test]
fn overlaid_doc_pinned_against_eviction() {
    let root = tmp_root("overlay-pin");
    std::fs::write(root.join("a.rs"), "one\n").unwrap();
    std::fs::write(root.join("b.rs"), "two\n").unwrap();
    let budgets = Budgets {
        max_docs: 1,
        ..test_budgets()
    };
    let (doc_tx, doc_rx) = std::sync::mpsc::channel();
    let backend = testutil::pipe_backend(
        test_spec(),
        root.clone(),
        budgets,
        doc_recording_server(doc_tx),
    );
    let (sink, rx) = collector();
    let att = attach(&root, &backend, 0, sink.clone());
    wait_ready(&backend);
    att.buffer("a.rs", Some(b"buffer\n".to_vec()));
    wait_doc(&doc_rx, |m, _| (m == "textDocument/didOpen").then_some(()));
    // Opening a second doc exceeds max_docs = 1, but neither the pinned
    // overlay nor the query's own document may be evicted — the cap
    // yields (bounded instead by max_overlays).
    att.query(5, LSP_QUERY_DEFINITION, 0, 0, 0, "b.rs", "", sink);
    wait_for(&rx, |msg| {
        parse_lsp_query_resp(msg)
            .filter(|r| r.nonce == 5)
            .map(|_| ())
    });
    while let Ok((method, _)) = doc_rx.try_recv() {
        assert_ne!(method, "textDocument/didClose");
    }
}

/// A shell-side edit — `git checkout`, `sed -i`, a formatter — reaches a
/// server that only diagnoses open documents. For those,
/// `workspace/didChangeWatchedFiles` is a no-op, so the watcher hint has
/// to admit the file to the open set or the change is invisible forever.
/// Capable servers must NOT be handed the document: they re-read from disk
/// themselves, and an open doc would make blit authoritative for content
/// it does not own.
#[test]
fn watcher_dirty_opens_docs_only_for_open_doc_servers() {
    fn observed_opens(needs_open_doc: bool) -> Vec<String> {
        let root = tmp_root(if needs_open_doc {
            "dirty-open"
        } else {
            "dirty-capable"
        });
        // Exists before start, so the real watcher stays quiet and the
        // injected hint drives the test deterministically.
        std::fs::write(root.join("touched.rs"), "fn a() {}\n").unwrap();
        // A file this backend does not route for must never be admitted.
        std::fs::write(root.join("notes.md"), "hello\n").unwrap();

        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let serve = move |mut reader: BufReader<Box<dyn Read + Send>>,
                          mut writer: Box<dyn Write + Send>| {
            while let Some(msg) = rpc::read_msg(&mut reader) {
                match msg {
                    rpc::RpcMsg::Request { id, method, .. } => {
                        let reply = match method.as_str() {
                            "initialize" => rpc::response(&id, json!({ "capabilities": {} })),
                            "shutdown" => rpc::response(&id, Value::Null),
                            _ => rpc::error_response(&id, -32601, "unhandled"),
                        };
                        let _ = rpc::write_msg(writer.as_mut(), &reply);
                    }
                    rpc::RpcMsg::Notification { method, params } => {
                        if method == "textDocument/didOpen" {
                            let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
                            let _ = tx.send(uri.to_string());
                        }
                        // Ordering marker: this always follows the batch,
                        // so receiving it means any didOpen already landed.
                        if method == "workspace/didChangeWatchedFiles" {
                            let _ = tx.send("__watched__".into());
                        }
                        if method == "exit" {
                            return;
                        }
                    }
                    rpc::RpcMsg::Response { .. } => {}
                }
            }
        };
        let spec = ServerSpec {
            needs_open_doc,
            ..test_spec()
        };
        let backend = testutil::pipe_backend(spec, root.clone(), test_budgets(), serve);
        wait_ready(&backend);

        backend.send(crate::backend::Cmd::Dirty(vec![
            (root.join("touched.rs"), false),
            (root.join("notes.md"), false),
        ]));

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut opens = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(u) if u == "__watched__" => break,
                Ok(u) => opens.push(u),
                Err(_) => break,
            }
        }
        opens
    }

    let opened = observed_opens(true);
    assert!(
        opened.iter().any(|u| u.ends_with("touched.rs")),
        "an open-doc-only server must be handed the dirty file, got {opened:?}"
    );
    assert!(
        !opened.iter().any(|u| u.ends_with("notes.md")),
        "a file this backend does not route for must not be admitted, got {opened:?}"
    );

    assert!(
        observed_opens(false).is_empty(),
        "a capable server re-reads from disk and must not be handed an open document"
    );
}
