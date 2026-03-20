#![allow(non_snake_case)]

use blit_remote::{self as remote, parse_server_msg as parse_msg, ServerMsg as Msg, TerminalState};
use js_sys::{Array, Object, Reflect};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn C2S_INPUT() -> u8 {
    remote::C2S_INPUT
}

#[wasm_bindgen]
pub fn C2S_RESIZE() -> u8 {
    remote::C2S_RESIZE
}

#[wasm_bindgen]
pub fn C2S_SCROLL() -> u8 {
    remote::C2S_SCROLL
}

#[wasm_bindgen]
pub fn C2S_ACK() -> u8 {
    remote::C2S_ACK
}

#[wasm_bindgen]
pub fn C2S_DISPLAY_RATE() -> u8 {
    remote::C2S_DISPLAY_RATE
}

#[wasm_bindgen]
pub fn C2S_CLIENT_METRICS() -> u8 {
    remote::C2S_CLIENT_METRICS
}

#[wasm_bindgen]
pub fn C2S_CREATE() -> u8 {
    remote::C2S_CREATE
}

#[wasm_bindgen]
pub fn C2S_FOCUS() -> u8 {
    remote::C2S_FOCUS
}

#[wasm_bindgen]
pub fn C2S_CLOSE() -> u8 {
    remote::C2S_CLOSE
}

#[wasm_bindgen]
pub fn C2S_SUBSCRIBE() -> u8 {
    remote::C2S_SUBSCRIBE
}

#[wasm_bindgen]
pub fn C2S_UNSUBSCRIBE() -> u8 {
    remote::C2S_UNSUBSCRIBE
}

#[wasm_bindgen]
pub fn C2S_SEARCH() -> u8 {
    remote::C2S_SEARCH
}

#[wasm_bindgen]
pub fn S2C_UPDATE() -> u8 {
    remote::S2C_UPDATE
}

#[wasm_bindgen]
pub fn S2C_CREATED() -> u8 {
    remote::S2C_CREATED
}

#[wasm_bindgen]
pub fn S2C_CLOSED() -> u8 {
    remote::S2C_CLOSED
}

#[wasm_bindgen]
pub fn S2C_LIST() -> u8 {
    remote::S2C_LIST
}

#[wasm_bindgen]
pub fn S2C_TITLE() -> u8 {
    remote::S2C_TITLE
}

#[wasm_bindgen]
pub fn S2C_SEARCH_RESULTS() -> u8 {
    remote::S2C_SEARCH_RESULTS
}

#[wasm_bindgen]
pub fn msg_create(rows: u16, cols: u16) -> Vec<u8> {
    remote::msg_create(rows, cols)
}

#[wasm_bindgen]
pub fn msg_create_command(rows: u16, cols: u16, command: &str) -> Vec<u8> {
    remote::msg_create_command(rows, cols, command)
}

#[wasm_bindgen]
pub fn msg_input(pty_id: u16, data: &[u8]) -> Vec<u8> {
    remote::msg_input(pty_id, data)
}

#[wasm_bindgen]
pub fn msg_resize(pty_id: u16, rows: u16, cols: u16) -> Vec<u8> {
    remote::msg_resize(pty_id, rows, cols)
}

#[wasm_bindgen]
pub fn msg_focus(pty_id: u16) -> Vec<u8> {
    remote::msg_focus(pty_id)
}

#[wasm_bindgen]
pub fn msg_close(pty_id: u16) -> Vec<u8> {
    remote::msg_close(pty_id)
}

#[wasm_bindgen]
pub fn msg_ack() -> Vec<u8> {
    remote::msg_ack()
}

#[wasm_bindgen]
pub fn msg_scroll(pty_id: u16, offset: u32) -> Vec<u8> {
    remote::msg_scroll(pty_id, offset)
}

#[wasm_bindgen]
pub fn msg_search(request_id: u16, query: &str) -> Vec<u8> {
    remote::msg_search(request_id, query)
}

#[wasm_bindgen]
pub fn msg_display_rate(fps: u16) -> Vec<u8> {
    remote::msg_display_rate(fps)
}

#[wasm_bindgen]
pub fn msg_client_metrics(backlog: u16, ack_ahead: u16, apply_ms_x10: u16) -> Vec<u8> {
    remote::msg_client_metrics(backlog, ack_ahead, apply_ms_x10)
}

