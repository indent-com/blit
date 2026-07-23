//! LSP JSON results → wire records (docs/design/lsp.md `LSP_QUERY`).
//!
//! Normalization lives here: `Location` vs `LocationLink`, hierarchical
//! `DocumentSymbol[]` vs flat `SymbolInformation[]`, the three hover
//! content shapes, and `WorkspaceEdit`'s two encodings all become the
//! four record kinds. Positions are transcoded from the backend's
//! negotiated encoding to wire byte columns against the text the
//! backend actually holds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use blit_remote::lsp::{
    LSP_HASH_NONE, LSP_MARKUP_MARKDOWN, LSP_MARKUP_PLAIN, LSP_SYMBOL_DEPRECATED, LspHash,
    LspQueryRecord, append_lsp_query_record,
};
use serde_json::Value;

use crate::text::{self, PositionEncoding};

/// Per-response source of file text and hashes: open-set documents
/// first (the exact text the backend holds), then a disk-read cache.
pub struct TextSource<'a> {
    pub open_docs: &'a HashMap<PathBuf, (String, LspHash)>,
    pub disk: HashMap<PathBuf, Option<(String, LspHash)>>,
}

impl<'a> TextSource<'a> {
    pub fn new(open_docs: &'a HashMap<PathBuf, (String, LspHash)>) -> Self {
        TextSource {
            open_docs,
            disk: HashMap::new(),
        }
    }

    fn lookup(&mut self, path: &Path) -> Option<(&str, LspHash)> {
        if let Some((text, hash)) = self.open_docs.get(path) {
            return Some((text.as_str(), *hash));
        }
        let entry = self.disk.entry(path.to_path_buf()).or_insert_with(|| {
            std::fs::read(path).ok().and_then(|bytes| {
                let hash = text::hash_bytes(&bytes);
                String::from_utf8(bytes).ok().map(|text| (text, hash))
            })
        });
        entry.as_ref().map(|(text, hash)| (text.as_str(), *hash))
    }
}

/// Budgets applied while appending records.
pub struct RecordSink<'a> {
    pub buf: &'a mut Vec<u8>,
    pub entries_left: usize,
    pub bytes_max: usize,
    pub truncated: bool,
    /// A `RENAME` plan dropped whole-file operations it cannot project.
    pub incomplete: bool,
}

impl<'a> RecordSink<'a> {
    pub fn push(&mut self, record: &LspQueryRecord<'_>) {
        if self.entries_left == 0 || self.buf.len() >= self.bytes_max {
            self.truncated = true;
            return;
        }
        self.entries_left -= 1;
        append_lsp_query_record(self.buf, record);
    }
}

