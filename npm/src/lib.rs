#![allow(non_snake_case)]

use wasm_bindgen::prelude::*;

const CELL_SIZE: usize = 12;

// ── Protocol constants ──────────────────────────────────────────────
// Exposed as top-level getter functions for JS consumption.

// Client → Server
#[wasm_bindgen] pub fn C2S_INPUT() -> u8 { 0x00 }
#[wasm_bindgen] pub fn C2S_RESIZE() -> u8 { 0x01 }
#[wasm_bindgen] pub fn C2S_SCROLL() -> u8 { 0x02 }
#[wasm_bindgen] pub fn C2S_ACK() -> u8 { 0x03 }
#[wasm_bindgen] pub fn C2S_DISPLAY_RATE() -> u8 { 0x04 }
#[wasm_bindgen] pub fn C2S_CLIENT_METRICS() -> u8 { 0x05 }
#[wasm_bindgen] pub fn C2S_CREATE() -> u8 { 0x10 }
#[wasm_bindgen] pub fn C2S_FOCUS() -> u8 { 0x11 }
#[wasm_bindgen] pub fn C2S_CLOSE() -> u8 { 0x12 }

// Server → Client
#[wasm_bindgen] pub fn S2C_UPDATE() -> u8 { 0x00 }
#[wasm_bindgen] pub fn S2C_CREATED() -> u8 { 0x01 }
#[wasm_bindgen] pub fn S2C_CLOSED() -> u8 { 0x02 }
#[wasm_bindgen] pub fn S2C_LIST() -> u8 { 0x03 }
#[wasm_bindgen] pub fn S2C_TITLE() -> u8 { 0x04 }

// ── Protocol message builders ───────────────────────────────────────

#[wasm_bindgen]
pub fn msg_create(rows: u16, cols: u16) -> Vec<u8> {
    vec![
        0x10,
        (rows & 0xff) as u8, (rows >> 8) as u8,
        (cols & 0xff) as u8, (cols >> 8) as u8,
    ]
}

#[wasm_bindgen]
pub fn msg_input(pty_id: u16, data: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3 + data.len());
    msg.push(0x00);
    msg.push((pty_id & 0xff) as u8);
    msg.push((pty_id >> 8) as u8);
    msg.extend_from_slice(data);
    msg
}

#[wasm_bindgen]
pub fn msg_resize(pty_id: u16, rows: u16, cols: u16) -> Vec<u8> {
    vec![
        0x01,
        (pty_id & 0xff) as u8, (pty_id >> 8) as u8,
        (rows & 0xff) as u8, (rows >> 8) as u8,
        (cols & 0xff) as u8, (cols >> 8) as u8,
    ]
}

#[wasm_bindgen]
pub fn msg_focus(pty_id: u16) -> Vec<u8> {
    vec![
        0x11,
        (pty_id & 0xff) as u8, (pty_id >> 8) as u8,
    ]
}

#[wasm_bindgen]
pub fn msg_close(pty_id: u16) -> Vec<u8> {
    vec![
        0x12,
        (pty_id & 0xff) as u8, (pty_id >> 8) as u8,
    ]
}

#[wasm_bindgen]
pub fn msg_ack() -> Vec<u8> {
    vec![0x03]
}

#[wasm_bindgen]
pub fn msg_scroll(pty_id: u16, offset: u32) -> Vec<u8> {
    vec![
        0x02,
        (pty_id & 0xff) as u8, (pty_id >> 8) as u8,
        (offset & 0xff) as u8, ((offset >> 8) & 0xff) as u8,
        ((offset >> 16) & 0xff) as u8, ((offset >> 24) & 0xff) as u8,
    ]
}

#[wasm_bindgen]
pub fn msg_display_rate(fps: u16) -> Vec<u8> {
    vec![
        0x04,
        (fps & 0xff) as u8, (fps >> 8) as u8,
    ]
}

#[wasm_bindgen]
pub fn msg_client_metrics(backlog: u16, ack_ahead: u16, apply_ms_x10: u16) -> Vec<u8> {
    vec![
        0x05,
        (backlog & 0xff) as u8, (backlog >> 8) as u8,
        (ack_ahead & 0xff) as u8, (ack_ahead >> 8) as u8,
        (apply_ms_x10 & 0xff) as u8, (apply_ms_x10 >> 8) as u8,
    ]
}

