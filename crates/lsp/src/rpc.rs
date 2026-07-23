//! JSON-RPC 2.0 over Content-Length-framed stdio — the LSP base
//! protocol. Framing only: request correlation and method semantics live
//! in the engine. Blocking I/O on plain threads, the house engine shape.

#[cfg(test)]
use std::io::Write;
use std::io::{BufRead, BufReader, Read};

use serde_json::{Value, json};

/// One inbound JSON-RPC message, already classified.
#[derive(Debug)]
pub enum RpcMsg {
    /// Server → client request: must be answered.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// Response to one of our requests.
    Response {
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
    },
    Notification {
        method: String,
        params: Value,
    },
}

/// Frame and write one JSON-RPC payload. Production framing goes
/// through [`frame`] into the writer thread; this stays for tests and
/// mock servers that own a writer directly.
#[cfg(test)]
pub fn write_msg(writer: &mut dyn Write, payload: &Value) -> std::io::Result<()> {
    writer.write_all(&frame(payload))?;
    writer.flush()
}

/// The Content-Length-framed bytes of one payload, ready to hand a
/// dedicated writer thread (so the engine never blocks on stdin).
pub fn frame(payload: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    let mut out = Vec::with_capacity(body.len() + 32);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(&body);
    out
}

pub fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// A response to a server → client request.
pub fn response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Read one framed message. `None` on EOF or malformed framing (the
/// stream is unrecoverable either way — the engine treats it as child
/// exit).
pub fn read_msg(reader: &mut BufReader<Box<dyn Read + Send>>) -> Option<RpcMsg> {
    // A header line is a handful of ASCII bytes; a broken server that
    // streams without a newline must not force an unbounded allocation,
    // the same threat the body's MAX_DECOMPRESSED guard covers below.
    const MAX_HEADER_LINE: usize = 8 * 1024;
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = Vec::new();
        let read = (&mut *reader)
            .take(MAX_HEADER_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
            .ok()?;
        if read == 0 {
            return None; // EOF
        }
        // An over-long or unterminated line is a framing error: end the
        // session rather than accept a partial header.
        if line.len() > MAX_HEADER_LINE || line.last() != Some(&b'\n') {
            return None;
        }
        let line = String::from_utf8_lossy(&line);
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            content_length = value.trim().parse().ok();
        }
        // Content-Type headers are ignored (utf-8 is the only sane
        // value and the deprecated utf8 alias decodes identically).
    }
    let len = content_length?;
    // A hostile or broken server must not force an unbounded allocation.
    if len > blit_remote::MAX_DECOMPRESSED {
        return None;
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    let value: Value = serde_json::from_slice(&body).ok()?;
    classify(value)
}

fn classify(value: Value) -> Option<RpcMsg> {
    let obj = value.as_object()?;
    let method = obj.get("method").and_then(|m| m.as_str());
    let id = obj.get("id").cloned();
    match (method, id) {
        (Some(method), Some(id)) => Some(RpcMsg::Request {
            id,
            method: method.to_string(),
            params: obj.get("params").cloned().unwrap_or(Value::Null),
        }),
        (Some(method), None) => Some(RpcMsg::Notification {
            method: method.to_string(),
            params: obj.get("params").cloned().unwrap_or(Value::Null),
        }),
        (None, Some(id)) => Some(RpcMsg::Response {
            id,
            result: obj.get("result").cloned(),
            error: obj.get("error").cloned(),
        }),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &request(1, "initialize", json!({"a": 1}))).unwrap();
        write_msg(&mut buf, &notification("initialized", json!({}))).unwrap();

        let boxed: Box<dyn Read + Send> = Box::new(std::io::Cursor::new(buf));
        let mut reader = BufReader::new(boxed);
        match read_msg(&mut reader).unwrap() {
            RpcMsg::Request { id, method, .. } => {
                assert_eq!(id, json!(1));
                assert_eq!(method, "initialize");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match read_msg(&mut reader).unwrap() {
            RpcMsg::Notification { method, .. } => assert_eq!(method, "initialized"),
            other => panic!("unexpected: {other:?}"),
        }
        assert!(read_msg(&mut reader).is_none());
    }
}