/// One transcoded wire range.
pub struct WireRange {
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

/// A range in the server's own units, for when no text is available to
/// transcode against (unreadable or non-file targets).
pub fn raw_range(range: &Value) -> WireRange {
    WireRange {
        line: range["start"]["line"].as_u64().unwrap_or(0) as u32,
        col: range["start"]["character"].as_u64().unwrap_or(0) as u32,
        end_line: range["end"]["line"].as_u64().unwrap_or(0) as u32,
        end_col: range["end"]["character"].as_u64().unwrap_or(0) as u32,
    }
}

/// LSP `{start, end}` range → wire byte columns, against `text`.
pub fn range_to_wire(range: &Value, txt: &str, enc: PositionEncoding) -> WireRange {
    let pos = |p: &Value| -> (u32, u32) {
        let line = p["line"].as_u64().unwrap_or(0) as u32;
        let character = p["character"].as_u64().unwrap_or(0) as u32;
        (line, text::col_from_encoding(txt, line, character, enc))
    };
    let (line, col) = pos(&range["start"]);
    let (end_line, end_col) = pos(&range["end"]);
    WireRange {
        line,
        col,
        end_line,
        end_col,
    }
}

/// One `Location`/`LocationLink` → a `LOCATION` record. Non-`file:`
/// URIs are dropped rather than mis-projected.
fn push_location(
    sink: &mut RecordSink<'_>,
    src: &mut TextSource<'_>,
    wire_root: &Path,
    uri: &str,
    range: &Value,
    enc: PositionEncoding,
) {
    let Some(path) = text::uri_to_path(uri) else {
        return;
    };
    let (range, hash) = match src.lookup(&path) {
        Some((txt, hash)) => (range_to_wire(range, txt, enc), hash),
        // Unreadable target: emit the server's raw positions with an
        // unknown hash — a byte-identical answer is better than none
        // for ASCII, and the zero hash marks it unverified.
        None => (raw_range(range), LSP_HASH_NONE),
    };
    sink.push(&LspQueryRecord::Location {
        flags: 0,
        hash,
        line: range.line,
        col: range.col,
        end_line: range.end_line,
        end_col: range.end_col,
        path: &text::wire_path(wire_root, &path),
    });
}

/// `textDocument/definition` (and references): `Location | Location[]
/// | LocationLink[] | null`.
pub fn locations(
    sink: &mut RecordSink<'_>,
    src: &mut TextSource<'_>,
    wire_root: &Path,
    result: &Value,
    enc: PositionEncoding,
) {
    let items: Vec<&Value> = match result {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![result],
        _ => return,
    };
    for item in items {
        if let Some(uri) = item["uri"].as_str() {
            push_location(sink, src, wire_root, uri, &item["range"], enc);
        } else if let Some(uri) = item["targetUri"].as_str() {
            // LocationLink: the selection range is the jump target.
            let range = if item["targetSelectionRange"].is_object() {
                &item["targetSelectionRange"]
            } else {
                &item["targetRange"]
            };
            push_location(sink, src, wire_root, uri, range, enc);
        }
    }
}

/// `textDocument/hover`: `contents` is `MarkupContent | MarkedString |
/// MarkedString[]`; everything becomes one markup record (plus a
/// `LOCATION` for the hovered range when the server reports one).
pub fn hover(
    sink: &mut RecordSink<'_>,
    src: &mut TextSource<'_>,
    wire_root: &Path,
    query_path: &Path,
    result: &Value,
    enc: PositionEncoding,
) {
    let contents = &result["contents"];
    let mut format = LSP_MARKUP_MARKDOWN;
    let mut body = String::new();
    let append_marked = |body: &mut String, item: &Value| {
        if let Some(s) = item.as_str() {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(s);
        } else if let (Some(language), Some(value)) =
            (item["language"].as_str(), item["value"].as_str())
        {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&format!("```{language}\n{value}\n```"));
        }
    };
    if let Some(kind) = contents["kind"].as_str() {
        // MarkupContent.
        if kind == "plaintext" {
            format = LSP_MARKUP_PLAIN;
        }
        body = contents["value"].as_str().unwrap_or_default().to_string();
    } else if let Some(items) = contents.as_array() {
        for item in items {
            append_marked(&mut body, item);
        }
    } else {
        append_marked(&mut body, contents);
    }
    if body.is_empty() {
        return;
    }
    sink.push(&LspQueryRecord::Markup {
        format,
        text: &body,
    });
    if result["range"].is_object()
        && let Some((txt, hash)) = src.lookup(query_path)
    {
        let range = range_to_wire(&result["range"], txt, enc);
        sink.push(&LspQueryRecord::Location {
            flags: 0,
            hash,
            line: range.line,
            col: range.col,
            end_line: range.end_line,
            end_col: range.end_col,
            path: &text::wire_path(wire_root, query_path),
        });
    }
}

fn symbol_flags(item: &Value) -> u8 {
    let mut flags = 0;
    if item["deprecated"].as_bool() == Some(true) {
        flags |= LSP_SYMBOL_DEPRECATED;
    }
    if let Some(tags) = item["tags"].as_array()
        && tags.iter().any(|t| t.as_u64() == Some(1))
    {
        flags |= LSP_SYMBOL_DEPRECATED;
    }
    flags
}

/// `textDocument/documentSymbol`: hierarchical `DocumentSymbol[]`
/// (flattened pre-order via `depth`) or flat `SymbolInformation[]`.
pub fn doc_symbols(
    sink: &mut RecordSink<'_>,
    src: &mut TextSource<'_>,
    wire_root: &Path,
    query_path: &Path,
    result: &Value,
    enc: PositionEncoding,
) {
    let Some(items) = result.as_array() else {
        return;
    };
    let query_wire = text::wire_path(wire_root, query_path);
    // One owned copy of the query file's text: the tree branch and the
    // per-item lookups below cannot share a live borrow of `src`.
    let query_txt: Option<(String, LspHash)> =
        src.lookup(query_path).map(|(t, h)| (t.to_string(), h));
    for item in items {
        if item["range"].is_object() {
            // DocumentSymbol tree.
            let txt = query_txt.as_ref().map(|(t, h)| (t.as_str(), *h));
            flatten_doc_symbol(sink, &query_wire, txt, item, 0, enc);
        } else if let Some(uri) = item["location"]["uri"].as_str() {
            // SymbolInformation.
            let Some(path) = text::uri_to_path(uri) else {
                continue;
            };
            let looked = src.lookup(&path);
            push_symbol(
                sink,
                &text::wire_path(wire_root, &path),
                looked,
                item,
                &item["location"]["range"],
                0,
                enc,
            );
        }
    }
}