// ── Server message parser ───────────────────────────────────────────

/// Parsed server message. Check `kind()` to determine the variant,
/// then access the relevant fields.
#[wasm_bindgen]
pub struct ServerMsg {
    kind: u8,
    pty_id: u16,
    payload: Vec<u8>,
}

#[wasm_bindgen]
impl ServerMsg {
    pub fn kind(&self) -> u8 { self.kind }
    pub fn pty_id(&self) -> u16 { self.pty_id }

    /// For S2C_UPDATE: the compressed frame payload to feed to Terminal.
    pub fn payload(&self) -> Vec<u8> { self.payload.clone() }

    /// For S2C_TITLE: the title string.
    pub fn title(&self) -> String {
        String::from_utf8_lossy(&self.payload).into_owned()
    }

    /// For S2C_LIST: array of pty IDs.
    pub fn pty_ids(&self) -> Vec<u16> {
        self.payload
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect()
    }
}

/// Parse a binary server message. Returns None for invalid messages.
#[wasm_bindgen]
pub fn parse_server_msg(data: &[u8]) -> Option<ServerMsg> {
    if data.is_empty() { return None; }
    match data[0] {
        0x00 => { // S2C_UPDATE
            if data.len() < 3 { return None; }
            let pty_id = u16::from_le_bytes([data[1], data[2]]);
            Some(ServerMsg { kind: data[0], pty_id, payload: data[3..].to_vec() })
        }
        0x01 | 0x02 => { // S2C_CREATED | S2C_CLOSED
            if data.len() < 3 { return None; }
            let pty_id = u16::from_le_bytes([data[1], data[2]]);
            Some(ServerMsg { kind: data[0], pty_id, payload: vec![] })
        }
        0x03 => { // S2C_LIST
            if data.len() < 3 { return None; }
            let count = u16::from_le_bytes([data[1], data[2]]) as usize;
            let end = (3 + count * 2).min(data.len());
            Some(ServerMsg { kind: data[0], pty_id: 0, payload: data[3..end].to_vec() })
        }
        0x04 => { // S2C_TITLE
            if data.len() < 3 { return None; }
            let pty_id = u16::from_le_bytes([data[1], data[2]]);
            Some(ServerMsg { kind: data[0], pty_id, payload: data[3..].to_vec() })
        }
        _ => None,
    }
}

// ── Terminal state machine ──────────────────────────────────────────

#[wasm_bindgen]
pub struct Terminal {
    rows: u16,
    cols: u16,
    cells: Vec<u8>,
    cursor_row: u16,
    cursor_col: u16,
    mode: u16,
}

#[wasm_bindgen]
impl Terminal {
    #[wasm_bindgen(constructor)]
    pub fn new(rows: u16, cols: u16) -> Self {
        let total = rows as usize * cols as usize;
        Terminal {
            rows,
            cols,
            cells: vec![0u8; total * CELL_SIZE],
            cursor_row: 0,
            cursor_col: 0,
            mode: 0,
        }
    }

    // ── Accessors ───────────────────────────────────────────────────

    #[wasm_bindgen(getter)] pub fn rows(&self) -> u16 { self.rows }
    #[wasm_bindgen(getter)] pub fn cols(&self) -> u16 { self.cols }
    #[wasm_bindgen(getter)] pub fn cursor_row(&self) -> u16 { self.cursor_row }
    #[wasm_bindgen(getter)] pub fn cursor_col(&self) -> u16 { self.cursor_col }
    pub fn cursor_visible(&self) -> bool { self.mode & 1 != 0 }
    pub fn app_cursor(&self) -> bool { self.mode & 2 != 0 }
    pub fn bracketed_paste(&self) -> bool { self.mode & 8 != 0 }
    pub fn mouse_mode(&self) -> u8 { ((self.mode >> 4) & 7) as u8 }
    pub fn mouse_encoding(&self) -> u8 { ((self.mode >> 7) & 3) as u8 }
    pub fn echo(&self) -> bool { self.mode & (1 << 9) != 0 }
    pub fn icanon(&self) -> bool { self.mode & (1 << 10) != 0 }