#[wasm_bindgen]
pub fn msg_subscribe(pty_id: u16) -> Vec<u8> {
    remote::msg_subscribe(pty_id)
}

#[wasm_bindgen]
pub fn msg_unsubscribe(pty_id: u16) -> Vec<u8> {
    remote::msg_unsubscribe(pty_id)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SearchResult {
    pty_id: u16,
    score: u32,
    primary_source: u8,
    matched_sources: u8,
    scroll_offset: Option<u32>,
    context: String,
}

impl SearchResult {
    fn from_remote(result: remote::SearchResultEntry<'_>) -> Self {
        Self {
            pty_id: result.pty_id,
            score: result.score,
            primary_source: result.primary_source,
            matched_sources: result.matched_sources,
            scroll_offset: result.scroll_offset,
            context: String::from_utf8_lossy(result.context).into_owned(),
        }
    }

    fn to_js_value(&self) -> JsValue {
        let obj = Object::new();
        Reflect::set(&obj, &JsValue::from_str("ptyId"), &JsValue::from(self.pty_id)).unwrap();
        Reflect::set(&obj, &JsValue::from_str("score"), &JsValue::from(self.score)).unwrap();
        Reflect::set(&obj, &JsValue::from_str("primarySource"), &JsValue::from(self.primary_source)).unwrap();
        Reflect::set(&obj, &JsValue::from_str("matchedSources"), &JsValue::from(self.matched_sources)).unwrap();
        Reflect::set(&obj, &JsValue::from_str("scrollOffset"), &self.scroll_offset.map(JsValue::from).unwrap_or(JsValue::NULL)).unwrap();
        Reflect::set(&obj, &JsValue::from_str("context"), &JsValue::from_str(&self.context)).unwrap();
        obj.into()
    }
}

#[wasm_bindgen]
pub struct ServerMsg {
    kind: u8,
    pty_id: u16,
    request_id: u16,
    payload: Vec<u8>,
    pty_ids: Vec<u16>,
    search_results: Vec<SearchResult>,
}

impl ServerMsg {
    fn from_remote(msg: Msg<'_>) -> Self {
        match msg {
            Msg::Update { pty_id, payload } => Self {
                kind: remote::S2C_UPDATE, pty_id, request_id: 0,
                payload: payload.to_vec(), pty_ids: Vec::new(), search_results: Vec::new(),
            },
            Msg::Created { pty_id } => Self {
                kind: remote::S2C_CREATED, pty_id, request_id: 0,
                payload: Vec::new(), pty_ids: Vec::new(), search_results: Vec::new(),
            },
            Msg::Closed { pty_id } => Self {
                kind: remote::S2C_CLOSED, pty_id, request_id: 0,
                payload: Vec::new(), pty_ids: Vec::new(), search_results: Vec::new(),
            },
            Msg::List { pty_ids } => Self {
                kind: remote::S2C_LIST, pty_id: 0, request_id: 0,
                payload: Vec::new(), pty_ids, search_results: Vec::new(),
            },
            Msg::Title { pty_id, title } => Self {
                kind: remote::S2C_TITLE, pty_id, request_id: 0,
                payload: title.to_vec(), pty_ids: Vec::new(), search_results: Vec::new(),
            },
            Msg::SearchResults { request_id, results } => {
                let search_results: Vec<SearchResult> =
                    results.into_iter().map(SearchResult::from_remote).collect();
                let pty_ids = search_results.iter().map(|r| r.pty_id).collect();
                Self {
                    kind: remote::S2C_SEARCH_RESULTS, pty_id: 0, request_id,
                    payload: Vec::new(), pty_ids, search_results,
                }
            }
        }
    }
}

#[wasm_bindgen]
impl ServerMsg {
    pub fn kind(&self) -> u8 { self.kind }
    pub fn pty_id(&self) -> u16 { self.pty_id }
    pub fn request_id(&self) -> u16 { self.request_id }
    pub fn payload(&self) -> Vec<u8> { self.payload.clone() }
    pub fn title(&self) -> String { String::from_utf8_lossy(&self.payload).into_owned() }
    pub fn pty_ids(&self) -> Vec<u16> { self.pty_ids.clone() }

    pub fn search_result_count(&self) -> usize { self.search_results.len() }

    pub fn search_result(&self, index: usize) -> JsValue {
        self.search_results.get(index).map(SearchResult::to_js_value).unwrap_or(JsValue::NULL)
    }

    pub fn search_results(&self) -> Array {
        let results = Array::new_with_length(self.search_results.len() as u32);
        for (i, r) in self.search_results.iter().enumerate() {
            results.set(i as u32, r.to_js_value());
        }
        results
    }
}

#[wasm_bindgen]
pub fn parse_server_msg(data: &[u8]) -> Option<ServerMsg> {
    Some(ServerMsg::from_remote(parse_msg(data)?))
}

#[wasm_bindgen]
pub struct Terminal {
    inner: TerminalState,
}

#[wasm_bindgen]
impl Terminal {
    #[wasm_bindgen(constructor)]
    pub fn new(rows: u16, cols: u16) -> Self {
        Self { inner: TerminalState::new(rows, cols) }
    }

    #[wasm_bindgen(getter)]
    pub fn rows(&self) -> u16 { self.inner.rows() }
    #[wasm_bindgen(getter)]
    pub fn cols(&self) -> u16 { self.inner.cols() }
    #[wasm_bindgen(getter)]
    pub fn cursor_row(&self) -> u16 { self.inner.cursor_row() }
    #[wasm_bindgen(getter)]
    pub fn cursor_col(&self) -> u16 { self.inner.cursor_col() }

    pub fn cursor_visible(&self) -> bool { self.inner.mode() & 1 != 0 }
    pub fn app_cursor(&self) -> bool { self.inner.mode() & 2 != 0 }
    pub fn bracketed_paste(&self) -> bool { self.inner.mode() & 8 != 0 }
    pub fn mouse_mode(&self) -> u8 { ((self.inner.mode() >> 4) & 7) as u8 }
    pub fn mouse_encoding(&self) -> u8 { ((self.inner.mode() >> 7) & 3) as u8 }
    pub fn echo(&self) -> bool { self.inner.mode() & (1 << 9) != 0 }
    pub fn icanon(&self) -> bool { self.inner.mode() & (1 << 10) != 0 }
    pub fn title(&self) -> String { self.inner.title().to_owned() }

    pub fn feed_compressed(&mut self, data: &[u8]) { let _ = self.inner.feed_compressed(data); }
    pub fn feed_compressed_batch(&mut self, batch: &[u8]) { let _ = self.inner.feed_compressed_batch(batch); }

    pub fn get_text(&self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> String {
        self.inner.get_text(start_row, start_col, end_row, end_col)
    }
    pub fn get_all_text(&self) -> String { self.inner.get_all_text() }
    pub fn get_cell(&self, row: u16, col: u16) -> Vec<u8> { self.inner.get_cell(row, col) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_search_result(
        bytes: &mut Vec<u8>,
        pty_id: u16,
        score: u32,
        primary_source: u8,
        matched_sources: u8,
        scroll_offset: Option<u32>,
        context: &str,
    ) {
        bytes.extend_from_slice(&pty_id.to_le_bytes());
        bytes.extend_from_slice(&score.to_le_bytes());
        bytes.push(primary_source);
        bytes.push(matched_sources);
        bytes.extend_from_slice(&scroll_offset.unwrap_or(u32::MAX).to_le_bytes());
        bytes.extend_from_slice(&(context.len() as u16).to_le_bytes());
        bytes.extend_from_slice(context.as_bytes());
    }

    #[test]
    fn parse_server_msg_preserves_search_result_metadata() {
        let mut data = Vec::new();
        data.push(remote::S2C_SEARCH_RESULTS);
        data.extend_from_slice(&17u16.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        encode_search_result(&mut data, 3, 91, 1, 0b011, Some(12), "visible");
        encode_search_result(&mut data, 8, 44, 0, 0b001, None, "title");

        let msg = parse_server_msg(&data).unwrap();

        assert_eq!(msg.kind, remote::S2C_SEARCH_RESULTS);
        assert_eq!(msg.request_id, 17);
        assert_eq!(msg.pty_ids, vec![3, 8]);
        assert_eq!(
            msg.search_results,
            vec![
                SearchResult {
                    pty_id: 3, score: 91, primary_source: 1, matched_sources: 0b011,
                    scroll_offset: Some(12), context: "visible".into(),
                },
                SearchResult {
                    pty_id: 8, score: 44, primary_source: 0, matched_sources: 0b001,
                    scroll_offset: None, context: "title".into(),
                },
            ]
        );
    }
}