fn flatten_doc_symbol(
    sink: &mut RecordSink<'_>,
    wire: &str,
    txt: Option<(&str, LspHash)>,
    item: &Value,
    depth: u8,
    enc: PositionEncoding,
) {
    push_symbol(sink, wire, txt, item, &item["range"], depth, enc);
    if let Some(children) = item["children"].as_array() {
        for child in children {
            flatten_doc_symbol(sink, wire, txt, child, depth.saturating_add(1), enc);
        }
    }
}

fn push_symbol(
    sink: &mut RecordSink<'_>,
    wire: &str,
    txt: Option<(&str, LspHash)>,
    item: &Value,
    range: &Value,
    depth: u8,
    enc: PositionEncoding,
) {
    let name = item["name"].as_str().unwrap_or_default();
    let sym_kind = item["kind"].as_u64().unwrap_or(0) as u8;
    let wr = match txt {
        Some((t, _)) => range_to_wire(range, t, enc),
        None => raw_range(range),
    };
    sink.push(&LspQueryRecord::Symbol {
        sym_kind,
        flags: symbol_flags(item),
        depth,
        line: wr.line,
        col: wr.col,
        end_line: wr.end_line,
        end_col: wr.end_col,
        name,
        path: wire,
    });
}

/// `workspace/symbol`: `SymbolInformation[] | WorkspaceSymbol[]`.
/// 3.17 `WorkspaceSymbol` may carry a location without a range; those
/// emit the zero range (a `workspaceSymbol/resolve` round remains a
/// server-side improvement that needs no wire change).
pub fn ws_symbols(
    sink: &mut RecordSink<'_>,
    src: &mut TextSource<'_>,
    wire_root: &Path,
    result: &Value,
    enc: PositionEncoding,
) {
    let Some(items) = result.as_array() else {
        return;
    };
    for item in items {
        let Some(uri) = item["location"]["uri"].as_str() else {
            continue;
        };
        let Some(path) = text::uri_to_path(uri) else {
            continue;
        };
        let looked = src.lookup(&path);
        push_symbol(
            sink,
            &text::wire_path(wire_root, &path),
            looked,
            item,
            &item["location"]["range"],
            0,
            enc,
        );
    }
}

/// `textDocument/rename`: a `WorkspaceEdit` in either encoding →
/// `EDIT` records. File create/rename/delete operations have no v1
/// projection and are skipped.
pub fn rename_edits(
    sink: &mut RecordSink<'_>,
    src: &mut TextSource<'_>,
    wire_root: &Path,
    result: &Value,
    enc: PositionEncoding,
) {
    let push_edits =
        |sink: &mut RecordSink<'_>, src: &mut TextSource<'_>, uri: &str, edits: &Value| {
            let Some(path) = text::uri_to_path(uri) else {
                return;
            };
            let Some(edits) = edits.as_array() else {
                return;
            };
            let looked = src.lookup(&path);
            let wire = text::wire_path(wire_root, &path);
            for edit in edits {
                let new_text = edit["newText"].as_str().unwrap_or_default();
                let (wr, hash) = match looked {
                    Some((t, hash)) => (range_to_wire(&edit["range"], t, enc), hash),
                    None => (raw_range(&edit["range"]), LSP_HASH_NONE),
                };
                sink.push(&LspQueryRecord::Edit {
                    flags: 0,
                    hash,
                    line: wr.line,
                    col: wr.col,
                    end_line: wr.end_line,
                    end_col: wr.end_col,
                    new_text,
                    path: &wire,
                });
            }
        };
    // `documentChanges` and `changes` are mutually exclusive encodings
    // of the same edit set; the former supersedes the latter when
    // present, so never emit from both or every edit is duplicated.
    if let Some(doc_changes) = result["documentChanges"].as_array() {
        for change in doc_changes {
            if let Some(uri) = change["textDocument"]["uri"].as_str() {
                push_edits(sink, src, uri, &change["edits"]);
            } else if change["kind"].is_string() {
                // A create/rename/delete file operation: it has no
                // textDocument edits array, and v1 has no projection for
                // it. Flag the plan incomplete rather than presenting a
                // partial rename as whole.
                sink.incomplete = true;
            }
        }
    } else if let Some(changes) = result["changes"].as_object() {
        for (uri, edits) in changes {
            push_edits(sink, src, uri, edits);
        }
    }
}
