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
pub fn C2S_CREATE_AT() -> u8 {
    remote::C2S_CREATE_AT
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
pub fn msg_create_tagged(rows: u16, cols: u16, tag: &str) -> Vec<u8> {
    remote::msg_create_tagged(rows, cols, tag)
}

#[wasm_bindgen]
pub fn msg_create_command(rows: u16, cols: u16, command: &str) -> Vec<u8> {
    remote::msg_create_command(rows, cols, command)
}

#[wasm_bindgen]
pub fn msg_create_tagged_command(rows: u16, cols: u16, tag: &str, command: &str) -> Vec<u8> {
    remote::msg_create_tagged_command(rows, cols, tag, command)
}

#[wasm_bindgen]
pub fn msg_create_at(rows: u16, cols: u16, tag: &str, src_pty_id: u16) -> Vec<u8> {
    remote::msg_create_at(rows, cols, tag, src_pty_id)
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
    tag: String,
    payload: Vec<u8>,
    pty_ids: Vec<u16>,
    tags: Vec<String>,
    search_results: Vec<SearchResult>,
}

impl ServerMsg {
    fn from_remote(msg: Msg<'_>) -> Self {
        match msg {
            Msg::Hello { version, features } => Self {
                kind: remote::S2C_HELLO, pty_id: version, request_id: 0, tag: String::new(),
                payload: features.to_le_bytes().to_vec(), pty_ids: Vec::new(), tags: Vec::new(), search_results: Vec::new(),
            },
            Msg::Update { pty_id, payload } => Self {
                kind: remote::S2C_UPDATE, pty_id, request_id: 0, tag: String::new(),
                payload: payload.to_vec(), pty_ids: Vec::new(), tags: Vec::new(), search_results: Vec::new(),
            },
            Msg::Created { pty_id, tag } => Self {
                kind: remote::S2C_CREATED, pty_id, request_id: 0, tag: tag.to_owned(),
                payload: Vec::new(), pty_ids: Vec::new(), tags: Vec::new(), search_results: Vec::new(),
            },
            Msg::CreatedN { nonce, pty_id, tag } => Self {
                kind: remote::S2C_CREATED_N, pty_id, request_id: nonce, tag: tag.to_owned(),
                payload: Vec::new(), pty_ids: Vec::new(), tags: Vec::new(), search_results: Vec::new(),
            },
            Msg::Closed { pty_id } => Self {
                kind: remote::S2C_CLOSED, pty_id, request_id: 0, tag: String::new(),
                payload: Vec::new(), pty_ids: Vec::new(), tags: Vec::new(), search_results: Vec::new(),
            },
            Msg::List { entries } => {
                let pty_ids = entries.iter().map(|e| e.pty_id).collect();
                let tags = entries.iter().map(|e| e.tag.to_owned()).collect();
                Self {
                    kind: remote::S2C_LIST, pty_id: 0, request_id: 0, tag: String::new(),
                    payload: Vec::new(), pty_ids, tags, search_results: Vec::new(),
                }
            },
            Msg::Title { pty_id, title } => Self {
                kind: remote::S2C_TITLE, pty_id, request_id: 0, tag: String::new(),
                payload: title.to_vec(), pty_ids: Vec::new(), tags: Vec::new(), search_results: Vec::new(),
            },
            Msg::SearchResults { request_id, results } => {
                let search_results: Vec<SearchResult> =
                    results.into_iter().map(SearchResult::from_remote).collect();
                let pty_ids = search_results.iter().map(|r| r.pty_id).collect();
                Self {
                    kind: remote::S2C_SEARCH_RESULTS, pty_id: 0, request_id, tag: String::new(),
                    payload: Vec::new(), pty_ids, tags: Vec::new(), search_results,
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
    pub fn tag(&self) -> String { self.tag.clone() }
    pub fn payload(&self) -> Vec<u8> { self.payload.clone() }
    pub fn title(&self) -> String { String::from_utf8_lossy(&self.payload).into_owned() }
    pub fn pty_ids(&self) -> Vec<u16> { self.pty_ids.clone() }
    pub fn tags(&self) -> Array {
        let arr = Array::new_with_length(self.tags.len() as u32);
        for (i, t) in self.tags.iter().enumerate() {
            arr.set(i as u32, JsValue::from_str(t));
        }
        arr
    }

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

    // ---------------------------------------------------------------
    // 1. Protocol constant functions
    // ---------------------------------------------------------------

    #[test]
    fn constant_c2s_values() {
        assert_eq!(C2S_INPUT(), 0x00);
        assert_eq!(C2S_RESIZE(), 0x01);
        assert_eq!(C2S_SCROLL(), 0x02);
        assert_eq!(C2S_ACK(), 0x03);
        assert_eq!(C2S_DISPLAY_RATE(), 0x04);
        assert_eq!(C2S_CLIENT_METRICS(), 0x05);
        assert_eq!(C2S_CREATE(), 0x10);
        assert_eq!(C2S_FOCUS(), 0x11);
        assert_eq!(C2S_CLOSE(), 0x12);
        assert_eq!(C2S_SUBSCRIBE(), 0x13);
        assert_eq!(C2S_UNSUBSCRIBE(), 0x14);
        assert_eq!(C2S_SEARCH(), 0x15);
    }

    #[test]
    fn constant_s2c_values() {
        assert_eq!(S2C_UPDATE(), 0x00);
        assert_eq!(S2C_CREATED(), 0x01);
        assert_eq!(S2C_CLOSED(), 0x02);
        assert_eq!(S2C_LIST(), 0x03);
        assert_eq!(S2C_TITLE(), 0x04);
        assert_eq!(S2C_SEARCH_RESULTS(), 0x05);
    }

    #[test]
    fn constants_match_remote_crate() {
        assert_eq!(C2S_INPUT(), remote::C2S_INPUT);
        assert_eq!(C2S_RESIZE(), remote::C2S_RESIZE);
        assert_eq!(C2S_SCROLL(), remote::C2S_SCROLL);
        assert_eq!(C2S_ACK(), remote::C2S_ACK);
        assert_eq!(C2S_DISPLAY_RATE(), remote::C2S_DISPLAY_RATE);
        assert_eq!(C2S_CLIENT_METRICS(), remote::C2S_CLIENT_METRICS);
        assert_eq!(C2S_CREATE(), remote::C2S_CREATE);
        assert_eq!(C2S_FOCUS(), remote::C2S_FOCUS);
        assert_eq!(C2S_CLOSE(), remote::C2S_CLOSE);
        assert_eq!(C2S_SUBSCRIBE(), remote::C2S_SUBSCRIBE);
        assert_eq!(C2S_UNSUBSCRIBE(), remote::C2S_UNSUBSCRIBE);
        assert_eq!(C2S_SEARCH(), remote::C2S_SEARCH);
        assert_eq!(S2C_UPDATE(), remote::S2C_UPDATE);
        assert_eq!(S2C_CREATED(), remote::S2C_CREATED);
        assert_eq!(S2C_CLOSED(), remote::S2C_CLOSED);
        assert_eq!(S2C_LIST(), remote::S2C_LIST);
        assert_eq!(S2C_TITLE(), remote::S2C_TITLE);
        assert_eq!(S2C_SEARCH_RESULTS(), remote::S2C_SEARCH_RESULTS);
    }

    // ---------------------------------------------------------------
    // 2. ServerMsg::from_remote() for each variant
    // ---------------------------------------------------------------

    #[test]
    fn from_remote_update() {
        let payload = b"hello world";
        let msg = ServerMsg::from_remote(remote::ServerMsg::Update { pty_id: 42, payload });
        assert_eq!(msg.kind, remote::S2C_UPDATE);
        assert_eq!(msg.pty_id, 42);
        assert_eq!(msg.request_id, 0);
        assert_eq!(msg.tag, "");
        assert_eq!(msg.payload, b"hello world");
        assert!(msg.pty_ids.is_empty());
        assert!(msg.tags.is_empty());
        assert!(msg.search_results.is_empty());
    }

    #[test]
    fn from_remote_created() {
        let msg = ServerMsg::from_remote(remote::ServerMsg::Created { pty_id: 7, tag: "my-tag" });
        assert_eq!(msg.kind, remote::S2C_CREATED);
        assert_eq!(msg.pty_id, 7);
        assert_eq!(msg.tag, "my-tag");
        assert!(msg.payload.is_empty());
        assert!(msg.pty_ids.is_empty());
        assert!(msg.tags.is_empty());
    }

    #[test]
    fn from_remote_closed() {
        let msg = ServerMsg::from_remote(remote::ServerMsg::Closed { pty_id: 99 });
        assert_eq!(msg.kind, remote::S2C_CLOSED);
        assert_eq!(msg.pty_id, 99);
        assert_eq!(msg.tag, "");
        assert!(msg.payload.is_empty());
        assert!(msg.pty_ids.is_empty());
    }

    #[test]
    fn from_remote_list() {
        let entries = vec![
            remote::PtyListEntry { pty_id: 1, tag: "alpha" },
            remote::PtyListEntry { pty_id: 2, tag: "beta" },
            remote::PtyListEntry { pty_id: 3, tag: "" },
        ];
        let msg = ServerMsg::from_remote(remote::ServerMsg::List { entries });
        assert_eq!(msg.kind, remote::S2C_LIST);
        assert_eq!(msg.pty_id, 0);
        assert_eq!(msg.pty_ids, vec![1, 2, 3]);
        assert_eq!(msg.tags, vec!["alpha", "beta", ""]);
        assert!(msg.payload.is_empty());
    }

    #[test]
    fn from_remote_title() {
        let title_bytes = b"My Terminal";
        let msg = ServerMsg::from_remote(remote::ServerMsg::Title { pty_id: 5, title: title_bytes });
        assert_eq!(msg.kind, remote::S2C_TITLE);
        assert_eq!(msg.pty_id, 5);
        assert_eq!(msg.payload, b"My Terminal");
        assert_eq!(msg.tag, "");
    }

    #[test]
    fn from_remote_search_results() {
        let results = vec![
            remote::SearchResultEntry {
                pty_id: 10, score: 100, primary_source: 2, matched_sources: 0b101,
                scroll_offset: Some(50), context: b"ctx1",
            },
        ];
        let msg = ServerMsg::from_remote(remote::ServerMsg::SearchResults { request_id: 33, results });
        assert_eq!(msg.kind, remote::S2C_SEARCH_RESULTS);
        assert_eq!(msg.request_id, 33);
        assert_eq!(msg.pty_ids, vec![10]);
        assert_eq!(msg.search_results.len(), 1);
        assert_eq!(msg.search_results[0].pty_id, 10);
        assert_eq!(msg.search_results[0].score, 100);
        assert_eq!(msg.search_results[0].primary_source, 2);
        assert_eq!(msg.search_results[0].matched_sources, 0b101);
        assert_eq!(msg.search_results[0].scroll_offset, Some(50));
        assert_eq!(msg.search_results[0].context, "ctx1");
    }

    #[test]
    fn from_remote_search_results_no_scroll_offset() {
        let results = vec![
            remote::SearchResultEntry {
                pty_id: 4, score: 0, primary_source: 0, matched_sources: 0,
                scroll_offset: None, context: b"",
            },
        ];
        let msg = ServerMsg::from_remote(remote::ServerMsg::SearchResults { request_id: 0, results });
        assert_eq!(msg.search_results[0].scroll_offset, None);
        assert_eq!(msg.search_results[0].context, "");
    }

    // ---------------------------------------------------------------
    // 3. Message builders produce correct wire format
    // ---------------------------------------------------------------

    #[test]
    fn msg_create_basic() {
        let m = msg_create(24, 80);
        assert_eq!(m[0], remote::C2S_CREATE);
        assert_eq!(u16::from_le_bytes([m[1], m[2]]), 24); // rows
        assert_eq!(u16::from_le_bytes([m[3], m[4]]), 80); // cols
        assert_eq!(u16::from_le_bytes([m[5], m[6]]), 0);  // tag_len = 0
        assert_eq!(m.len(), 7);
    }

    #[test]
    fn msg_create_tagged_includes_tag() {
        let m = msg_create_tagged(30, 120, "test");
        assert_eq!(m[0], remote::C2S_CREATE);
        assert_eq!(u16::from_le_bytes([m[1], m[2]]), 30);
        assert_eq!(u16::from_le_bytes([m[3], m[4]]), 120);
        assert_eq!(u16::from_le_bytes([m[5], m[6]]), 4); // tag_len
        assert_eq!(&m[7..11], b"test");
        assert_eq!(m.len(), 11);
    }

    #[test]
    fn msg_create_command_includes_command() {
        let m = msg_create_command(24, 80, "bash");
        assert_eq!(m[0], remote::C2S_CREATE);
        assert_eq!(u16::from_le_bytes([m[5], m[6]]), 0); // tag_len = 0
        assert_eq!(&m[7..], b"bash");
    }

    #[test]
    fn msg_create_tagged_command_includes_both() {
        let m = msg_create_tagged_command(24, 80, "t", "cmd");
        assert_eq!(m[0], remote::C2S_CREATE);
        assert_eq!(u16::from_le_bytes([m[5], m[6]]), 1); // tag_len = 1
        assert_eq!(m[7], b't');
        assert_eq!(&m[8..], b"cmd");
    }

    #[test]
    fn msg_create_at_encodes_src_pty_id() {
        let m = msg_create_at(24, 80, "shell", 42);
        assert_eq!(m[0], remote::C2S_CREATE_AT);
        assert_eq!(u16::from_le_bytes([m[1], m[2]]), 24);
        assert_eq!(u16::from_le_bytes([m[3], m[4]]), 80);
        assert_eq!(u16::from_le_bytes([m[5], m[6]]), 5); // tag_len "shell"
        assert_eq!(&m[7..12], b"shell");
        assert_eq!(u16::from_le_bytes([m[12], m[13]]), 42); // src_pty_id
        assert_eq!(m.len(), 14);
    }

    #[test]
    fn msg_input_structure() {
        let m = msg_input(5, b"hello");
        assert_eq!(m[0], remote::C2S_INPUT);
        assert_eq!(u16::from_le_bytes([m[1], m[2]]), 5);
        assert_eq!(&m[3..], b"hello");
    }

    #[test]
    fn msg_resize_structure() {
        let m = msg_resize(3, 40, 100);
        assert_eq!(m[0], remote::C2S_RESIZE);
        assert_eq!(u16::from_le_bytes([m[1], m[2]]), 3);   // pty_id
        assert_eq!(u16::from_le_bytes([m[3], m[4]]), 40);  // rows
        assert_eq!(u16::from_le_bytes([m[5], m[6]]), 100); // cols
    }

    #[test]
    fn msg_focus_structure() {
        let m = msg_focus(12);
        assert_eq!(m[0], remote::C2S_FOCUS);
        assert_eq!(u16::from_le_bytes([m[1], m[2]]), 12);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn msg_close_structure() {
        let m = msg_close(7);
        assert_eq!(m[0], remote::C2S_CLOSE);
        assert_eq!(u16::from_le_bytes([m[1], m[2]]), 7);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn msg_subscribe_structure() {
        let m = msg_subscribe(20);
        assert_eq!(m[0], remote::C2S_SUBSCRIBE);
        assert_eq!(u16::from_le_bytes([m[1], m[2]]), 20);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn msg_unsubscribe_structure() {
        let m = msg_unsubscribe(21);
        assert_eq!(m[0], remote::C2S_UNSUBSCRIBE);
        assert_eq!(u16::from_le_bytes([m[1], m[2]]), 21);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn msg_search_structure() {
        let m = msg_search(99, "find me");
        assert_eq!(m[0], remote::C2S_SEARCH);
        assert_eq!(u16::from_le_bytes([m[1], m[2]]), 99);
        assert_eq!(&m[3..], b"find me");
    }

    #[test]
    fn msg_ack_structure() {
        let m = msg_ack();
        assert_eq!(m, vec![remote::C2S_ACK]);
    }

    #[test]
    fn msg_scroll_structure() {
        let m = msg_scroll(4, 1000);
        assert_eq!(m[0], remote::C2S_SCROLL);
        assert_eq!(u16::from_le_bytes([m[1], m[2]]), 4);
        assert_eq!(u32::from_le_bytes([m[3], m[4], m[5], m[6]]), 1000);
    }

    #[test]
    fn msg_display_rate_structure() {
        let m = msg_display_rate(60);
        assert_eq!(m[0], remote::C2S_DISPLAY_RATE);
        assert_eq!(u16::from_le_bytes([m[1], m[2]]), 60);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn msg_client_metrics_structure() {
        let m = msg_client_metrics(10, 20, 30);
        assert_eq!(m[0], remote::C2S_CLIENT_METRICS);
        assert_eq!(u16::from_le_bytes([m[1], m[2]]), 10);  // backlog
        assert_eq!(u16::from_le_bytes([m[3], m[4]]), 20);  // ack_ahead
        assert_eq!(u16::from_le_bytes([m[5], m[6]]), 30);  // apply_ms_x10
    }

    // ---------------------------------------------------------------
    // 4. Terminal (via TerminalState) basic methods
    // ---------------------------------------------------------------

    #[test]
    fn terminal_state_new_dimensions() {
        let ts = TerminalState::new(24, 80);
        assert_eq!(ts.rows(), 24);
        assert_eq!(ts.cols(), 80);
    }

    #[test]
    fn terminal_state_initial_cursor() {
        let ts = TerminalState::new(24, 80);
        assert_eq!(ts.cursor_row(), 0);
        assert_eq!(ts.cursor_col(), 0);
    }

    #[test]
    fn terminal_state_initial_title() {
        let ts = TerminalState::new(24, 80);
        assert_eq!(ts.title(), "");
    }

    #[test]
    fn terminal_state_initial_mode() {
        let ts = TerminalState::new(24, 80);
        // default mode: cursor not visible initially (bit 0 unset)
        assert_eq!(ts.mode() & 1, 0, "cursor should not be visible initially");
    }

    #[test]
    fn terminal_state_get_all_text_empty() {
        let ts = TerminalState::new(4, 10);
        let text = ts.get_all_text();
        // freshly created terminal should have empty/blank text
        assert!(text.trim().is_empty());
    }

    #[test]
    fn terminal_state_get_text_range() {
        let ts = TerminalState::new(4, 10);
        let text = ts.get_text(0, 0, 0, 10);
        assert!(text.trim().is_empty());
    }

    #[test]
    fn terminal_state_get_cell() {
        let ts = TerminalState::new(4, 10);
        let cell = ts.get_cell(0, 0);
        // cell data should be non-empty (even for a blank cell it encodes something)
        // just verify it doesn't panic
        let _ = cell;
    }

    #[test]
    fn terminal_state_different_sizes() {
        for (rows, cols) in [(1, 1), (100, 200), (24, 80), (50, 132)] {
            let ts = TerminalState::new(rows, cols);
            assert_eq!(ts.rows(), rows);
            assert_eq!(ts.cols(), cols);
        }
    }

    // ---------------------------------------------------------------
    // 5. parse_server_msg() round-trips
    // ---------------------------------------------------------------

    #[test]
    fn parse_update_round_trip() {
        let mut data = vec![remote::S2C_UPDATE];
        data.extend_from_slice(&42u16.to_le_bytes());
        data.extend_from_slice(b"payload data");
        let msg = parse_server_msg(&data).unwrap();
        assert_eq!(msg.kind, remote::S2C_UPDATE);
        assert_eq!(msg.pty_id, 42);
        assert_eq!(msg.payload, b"payload data");
    }

    #[test]
    fn parse_created_round_trip() {
        let mut data = vec![remote::S2C_CREATED];
        data.extend_from_slice(&7u16.to_le_bytes());
        data.extend_from_slice(b"shell");
        let msg = parse_server_msg(&data).unwrap();
        assert_eq!(msg.kind, remote::S2C_CREATED);
        assert_eq!(msg.pty_id, 7);
        assert_eq!(msg.tag, "shell");
    }

    #[test]
    fn parse_closed_round_trip() {
        let mut data = vec![remote::S2C_CLOSED];
        data.extend_from_slice(&99u16.to_le_bytes());
        let msg = parse_server_msg(&data).unwrap();
        assert_eq!(msg.kind, remote::S2C_CLOSED);
        assert_eq!(msg.pty_id, 99);
    }

    #[test]
    fn parse_list_round_trip() {
        let mut data = vec![remote::S2C_LIST];
        data.extend_from_slice(&2u16.to_le_bytes()); // count
        // entry 1: pty_id=1, tag="a"
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes()); // tag_len
        data.push(b'a');
        // entry 2: pty_id=2, tag="bc"
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes()); // tag_len
        data.extend_from_slice(b"bc");
        let msg = parse_server_msg(&data).unwrap();
        assert_eq!(msg.kind, remote::S2C_LIST);
        assert_eq!(msg.pty_ids, vec![1, 2]);
        assert_eq!(msg.tags, vec!["a", "bc"]);
    }

    #[test]
    fn parse_title_round_trip() {
        let mut data = vec![remote::S2C_TITLE];
        data.extend_from_slice(&5u16.to_le_bytes());
        data.extend_from_slice(b"my title");
        let msg = parse_server_msg(&data).unwrap();
        assert_eq!(msg.kind, remote::S2C_TITLE);
        assert_eq!(msg.pty_id, 5);
        assert_eq!(msg.payload, b"my title");
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_server_msg(&[]).is_none());
    }

    #[test]
    fn parse_too_short_returns_none() {
        // S2C_UPDATE needs at least 3 bytes (kind + pty_id)
        assert!(parse_server_msg(&[remote::S2C_UPDATE]).is_none());
        assert!(parse_server_msg(&[remote::S2C_UPDATE, 0]).is_none());
    }

    #[test]
    fn parse_unknown_kind_returns_none() {
        assert!(parse_server_msg(&[0xFF, 0, 0]).is_none());
    }

    #[test]
    fn parse_list_empty_entries() {
        let mut data = vec![remote::S2C_LIST];
        data.extend_from_slice(&0u16.to_le_bytes()); // count = 0
        let msg = parse_server_msg(&data).unwrap();
        assert_eq!(msg.kind, remote::S2C_LIST);
        assert!(msg.pty_ids.is_empty());
        assert!(msg.tags.is_empty());
    }

    #[test]
    fn parse_search_results_empty() {
        let mut data = vec![remote::S2C_SEARCH_RESULTS];
        data.extend_from_slice(&1u16.to_le_bytes()); // request_id
        data.extend_from_slice(&0u16.to_le_bytes()); // count = 0
        let msg = parse_server_msg(&data).unwrap();
        assert_eq!(msg.kind, remote::S2C_SEARCH_RESULTS);
        assert_eq!(msg.request_id, 1);
        assert!(msg.search_results.is_empty());
        assert!(msg.pty_ids.is_empty());
    }

    #[test]
    fn parse_created_empty_tag() {
        let mut data = vec![remote::S2C_CREATED];
        data.extend_from_slice(&1u16.to_le_bytes());
        // no tag bytes
        let msg = parse_server_msg(&data).unwrap();
        assert_eq!(msg.kind, remote::S2C_CREATED);
        assert_eq!(msg.pty_id, 1);
        assert_eq!(msg.tag, "");
    }

    #[test]
    fn parse_update_empty_payload() {
        let mut data = vec![remote::S2C_UPDATE];
        data.extend_from_slice(&0u16.to_le_bytes());
        let msg = parse_server_msg(&data).unwrap();
        assert_eq!(msg.kind, remote::S2C_UPDATE);
        assert_eq!(msg.pty_id, 0);
        assert!(msg.payload.is_empty());
    }

    // ---------------------------------------------------------------
    // SearchResult::from_remote
    // ---------------------------------------------------------------

    #[test]
    fn search_result_from_remote_with_scroll() {
        let entry = remote::SearchResultEntry {
            pty_id: 5, score: 200, primary_source: 1, matched_sources: 0b110,
            scroll_offset: Some(42), context: b"match context",
        };
        let sr = SearchResult::from_remote(entry);
        assert_eq!(sr.pty_id, 5);
        assert_eq!(sr.score, 200);
        assert_eq!(sr.primary_source, 1);
        assert_eq!(sr.matched_sources, 0b110);
        assert_eq!(sr.scroll_offset, Some(42));
        assert_eq!(sr.context, "match context");
    }

    #[test]
    fn search_result_from_remote_without_scroll() {
        let entry = remote::SearchResultEntry {
            pty_id: 0, score: 0, primary_source: 0, matched_sources: 0,
            scroll_offset: None, context: b"",
        };
        let sr = SearchResult::from_remote(entry);
        assert_eq!(sr.scroll_offset, None);
        assert_eq!(sr.context, "");
    }

    #[test]
    fn search_result_from_remote_invalid_utf8() {
        let entry = remote::SearchResultEntry {
            pty_id: 1, score: 1, primary_source: 0, matched_sources: 0,
            scroll_offset: None, context: &[0xFF, 0xFE],
        };
        let sr = SearchResult::from_remote(entry);
        // from_utf8_lossy replaces invalid bytes with replacement character
        assert!(sr.context.contains('\u{FFFD}'));
    }
}