    // ── Feed compressed data ────────────────────────────────────────

    pub fn feed_compressed(&mut self, data: &[u8]) {
        let payload = match lz4_flex::decompress_size_prepended(data) {
            Ok(d) => d,
            Err(_) => return,
        };
        self.apply_payload(&payload);
    }

    pub fn feed_compressed_batch(&mut self, batch: &[u8]) {
        let mut off = 0usize;
        while off + 4 <= batch.len() {
            let len = u32::from_le_bytes([
                batch[off], batch[off + 1], batch[off + 2], batch[off + 3],
            ]) as usize;
            off += 4;
            if off + len > batch.len() { break; }
            if let Ok(payload) = lz4_flex::decompress_size_prepended(&batch[off..off + len]) {
                self.apply_payload(&payload);
            }
            off += len;
        }
    }

    fn apply_payload(&mut self, payload: &[u8]) {
        if payload.len() < 10 { return; }

        let new_rows = u16::from_le_bytes([payload[0], payload[1]]);
        let new_cols = u16::from_le_bytes([payload[2], payload[3]]);

        if new_rows != self.rows || new_cols != self.cols {
            self.rows = new_rows;
            self.cols = new_cols;
            let total = new_rows as usize * new_cols as usize;
            self.cells = vec![0u8; total * CELL_SIZE];
        }

        let total_cells = self.rows as usize * self.cols as usize;
        let bitmask_len = (total_cells + 7) / 8;
        if payload.len() < 10 + bitmask_len { return; }

        let bitmask = &payload[10..10 + bitmask_len];
        let data_start = 10 + bitmask_len;

        let dirty_count = (0..total_cells)
            .filter(|&i| bitmask[i / 8] & (1 << (i % 8)) != 0)
            .count();
        if payload.len() < data_start + dirty_count * CELL_SIZE { return; }

        let mut dirty_idx = 0usize;
        for i in 0..total_cells {
            if bitmask[i / 8] & (1 << (i % 8)) != 0 {
                let cell_idx = i * CELL_SIZE;
                for byte_pos in 0..CELL_SIZE {
                    self.cells[cell_idx + byte_pos] =
                        payload[data_start + byte_pos * dirty_count + dirty_idx];
                }
                dirty_idx += 1;
            }
        }

        self.cursor_row = u16::from_le_bytes([payload[4], payload[5]]);
        self.cursor_col = u16::from_le_bytes([payload[6], payload[7]]);
        self.mode = u16::from_le_bytes([payload[8], payload[9]]);
    }

    // ── Read terminal content ───────────────────────────────────────

    pub fn get_text(&self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> String {
        let mut result = String::new();
        for row in start_row..=end_row.min(self.rows.saturating_sub(1)) {
            let c0 = if row == start_row { start_col } else { 0 };
            let c1 = if row == end_row { end_col } else { self.cols - 1 };
            let mut line = String::new();
            let mut col = c0;
            while col <= c1.min(self.cols - 1) {
                let idx = (row as usize * self.cols as usize + col as usize) * CELL_SIZE;
                let f1 = self.cells[idx + 1];
                if f1 & 4 != 0 { col += 1; continue; }
                let content_len = ((f1 >> 3) & 7) as usize;
                if content_len > 0 {
                    if let Ok(s) = std::str::from_utf8(&self.cells[idx + 8..idx + 8 + content_len]) {
                        line.push_str(s);
                    }
                } else {
                    line.push(' ');
                }
                col += 1;
            }
            result.push_str(line.trim_end());
            if row < end_row.min(self.rows.saturating_sub(1)) { result.push('\n'); }
        }
        result
    }

    /// Get all text content as a single string.
    pub fn get_all_text(&self) -> String {
        if self.rows == 0 || self.cols == 0 { return String::new(); }
        self.get_text(0, 0, self.rows - 1, self.cols - 1)
    }

    /// Raw cell data for a given position. Returns 12 bytes or empty if out of bounds.
    pub fn get_cell(&self, row: u16, col: u16) -> Vec<u8> {
        if row >= self.rows || col >= self.cols { return vec![]; }
        let idx = (row as usize * self.cols as usize + col as usize) * CELL_SIZE;
        self.cells[idx..idx + CELL_SIZE].to_vec()
    }
}
