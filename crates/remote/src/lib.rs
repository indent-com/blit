use std::collections::BTreeMap;

use lz4_flex::{compress_prepend_size, decompress_size_prepended};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const CELL_SIZE: usize = 12;
const TITLE_PRESENT: u16 = 1 << 15;
const OPS_PRESENT: u16 = 1 << 14;
const STRINGS_PRESENT: u16 = 1 << 13;
const LINE_FLAGS_PRESENT: u16 = 1 << 12;
const TITLE_LEN_MASK: u16 = LINE_FLAGS_PRESENT - 1;

/// Per-row flag: this row's content continues on the next row (line wrap).
pub const ROW_FLAG_WRAPPED: u8 = 1 << 0;

/// Sentinel value for content_len indicating the cell's text lives in the
/// overflow string table.  Bytes 8-11 then hold an FNV-1a hash of the full
/// UTF-8 string (for diff correctness), and the actual string is stored in
/// `FrameState::overflow` keyed by cell index.
const CONTENT_OVERFLOW: u8 = 7;

const ENABLE_SCROLL_OPS: bool = true;
const MODE_ECHO: u16 = 1 << 9;
const MODE_ICANON: u16 = 1 << 10;

const OP_COPY_RECT: u8 = 0x01;
const OP_FILL_RECT: u8 = 0x02;
const OP_PATCH_CELLS: u8 = 0x03;

pub const C2S_INPUT: u8 = 0x00;
/// Desired viewport size(s): [0x01][pty_id:2][rows:2][cols:2]...
/// Clients may batch multiple PTY resize entries in one message. The server
/// mediates these per-client desired sizes into each PTY's effective size.
/// A `rows, cols` pair of `0, 0` clears this client's desired size for that PTY.
pub const C2S_RESIZE: u8 = 0x01;
pub const C2S_SCROLL: u8 = 0x02;
pub const C2S_ACK: u8 = 0x03;
pub const C2S_DISPLAY_RATE: u8 = 0x04;
pub const C2S_CLIENT_METRICS: u8 = 0x05;
/// Mouse event: [0x06][pty_id:2][type:1][button:1][col:2][row:2]
/// type: 0=down, 1=up, 2=move
/// button: 0=left, 1=mid, 2=right, 3=release, 64=wheel_up, 65=wheel_down
/// The server generates the correct escape sequence based on mouse_mode and mouse_encoding.
pub const C2S_MOUSE: u8 = 0x06;
/// Restart an exited PTY: [0x07][pty_id:2]
/// Server spawns a new shell in the same PTY slot, preserving the pty_id.
pub const C2S_RESTART: u8 = 0x07;
pub const C2S_CREATE: u8 = 0x10;
pub const C2S_FOCUS: u8 = 0x11;
pub const C2S_CLOSE: u8 = 0x12;
pub const C2S_SUBSCRIBE: u8 = 0x13;
pub const C2S_UNSUBSCRIBE: u8 = 0x14;
pub const C2S_SEARCH: u8 = 0x15;
pub const C2S_CREATE_AT: u8 = 0x16;
pub const C2S_CREATE_N: u8 = 0x17;
/// Generic create: [0x18][nonce:2][rows:2][cols:2][features:1][tag_len:2][tag:N][...optional fields]
/// Features: bit 0 = has src_pty_id (2 bytes after tag), bit 1 = has command (remaining bytes after src_pty_id if present)
/// Server responds with S2C_CREATED_N using the same nonce.
pub const C2S_CREATE2: u8 = 0x18;
pub const CREATE2_HAS_SRC_PTY: u8 = 1 << 0;
pub const CREATE2_HAS_COMMAND: u8 = 1 << 1;
/// Read text from a PTY's scrollback + viewport: [0x19][nonce:2][pty_id:2][offset:4][limit:4][flags:1]
/// offset: number of lines to skip from the top (oldest = 0), or from the end if READ_TAIL is set
/// limit: max lines to return (0 = all)
/// flags: bit 0 = include ANSI styling, bit 1 = offset counts from the end
/// Server responds with S2C_TEXT using the same nonce.
pub const C2S_READ: u8 = 0x19;
pub const READ_ANSI: u8 = 1 << 0;
pub const READ_TAIL: u8 = 1 << 1;
/// Copy text from a range of absolute row/col positions in scrollback + viewport:
/// [0x1B][nonce:2][pty_id:2][start_tail:4][start_col:2][end_tail:4][end_col:2][flags:1]
/// start_tail/end_tail: physical row distance from the bottom (0 = last row).
/// start is the earlier position (closer to top), so start_tail >= end_tail.
/// flags: reserved (0 for now).
/// Server responds with S2C_TEXT using the same nonce.
pub const C2S_COPY_RANGE: u8 = 0x1B;
/// Send a signal to a PTY's session leader: [0x1A][pty_id:2][signal:4]
/// signal is a raw libc signal number (e.g. SIGTERM=15, SIGKILL=9).
pub const C2S_KILL: u8 = 0x1A;

pub const S2C_UPDATE: u8 = 0x00;
pub const S2C_CREATED: u8 = 0x01;
pub const S2C_CLOSED: u8 = 0x02;
pub const S2C_LIST: u8 = 0x03;
pub const S2C_TITLE: u8 = 0x04;
pub const S2C_SEARCH_RESULTS: u8 = 0x05;
pub const S2C_CREATED_N: u8 = 0x06;
pub const S2C_HELLO: u8 = 0x07;
/// The PTY's subprocess has exited but the terminal state is retained.
/// Clients can still read/scroll the last frame. Send C2S_CLOSE to dismiss.
/// Wire: [0x08][pty_id:2][exit_status:4]
/// exit_status: WEXITSTATUS if normal exit, negative signal number if signalled,
///              EXIT_STATUS_UNKNOWN if not yet collected.
pub const S2C_EXITED: u8 = 0x08;
pub const EXIT_STATUS_UNKNOWN: i32 = i32::MIN;
/// Sent after the initial burst (HELLO, LIST, TITLE*, EXITED*) is complete.
/// Clients can use this to know when the initial state has been fully transmitted.
pub const S2C_READY: u8 = 0x09;
/// Text response: [0x0A][nonce:2][pty_id:2][total_lines:4][offset:4][text:N]
/// nonce: echoed from C2S_READ request
/// total_lines: total available lines (scrollback + viewport rows)
/// offset: the offset that was requested
/// text: UTF-8 text, lines separated by \n
pub const S2C_TEXT: u8 = 0x0A;

pub const FEATURE_CREATE_NONCE: u32 = 1 << 0;
pub const FEATURE_RESTART: u32 = 1 << 1;
pub const FEATURE_RESIZE_BATCH: u32 = 1 << 2;
pub const FEATURE_COPY_RANGE: u32 = 1 << 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub row: u16,
    pub col: u16,
    pub rows: u16,
    pub cols: u16,
}

impl Rect {
    pub const fn new(row: u16, col: u16, rows: u16, cols: u16) -> Self {
        Self {
            row,
            col,
            rows,
            cols,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrameState {
    rows: u16,
    cols: u16,
    cells: Vec<u8>,
    cursor_row: u16,
    cursor_col: u16,
    mode: u16,
    title: String,
    /// Overflow strings for cells whose content exceeds 4 bytes.
    /// Keyed by flat cell index (row * cols + col).
    overflow: BTreeMap<usize, String>,
    /// Per-row flags. `ROW_FLAG_WRAPPED` means the row continues on the next.
    line_flags: Vec<u8>,
    /// Total scrollback lines available for this PTY.
    scrollback_lines: u32,
}

impl FrameState {
    pub fn new(rows: u16, cols: u16) -> Self {
        let total = rows as usize * cols as usize;
        Self {
            rows,
            cols,
            cells: vec![0; total * CELL_SIZE],
            cursor_row: 0,
            cursor_col: 0,
            mode: 0,
            title: String::new(),
            overflow: BTreeMap::new(),
            line_flags: vec![0; rows as usize],
            scrollback_lines: 0,
        }
    }

    pub fn from_parts(
        rows: u16,
        cols: u16,
        cursor_row: u16,
        cursor_col: u16,
        mode: u16,
        title: impl Into<String>,
        cells: Vec<u8>,
    ) -> Self {
        let mut state = Self::new(rows, cols);
        if cells.len() == state.cells.len() {
            state.cells = cells;
        }
        state.cursor_row = cursor_row;
        state.cursor_col = cursor_col;
        state.mode = mode;
        state.title = title.into();
        state
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn cursor_row(&self) -> u16 {
        self.cursor_row
    }

    pub fn cursor_col(&self) -> u16 {
        self.cursor_col
    }

    pub fn mode(&self) -> u16 {
        self.mode
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn cells(&self) -> &[u8] {
        &self.cells
    }

    pub fn cells_mut(&mut self) -> &mut [u8] {
        &mut self.cells
    }

    pub fn overflow(&self) -> &BTreeMap<usize, String> {
        &self.overflow
    }

    pub fn overflow_mut(&mut self) -> &mut BTreeMap<usize, String> {
        &mut self.overflow
    }

    pub fn line_flags(&self) -> &[u8] {
        &self.line_flags
    }

    pub fn line_flags_mut(&mut self) -> &mut Vec<u8> {
        &mut self.line_flags
    }

    pub fn scrollback_lines(&self) -> u32 {
        self.scrollback_lines
    }

    pub fn set_scrollback_lines(&mut self, lines: u32) {
        self.scrollback_lines = lines;
    }

    pub fn is_wrapped(&self, row: u16) -> bool {
        self.line_flags.get(row as usize).copied().unwrap_or(0) & ROW_FLAG_WRAPPED != 0
    }

    pub fn set_wrapped(&mut self, row: u16, wrapped: bool) {
        if let Some(flags) = self.line_flags.get_mut(row as usize) {
            if wrapped {
                *flags |= ROW_FLAG_WRAPPED;
            } else {
                *flags &= !ROW_FLAG_WRAPPED;
            }
        }
    }

    /// Returns the text content of a cell, resolving overflow if needed.
    pub fn cell_content(&self, row: u16, col: u16) -> &str {
        if row >= self.rows || col >= self.cols {
            return "";
        }
        let flat = row as usize * self.cols as usize + col as usize;
        let idx = flat * CELL_SIZE;
        let f1 = self.cells[idx + 1];
        if f1 & 4 != 0 {
            return ""; // wide continuation
        }
        let content_len = ((f1 >> 3) & 7) as usize;
        if content_len == CONTENT_OVERFLOW as usize {
            if let Some(s) = self.overflow.get(&flat) {
                return s.as_str();
            }
            return "";
        }
        if content_len == 0 {
            return " ";
        }
        std::str::from_utf8(&self.cells[idx + 8..idx + 8 + content_len]).unwrap_or(" ")
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.cells = vec![0; rows as usize * cols as usize * CELL_SIZE];
        self.overflow.clear();
        self.line_flags = vec![0; rows as usize];
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
    }

    pub fn set_cursor(&mut self, row: u16, col: u16) {
        self.cursor_row = row.min(self.rows.saturating_sub(1));
        self.cursor_col = col.min(self.cols.saturating_sub(1));
    }

    pub fn set_mode(&mut self, mode: u16) {
        self.mode = mode;
    }

    pub fn set_title(&mut self, title: impl Into<String>) -> bool {
        let title = title.into();
        if self.title == title {
            return false;
        }
        self.title = title;
        true
    }

    pub fn clear(&mut self, style: CellStyle) {
        for row in 0..self.rows {
            for col in 0..self.cols {
                self.set_blank_cell(row, col, style);
            }
        }
    }

    pub fn fill_rect(&mut self, rect: Rect, ch: char, style: CellStyle) {
        let row_end = rect.row.saturating_add(rect.rows).min(self.rows);
        let col_end = rect.col.saturating_add(rect.cols).min(self.cols);
        for row in rect.row..row_end {
            let mut col = rect.col;
            while col < col_end {
                let width = self.set_cell(row, col, ch, style);
                if width == 0 {
                    break;
                }
                col = col.saturating_add(width);
            }
        }
    }

    pub fn write_text(&mut self, row: u16, col: u16, text: &str, style: CellStyle) -> u16 {
        if row >= self.rows || col >= self.cols {
            return col;
        }
        let mut cur_col = col;
        for ch in text.chars() {
            if cur_col >= self.cols {
                break;
            }
            let width = self.set_cell(row, cur_col, ch, style);
            if width == 0 {
                continue;
            }
            cur_col = cur_col.saturating_add(width);
        }
        cur_col
    }

    pub fn write_wrapped_text(&mut self, rect: Rect, text: &str, style: CellStyle) -> usize {
        if rect.rows == 0 || rect.cols == 0 {
            return 0;
        }
        let lines = wrap_text_lines(text, rect.cols as usize);
        let max_rows = rect.rows.min(self.rows.saturating_sub(rect.row));
        for (idx, line) in lines.iter().take(max_rows as usize).enumerate() {
            let row = rect.row + idx as u16;
            self.write_text(row, rect.col, line, style);
        }
        lines.len()
    }

    pub fn write_scrolling_text<S: AsRef<str>>(
        &mut self,
        rect: Rect,
        lines: &[S],
        offset_from_bottom: usize,
        style: CellStyle,
    ) {
        if rect.rows == 0 || rect.cols == 0 {
            return;
        }
        let mut wrapped = Vec::with_capacity(lines.len());
        for line in lines {
            let line = line.as_ref();
            let out = wrap_text_lines(line, rect.cols as usize);
            if out.is_empty() {
                wrapped.push(String::new());
            } else {
                wrapped.extend(out);
            }
        }
        let visible = rect.rows as usize;
        let end = wrapped.len().saturating_sub(offset_from_bottom);
        let start = end.saturating_sub(visible);
        for row in 0..rect.rows {
            self.fill_rect(
                Rect::new(rect.row + row, rect.col, 1, rect.cols),
                ' ',
                style,
            );
        }
        for (idx, line) in wrapped[start..end].iter().enumerate() {
            self.write_text(rect.row + idx as u16, rect.col, line, style);
        }
    }

    pub fn get_text(&self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> String {
        let mut result = String::new();
        if self.rows == 0 || self.cols == 0 {
            return result;
        }
        for row in start_row..=end_row.min(self.rows.saturating_sub(1)) {
            let c0 = if row == start_row { start_col } else { 0 };
            let c1 = if row == end_row {
                end_col
            } else {
                self.cols - 1
            };
            let mut line = String::new();
            let mut col = c0;
            while col <= c1.min(self.cols - 1) {
                line.push_str(self.cell_content(row, col));
                col += 1;
            }
            result.push_str(line.trim_end());
            if row < end_row.min(self.rows.saturating_sub(1)) && !self.is_wrapped(row) {
                result.push('\n');
            }
        }
        result
    }

    pub fn get_all_text(&self) -> String {
        if self.rows == 0 || self.cols == 0 {
            return String::new();
        }
        self.get_text(0, 0, self.rows - 1, self.cols - 1)
    }

    fn cell_style(&self, row: u16, col: u16) -> CellStyle {
        if row >= self.rows || col >= self.cols {
            return CellStyle::default();
        }
        let idx = self.cell_offset(row, col);
        let f0 = self.cells[idx];
        let f1 = self.cells[idx + 1];
        let fg_type = f0 & 3;
        let bg_type = (f0 >> 2) & 3;
        let fg = match fg_type {
            1 => Color::Indexed(self.cells[idx + 2]),
            2 => Color::Rgb(
                self.cells[idx + 2],
                self.cells[idx + 3],
                self.cells[idx + 4],
            ),
            _ => Color::Default,
        };
        let bg = match bg_type {
            1 => Color::Indexed(self.cells[idx + 5]),
            2 => Color::Rgb(
                self.cells[idx + 5],
                self.cells[idx + 6],
                self.cells[idx + 7],
            ),
            _ => Color::Default,
        };
        CellStyle {
            fg,
            bg,
            bold: (f0 >> 4) & 1 != 0,
            dim: (f0 >> 5) & 1 != 0,
            italic: (f0 >> 6) & 1 != 0,
            underline: (f0 >> 7) & 1 != 0,
            inverse: f1 & 1 != 0,
        }
    }

    pub fn get_ansi_text(&self) -> String {
        if self.rows == 0 || self.cols == 0 {
            return String::new();
        }
        let mut result = String::new();
        let mut cur_style = CellStyle::default();
        for row in 0..self.rows {
            let mut line = String::new();
            let mut col = 0u16;
            while col < self.cols {
                let style = self.cell_style(row, col);
                if style != cur_style {
                    push_sgr(&mut line, &style);
                    cur_style = style;
                }
                line.push_str(self.cell_content(row, col));
                col += 1;
            }
            let trimmed = line.trim_end();
            result.push_str(trimmed);
            if cur_style != CellStyle::default() {
                result.push_str("\x1b[0m");
                cur_style = CellStyle::default();
            }
            if row < self.rows - 1 {
                result.push('\n');
            }
        }
        result
    }

    pub fn get_cell(&self, row: u16, col: u16) -> Vec<u8> {
        if row >= self.rows || col >= self.cols {
            return Vec::new();
        }
        let idx = self.cell_offset(row, col);
        self.cells[idx..idx + CELL_SIZE].to_vec()
    }

    fn cell_offset(&self, row: u16, col: u16) -> usize {
        (row as usize * self.cols as usize + col as usize) * CELL_SIZE
    }

    fn set_cell(&mut self, row: u16, col: u16, ch: char, style: CellStyle) -> u16 {
        if row >= self.rows || col >= self.cols {
            return 0;
        }
        let raw_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if raw_width == 0 {
            return 0;
        }
        let width = if raw_width > 1 && col + 1 < self.cols {
            2
        } else {
            1
        };
        let idx = self.cell_offset(row, col);
        encode_cell(
            &mut self.cells[idx..idx + CELL_SIZE],
            Some(ch),
            style,
            width == 2,
            false,
        );
        if width == 2 {
            let cont_idx = self.cell_offset(row, col + 1);
            encode_cell(
                &mut self.cells[cont_idx..cont_idx + CELL_SIZE],
                None,
                style,
                false,
                true,
            );
        }
        width
    }

    fn set_blank_cell(&mut self, row: u16, col: u16, style: CellStyle) {
        if row >= self.rows || col >= self.cols {
            return;
        }
        let idx = self.cell_offset(row, col);
        encode_cell(
            &mut self.cells[idx..idx + CELL_SIZE],
            None,
            style,
            false,
            false,
        );
    }
}

#[derive(Clone, Debug)]
pub struct TerminalState {
    frame: FrameState,
}

impl TerminalState {
    pub fn new(rows: u16, cols: u16) -> Self {
        let frame = FrameState::new(rows, cols);
        Self { frame }
    }

    pub fn frame(&self) -> &FrameState {
        &self.frame
    }

    pub fn frame_mut(&mut self) -> &mut FrameState {
        &mut self.frame
    }

    pub fn title(&self) -> &str {
        self.frame.title()
    }

    pub fn rows(&self) -> u16 {
        self.frame.rows()
    }

    pub fn cols(&self) -> u16 {
        self.frame.cols()
    }

    pub fn is_wrapped(&self, row: u16) -> bool {
        self.frame.is_wrapped(row)
    }

    pub fn cursor_row(&self) -> u16 {
        self.frame.cursor_row()
    }

    pub fn cursor_col(&self) -> u16 {
        self.frame.cursor_col()
    }

    pub fn mode(&self) -> u16 {
        self.frame.mode()
    }

    pub fn cells(&self) -> &[u8] {
        self.frame.cells()
    }

    pub fn set_title(&mut self, title: &str) -> bool {
        self.frame.set_title(title.to_owned())
    }

    pub fn get_text(&self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> String {
        self.frame.get_text(start_row, start_col, end_row, end_col)
    }

    pub fn get_all_text(&self) -> String {
        self.frame.get_all_text()
    }

    pub fn get_ansi_text(&self) -> String {
        self.frame.get_ansi_text()
    }

    pub fn get_cell(&self, row: u16, col: u16) -> Vec<u8> {
        self.frame.get_cell(row, col)
    }

    pub fn feed_compressed(&mut self, data: &[u8]) -> bool {
        let payload = match decompress_size_prepended(data) {
            Ok(d) => d,
            Err(_) => return false,
        };
        self.apply_payload(&payload)
    }

    pub fn feed_compressed_batch(&mut self, batch: &[u8]) -> bool {
        let mut changed = false;
        let mut off = 0usize;
        while off + 4 <= batch.len() {
            let len =
                u32::from_le_bytes([batch[off], batch[off + 1], batch[off + 2], batch[off + 3]])
                    as usize;
            off += 4;
            if off + len > batch.len() {
                break;
            }
            if let Ok(payload) = decompress_size_prepended(&batch[off..off + len]) {
                changed |= self.apply_payload(&payload);
            }
            off += len;
        }
        changed
    }

    fn apply_payload(&mut self, payload: &[u8]) -> bool {
        if payload.len() < 12 {
            return false;
        }

        let new_rows = u16::from_le_bytes([payload[0], payload[1]]);
        let new_cols = u16::from_le_bytes([payload[2], payload[3]]);
        let new_cursor_row = u16::from_le_bytes([payload[4], payload[5]]);
        let new_cursor_col = u16::from_le_bytes([payload[6], payload[7]]);
        let new_mode = u16::from_le_bytes([payload[8], payload[9]]);
        let title_field = u16::from_le_bytes([payload[10], payload[11]]);
        let title_present = title_field & TITLE_PRESENT != 0;
        let ops_present = title_field & OPS_PRESENT != 0;
        let strings_present = title_field & STRINGS_PRESENT != 0;
        let line_flags_present = title_field & LINE_FLAGS_PRESENT != 0;
        let title_len = (title_field & TITLE_LEN_MASK) as usize;

        let title_start = 12usize;
        let title_end = title_start.saturating_add(title_len);
        if payload.len() < title_end {
            return false;
        }
        let title_changed = if title_present {
            let title = String::from_utf8_lossy(&payload[title_start..title_end]).into_owned();
            self.frame.set_title(title)
        } else {
            false
        };

        let resized = new_rows != self.frame.rows || new_cols != self.frame.cols;
        if resized {
            self.frame.resize(new_rows, new_cols);
        }

        let old_cursor_row = self.frame.cursor_row;
        let old_cursor_col = self.frame.cursor_col;
        let old_mode = self.frame.mode;

        let (content_changed, ops_end) = if ops_present {
            let ops_start = title_end;
            if payload.len() < ops_start + 2 {
                return false;
            }
            let (changed, consumed) = self
                .apply_ops_payload(&payload[ops_start..])
                .unwrap_or((false, 0));
            (changed, ops_start + consumed)
        } else {
            let (changed, consumed) = self
                .apply_legacy_patch_payload(&payload[title_end..])
                .unwrap_or((false, 0));
            (changed, title_end + consumed)
        };

        let mut after_strings = ops_end;
        if strings_present {
            after_strings = self.apply_overflow_strings(&payload[ops_end..]);
            after_strings += ops_end;
        }

        let (line_flags_changed, after_line_flags) = if line_flags_present {
            let lf_start = after_strings;
            let lf_end = lf_start + new_rows as usize;
            if payload.len() >= lf_end {
                let new_flags = &payload[lf_start..lf_end];
                let changed = self.frame.line_flags != new_flags;
                self.frame.line_flags.clear();
                self.frame.line_flags.extend_from_slice(new_flags);
                (changed, lf_end)
            } else {
                (false, after_strings)
            }
        } else {
            (false, after_strings)
        };

        // Trailing scrollback count (backward-compatible extension).
        if payload.len() >= after_line_flags + 4 {
            self.frame.scrollback_lines = u32::from_le_bytes([
                payload[after_line_flags],
                payload[after_line_flags + 1],
                payload[after_line_flags + 2],
                payload[after_line_flags + 3],
            ]);
        }

        self.frame.cursor_row = new_cursor_row.min(self.frame.rows.saturating_sub(1));
        self.frame.cursor_col = new_cursor_col.min(self.frame.cols.saturating_sub(1));
        self.frame.mode = new_mode;
        resized
            || title_changed
            || content_changed
            || line_flags_changed
            || new_cursor_row != old_cursor_row
            || new_cursor_col != old_cursor_col
            || new_mode != old_mode
    }

    fn apply_legacy_patch_payload(&mut self, payload: &[u8]) -> Option<(bool, usize)> {
        let total_cells = self.frame.rows as usize * self.frame.cols as usize;
        let bitmask_len = total_cells.div_ceil(8);
        if payload.len() < bitmask_len {
            return None;
        }
        let bitmask = &payload[..bitmask_len];
        let dirty_count = (0..total_cells)
            .filter(|&i| bitmask[i / 8] & (1 << (i % 8)) != 0)
            .count();
        let data = &payload[bitmask_len..];
        if data.len() < dirty_count * CELL_SIZE {
            return None;
        }
        self.apply_patch_cells(bitmask, &data[..dirty_count * CELL_SIZE], dirty_count);
        Some((dirty_count > 0, bitmask_len + dirty_count * CELL_SIZE))
    }

    fn apply_ops_payload(&mut self, payload: &[u8]) -> Option<(bool, usize)> {
        if payload.len() < 2 {
            return None;
        }
        let op_count = u16::from_le_bytes([payload[0], payload[1]]) as usize;
        let total_cells = self.frame.rows as usize * self.frame.cols as usize;
        let bitmask_len = total_cells.div_ceil(8);
        let mut off = 2usize;
        let mut changed = false;

        for _ in 0..op_count {
            if off >= payload.len() {
                return None;
            }
            let op = payload[off];
            off += 1;
            match op {
                OP_COPY_RECT => {
                    if payload.len() < off + 12 {
                        return None;
                    }
                    let src_row = u16::from_le_bytes([payload[off], payload[off + 1]]);
                    let src_col = u16::from_le_bytes([payload[off + 2], payload[off + 3]]);
                    let dst_row = u16::from_le_bytes([payload[off + 4], payload[off + 5]]);
                    let dst_col = u16::from_le_bytes([payload[off + 6], payload[off + 7]]);
                    let rows = u16::from_le_bytes([payload[off + 8], payload[off + 9]]);
                    let cols = u16::from_le_bytes([payload[off + 10], payload[off + 11]]);
                    off += 12;
                    changed |= self.apply_copy_rect(src_row, src_col, dst_row, dst_col, rows, cols);
                }
                OP_FILL_RECT => {
                    if payload.len() < off + 8 + CELL_SIZE {
                        return None;
                    }
                    let row = u16::from_le_bytes([payload[off], payload[off + 1]]);
                    let col = u16::from_le_bytes([payload[off + 2], payload[off + 3]]);
                    let rows = u16::from_le_bytes([payload[off + 4], payload[off + 5]]);
                    let cols = u16::from_le_bytes([payload[off + 6], payload[off + 7]]);
                    off += 8;
                    let mut cell = [0u8; CELL_SIZE];
                    cell.copy_from_slice(&payload[off..off + CELL_SIZE]);
                    off += CELL_SIZE;
                    changed |= self.apply_fill_rect(row, col, rows, cols, &cell);
                }
                OP_PATCH_CELLS => {
                    if payload.len() < off + bitmask_len {
                        return None;
                    }
                    let bitmask = &payload[off..off + bitmask_len];
                    off += bitmask_len;
                    let dirty_count = (0..total_cells)
                        .filter(|&i| bitmask[i / 8] & (1 << (i % 8)) != 0)
                        .count();
                    if payload.len() < off + dirty_count * CELL_SIZE {
                        return None;
                    }
                    self.apply_patch_cells(
                        bitmask,
                        &payload[off..off + dirty_count * CELL_SIZE],
                        dirty_count,
                    );
                    off += dirty_count * CELL_SIZE;
                    changed |= dirty_count > 0;
                }
                _ => return None,
            }
        }

        Some((changed, off))
    }

    fn apply_patch_cells(&mut self, bitmask: &[u8], data: &[u8], dirty_count: usize) {
        let total_cells = self.frame.rows as usize * self.frame.cols as usize;
        let mut dirty_idx = 0usize;
        for i in 0..total_cells {
            if bitmask[i / 8] & (1 << (i % 8)) == 0 {
                continue;
            }
            let cell_idx = i * CELL_SIZE;
            for byte_pos in 0..CELL_SIZE {
                self.frame.cells[cell_idx + byte_pos] = data[byte_pos * dirty_count + dirty_idx];
            }
            // Remove stale overflow entry when a cell is updated — it may
            // have transitioned from overflow (content_len=7) to inline.
            let new_content_len = (self.frame.cells[cell_idx + 1] >> 3) & 7;
            if new_content_len != CONTENT_OVERFLOW {
                self.frame.overflow.remove(&i);
            }
            dirty_idx += 1;
        }
    }

    fn apply_copy_rect(
        &mut self,
        src_row: u16,
        src_col: u16,
        dst_row: u16,
        dst_col: u16,
        rows: u16,
        cols: u16,
    ) -> bool {
        let rows = rows
            .min(self.frame.rows.saturating_sub(src_row))
            .min(self.frame.rows.saturating_sub(dst_row));
        let cols = cols
            .min(self.frame.cols.saturating_sub(src_col))
            .min(self.frame.cols.saturating_sub(dst_col));
        if rows == 0 || cols == 0 {
            return false;
        }

        let frame_cols = self.frame.cols as usize;

        // Copy overflow strings for the source region.
        let mut overflow_temp: Vec<(usize, String)> = Vec::new();
        for r in 0..rows as usize {
            for c in 0..cols as usize {
                let src_flat = (src_row as usize + r) * frame_cols + src_col as usize + c;
                if let Some(s) = self.frame.overflow.get(&src_flat) {
                    let dst_flat = (dst_row as usize + r) * frame_cols + dst_col as usize + c;
                    overflow_temp.push((dst_flat, s.clone()));
                }
            }
        }

        let mut temp = vec![0u8; rows as usize * cols as usize * CELL_SIZE];
        for r in 0..rows as usize {
            let src_off = self.frame.cell_offset(src_row + r as u16, src_col);
            let src_end = src_off + cols as usize * CELL_SIZE;
            let dst_off = r * cols as usize * CELL_SIZE;
            temp[dst_off..dst_off + cols as usize * CELL_SIZE]
                .copy_from_slice(&self.frame.cells[src_off..src_end]);
        }
        for r in 0..rows as usize {
            let dst_off = self.frame.cell_offset(dst_row + r as u16, dst_col);
            let dst_end = dst_off + cols as usize * CELL_SIZE;
            let src_off = r * cols as usize * CELL_SIZE;
            self.frame.cells[dst_off..dst_end]
                .copy_from_slice(&temp[src_off..src_off + cols as usize * CELL_SIZE]);
        }

        for r in 0..rows as usize {
            for c in 0..cols as usize {
                let dst_flat = (dst_row as usize + r) * frame_cols + dst_col as usize + c;
                self.frame.overflow.remove(&dst_flat);
            }
        }
        for (idx, s) in overflow_temp {
            self.frame.overflow.insert(idx, s);
        }

        true
    }

    fn apply_fill_rect(
        &mut self,
        row: u16,
        col: u16,
        rows: u16,
        cols: u16,
        cell: &[u8; CELL_SIZE],
    ) -> bool {
        let row_end = row.saturating_add(rows).min(self.frame.rows);
        let col_end = col.saturating_add(cols).min(self.frame.cols);
        // Fill cells never have overflow content — clear stale entries.
        let frame_cols = self.frame.cols as usize;
        for r in row..row_end {
            for c in col..col_end {
                self.frame
                    .overflow
                    .remove(&(r as usize * frame_cols + c as usize));
            }
        }
        if row >= row_end || col >= col_end {
            return false;
        }
        for r in row..row_end {
            for c in col..col_end {
                let off = self.frame.cell_offset(r, c);
                self.frame.cells[off..off + CELL_SIZE].copy_from_slice(cell);
            }
        }
        true
    }

    fn apply_overflow_strings(&mut self, data: &[u8]) -> usize {
        if data.len() < 2 {
            return 0;
        }
        let count = u16::from_le_bytes([data[0], data[1]]) as usize;
        let mut off = 2usize;
        for _ in 0..count {
            if off + 6 > data.len() {
                break;
            }
            let cell_idx =
                u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                    as usize;
            let len = u16::from_le_bytes([data[off + 4], data[off + 5]]) as usize;
            off += 6;
            if off + len > data.len() {
                break;
            }
            if let Ok(s) = std::str::from_utf8(&data[off..off + len]) {
                self.frame.overflow.insert(cell_idx, s.to_owned());
            }
            off += len;
        }
        off
    }
}

#[derive(Clone, Debug)]
pub enum Node {
    Fill {
        rect: Rect,
        ch: char,
        style: CellStyle,
    },
    Text {
        row: u16,
        col: u16,
        text: String,
        style: CellStyle,
    },
    WrappedText {
        rect: Rect,
        text: String,
        style: CellStyle,
    },
    ScrollingText {
        rect: Rect,
        lines: Vec<String>,
        offset_from_bottom: usize,
        style: CellStyle,
    },
}

#[derive(Clone, Debug, Default)]
pub struct Dom {
    background: CellStyle,
    title: Option<String>,
    nodes: Vec<Node>,
}

impl Dom {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.title = None;
        self.nodes.clear();
    }

    pub fn set_background(&mut self, style: CellStyle) {
        self.background = style;
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = Some(title.into());
    }

    pub fn fill(&mut self, rect: Rect, ch: char, style: CellStyle) {
        self.nodes.push(Node::Fill { rect, ch, style });
    }

    pub fn text(&mut self, row: u16, col: u16, text: impl Into<String>, style: CellStyle) {
        self.nodes.push(Node::Text {
            row,
            col,
            text: text.into(),
            style,
        });
    }

    pub fn wrapped_text(&mut self, rect: Rect, text: impl Into<String>, style: CellStyle) {
        self.nodes.push(Node::WrappedText {
            rect,
            text: text.into(),
            style,
        });
    }

    pub fn scrolling_text<S, I>(
        &mut self,
        rect: Rect,
        lines: I,
        offset_from_bottom: usize,
        style: CellStyle,
    ) where
        S: Into<String>,
        I: IntoIterator<Item = S>,
    {
        self.nodes.push(Node::ScrollingText {
            rect,
            lines: lines.into_iter().map(Into::into).collect(),
            offset_from_bottom,
            style,
        });
    }

    pub fn render_to(&self, frame: &mut FrameState) {
        frame.clear(self.background);
        frame.set_title(self.title.clone().unwrap_or_default());
        for node in &self.nodes {
            match node {
                Node::Fill { rect, ch, style } => frame.fill_rect(*rect, *ch, *style),
                Node::Text {
                    row,
                    col,
                    text,
                    style,
                } => {
                    frame.write_text(*row, *col, text, *style);
                }
                Node::WrappedText { rect, text, style } => {
                    frame.write_wrapped_text(*rect, text, *style);
                }
                Node::ScrollingText {
                    rect,
                    lines,
                    offset_from_bottom,
                    style,
                } => {
                    frame.write_scrolling_text(*rect, lines, *offset_from_bottom, *style);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct CallbackRenderer {
    dom: Dom,
    frame: FrameState,
}

impl CallbackRenderer {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            dom: Dom::new(),
            frame: FrameState::new(rows, cols),
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.frame.resize(rows, cols);
    }

    pub fn frame(&self) -> &FrameState {
        &self.frame
    }

    pub fn render<F>(&mut self, render: F) -> &FrameState
    where
        F: FnOnce(&mut Dom),
    {
        self.dom.clear();
        render(&mut self.dom);
        self.dom.render_to(&mut self.frame);
        &self.frame
    }
}

pub enum ServerMsg<'a> {
    Hello {
        version: u16,
        features: u32,
    },
    Update {
        pty_id: u16,
        payload: &'a [u8],
    },
    Created {
        pty_id: u16,
        tag: &'a str,
    },
    CreatedN {
        nonce: u16,
        pty_id: u16,
        tag: &'a str,
    },
    Closed {
        pty_id: u16,
    },
    Exited {
        pty_id: u16,
        exit_status: i32,
    },
    List {
        entries: Vec<PtyListEntry<'a>>,
    },
    Title {
        pty_id: u16,
        title: &'a [u8],
    },
    SearchResults {
        request_id: u16,
        results: Vec<SearchResultEntry<'a>>,
    },
    Ready,
    Text {
        nonce: u16,
        pty_id: u16,
        total_lines: u32,
        offset: u32,
        text: &'a str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtyListEntry<'a> {
    pub pty_id: u16,
    pub tag: &'a str,
    pub command: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchResultEntry<'a> {
    pub pty_id: u16,
    pub score: u32,
    pub primary_source: u8,
    pub matched_sources: u8,
    pub scroll_offset: Option<u32>,
    pub context: &'a [u8],
}

pub fn parse_server_msg(data: &[u8]) -> Option<ServerMsg<'_>> {
    if data.is_empty() {
        return None;
    }
    match data[0] {
        S2C_HELLO => {
            if data.len() < 7 {
                return None;
            }
            let version = u16::from_le_bytes([data[1], data[2]]);
            let features = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
            Some(ServerMsg::Hello { version, features })
        }
        S2C_UPDATE => {
            if data.len() < 3 {
                return None;
            }
            Some(ServerMsg::Update {
                pty_id: u16::from_le_bytes([data[1], data[2]]),
                payload: &data[3..],
            })
        }
        S2C_CREATED => {
            if data.len() < 3 {
                return None;
            }
            let tag = std::str::from_utf8(data.get(3..).unwrap_or_default()).unwrap_or_default();
            Some(ServerMsg::Created {
                pty_id: u16::from_le_bytes([data[1], data[2]]),
                tag,
            })
        }
        S2C_CREATED_N => {
            if data.len() < 5 {
                return None;
            }
            let nonce = u16::from_le_bytes([data[1], data[2]]);
            let pty_id = u16::from_le_bytes([data[3], data[4]]);
            let tag = std::str::from_utf8(data.get(5..).unwrap_or_default()).unwrap_or_default();
            Some(ServerMsg::CreatedN { nonce, pty_id, tag })
        }
        S2C_CLOSED => {
            if data.len() < 3 {
                return None;
            }
            Some(ServerMsg::Closed {
                pty_id: u16::from_le_bytes([data[1], data[2]]),
            })
        }
        S2C_EXITED => {
            if data.len() < 7 {
                return None;
            }
            Some(ServerMsg::Exited {
                pty_id: u16::from_le_bytes([data[1], data[2]]),
                exit_status: i32::from_le_bytes([data[3], data[4], data[5], data[6]]),
            })
        }
        S2C_LIST => {
            if data.len() < 3 {
                return None;
            }
            let count = u16::from_le_bytes([data[1], data[2]]) as usize;
            let mut entries = Vec::with_capacity(count);
            let mut offset = 3;
            for _ in 0..count {
                if offset + 4 > data.len() {
                    break;
                }
                let pty_id = u16::from_le_bytes([data[offset], data[offset + 1]]);
                let tag_len = u16::from_le_bytes([data[offset + 2], data[offset + 3]]) as usize;
                offset += 4;
                if offset + tag_len > data.len() {
                    break;
                }
                let tag = std::str::from_utf8(&data[offset..offset + tag_len]).unwrap_or_default();
                offset += tag_len;
                let command = if offset + 2 <= data.len() {
                    let cmd_len =
                        u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
                    offset += 2;
                    let cmd = if offset + cmd_len <= data.len() {
                        std::str::from_utf8(&data[offset..offset + cmd_len]).unwrap_or_default()
                    } else {
                        ""
                    };
                    offset += cmd_len;
                    cmd
                } else {
                    ""
                };
                entries.push(PtyListEntry { pty_id, tag, command });
            }
            Some(ServerMsg::List { entries })
        }
        S2C_TITLE => {
            if data.len() < 3 {
                return None;
            }
            Some(ServerMsg::Title {
                pty_id: u16::from_le_bytes([data[1], data[2]]),
                title: &data[3..],
            })
        }
        S2C_SEARCH_RESULTS => {
            if data.len() < 5 {
                return None;
            }
            let request_id = u16::from_le_bytes([data[1], data[2]]);
            let count = u16::from_le_bytes([data[3], data[4]]) as usize;
            let mut results = Vec::with_capacity(count);
            let mut offset = 5usize;
            for _ in 0..count {
                if offset + 14 > data.len() {
                    return None;
                }
                let pty_id = u16::from_le_bytes([data[offset], data[offset + 1]]);
                let score = u32::from_le_bytes([
                    data[offset + 2],
                    data[offset + 3],
                    data[offset + 4],
                    data[offset + 5],
                ]);
                let primary_source = data[offset + 6];
                let matched_sources = data[offset + 7];
                let scroll_offset = u32::from_le_bytes([
                    data[offset + 8],
                    data[offset + 9],
                    data[offset + 10],
                    data[offset + 11],
                ]);
                let context_len =
                    u16::from_le_bytes([data[offset + 12], data[offset + 13]]) as usize;
                offset += 14;
                if offset + context_len > data.len() {
                    return None;
                }
                results.push(SearchResultEntry {
                    pty_id,
                    score,
                    primary_source,
                    matched_sources,
                    scroll_offset: if scroll_offset == u32::MAX {
                        None
                    } else {
                        Some(scroll_offset)
                    },
                    context: &data[offset..offset + context_len],
                });
                offset += context_len;
            }
            Some(ServerMsg::SearchResults {
                request_id,
                results,
            })
        }
        S2C_READY => Some(ServerMsg::Ready),
        S2C_TEXT => {
            if data.len() < 13 {
                return None;
            }
            let nonce = u16::from_le_bytes([data[1], data[2]]);
            let pty_id = u16::from_le_bytes([data[3], data[4]]);
            let total_lines = u32::from_le_bytes([data[5], data[6], data[7], data[8]]);
            let offset = u32::from_le_bytes([data[9], data[10], data[11], data[12]]);
            let text = std::str::from_utf8(data.get(13..).unwrap_or_default()).unwrap_or_default();
            Some(ServerMsg::Text {
                nonce,
                pty_id,
                total_lines,
                offset,
                text,
            })
        }
        _ => None,
    }
}

pub fn msg_hello(version: u16, features: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(S2C_HELLO);
    msg.extend_from_slice(&version.to_le_bytes());
    msg.extend_from_slice(&features.to_le_bytes());
    msg
}

pub fn msg_create(rows: u16, cols: u16) -> Vec<u8> {
    msg_create_tagged(rows, cols, "")
}

pub fn msg_create_tagged(rows: u16, cols: u16, tag: &str) -> Vec<u8> {
    let tag_bytes = tag.as_bytes();
    let tag_len = tag_bytes.len().min(u16::MAX as usize);
    let mut msg = Vec::with_capacity(7 + tag_len);
    msg.push(C2S_CREATE);
    msg.extend_from_slice(&rows.to_le_bytes());
    msg.extend_from_slice(&cols.to_le_bytes());
    msg.extend_from_slice(&(tag_len as u16).to_le_bytes());
    msg.extend_from_slice(&tag_bytes[..tag_len]);
    msg
}

/// Spawn a new PTY in the same working directory as `src_pty_id`.
pub fn msg_create_at(rows: u16, cols: u16, tag: &str, src_pty_id: u16) -> Vec<u8> {
    let tag_bytes = tag.as_bytes();
    let tag_len = tag_bytes.len().min(u16::MAX as usize);
    let mut msg = Vec::with_capacity(9 + tag_len);
    msg.push(C2S_CREATE_AT);
    msg.extend_from_slice(&rows.to_le_bytes());
    msg.extend_from_slice(&cols.to_le_bytes());
    msg.extend_from_slice(&(tag_len as u16).to_le_bytes());
    msg.extend_from_slice(&tag_bytes[..tag_len]);
    msg.extend_from_slice(&src_pty_id.to_le_bytes());
    msg
}

pub fn msg_create_n(nonce: u16, rows: u16, cols: u16, tag: &str) -> Vec<u8> {
    let tag_bytes = tag.as_bytes();
    let tag_len = tag_bytes.len().min(u16::MAX as usize);
    let mut msg = Vec::with_capacity(9 + tag_len);
    msg.push(C2S_CREATE_N);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&rows.to_le_bytes());
    msg.extend_from_slice(&cols.to_le_bytes());
    msg.extend_from_slice(&(tag_len as u16).to_le_bytes());
    msg.extend_from_slice(&tag_bytes[..tag_len]);
    msg
}

pub fn msg_create_n_command(nonce: u16, rows: u16, cols: u16, tag: &str, command: &str) -> Vec<u8> {
    let mut msg = msg_create_n(nonce, rows, cols, tag);
    msg.extend_from_slice(command.as_bytes());
    msg
}

pub fn msg_create_command(rows: u16, cols: u16, command: &str) -> Vec<u8> {
    msg_create_tagged_command(rows, cols, "", command)
}

pub fn msg_create_tagged_command(rows: u16, cols: u16, tag: &str, command: &str) -> Vec<u8> {
    let mut msg = msg_create_tagged(rows, cols, tag);
    msg.extend_from_slice(command.as_bytes());
    msg
}

pub fn msg_input(pty_id: u16, data: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3 + data.len());
    msg.push(C2S_INPUT);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(data);
    msg
}

pub fn msg_resize(pty_id: u16, rows: u16, cols: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(C2S_RESIZE);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&rows.to_le_bytes());
    msg.extend_from_slice(&cols.to_le_bytes());
    msg
}

pub fn msg_resize_batch(entries: &[(u16, u16, u16)]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(1 + entries.len() * 6);
    msg.push(C2S_RESIZE);
    for &(pty_id, rows, cols) in entries {
        msg.extend_from_slice(&pty_id.to_le_bytes());
        msg.extend_from_slice(&rows.to_le_bytes());
        msg.extend_from_slice(&cols.to_le_bytes());
    }
    msg
}

pub fn msg_focus(pty_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_FOCUS);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg
}

pub fn msg_close(pty_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_CLOSE);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg
}

pub fn msg_kill(pty_id: u16, signal: i32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(C2S_KILL);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&signal.to_le_bytes());
    msg
}

pub fn msg_restart(pty_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_RESTART);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg
}

pub fn msg_subscribe(pty_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_SUBSCRIBE);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg
}

pub fn msg_unsubscribe(pty_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_UNSUBSCRIBE);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg
}

pub fn msg_search(request_id: u16, query: &str) -> Vec<u8> {
    let query = query.as_bytes();
    let mut msg = Vec::with_capacity(3 + query.len());
    msg.push(C2S_SEARCH);
    msg.extend_from_slice(&request_id.to_le_bytes());
    msg.extend_from_slice(query);
    msg
}

pub fn msg_ack() -> Vec<u8> {
    vec![C2S_ACK]
}

pub fn msg_scroll(pty_id: u16, offset: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(C2S_SCROLL);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&offset.to_le_bytes());
    msg
}

pub fn msg_display_rate(fps: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_DISPLAY_RATE);
    msg.extend_from_slice(&fps.to_le_bytes());
    msg
}

pub fn msg_client_metrics(backlog: u16, ack_ahead: u16, apply_ms_x10: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(C2S_CLIENT_METRICS);
    msg.extend_from_slice(&backlog.to_le_bytes());
    msg.extend_from_slice(&ack_ahead.to_le_bytes());
    msg.extend_from_slice(&apply_ms_x10.to_le_bytes());
    msg
}

pub fn msg_read(nonce: u16, pty_id: u16, offset: u32, limit: u32, flags: u8) -> Vec<u8> {
    let mut msg = Vec::with_capacity(14);
    msg.push(C2S_READ);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&offset.to_le_bytes());
    msg.extend_from_slice(&limit.to_le_bytes());
    msg.push(flags);
    msg
}

pub fn msg_copy_range(
    nonce: u16,
    pty_id: u16,
    start_tail: u32,
    start_col: u16,
    end_tail: u32,
    end_col: u16,
    flags: u8,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(18);
    msg.push(C2S_COPY_RANGE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&start_tail.to_le_bytes());
    msg.extend_from_slice(&start_col.to_le_bytes());
    msg.extend_from_slice(&end_tail.to_le_bytes());
    msg.extend_from_slice(&end_col.to_le_bytes());
    msg.push(flags);
    msg
}

pub fn msg_exited(pty_id: u16, exit_status: i32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(S2C_EXITED);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&exit_status.to_le_bytes());
    msg
}

fn push_sgr(out: &mut String, style: &CellStyle) {
    use std::fmt::Write;
    out.push_str("\x1b[0");
    if style.bold {
        out.push_str(";1");
    }
    if style.dim {
        out.push_str(";2");
    }
    if style.italic {
        out.push_str(";3");
    }
    if style.underline {
        out.push_str(";4");
    }
    if style.inverse {
        out.push_str(";7");
    }
    match style.fg {
        Color::Indexed(n) => {
            let _ = write!(out, ";38;5;{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";38;2;{r};{g};{b}");
        }
        Color::Default => {}
    }
    match style.bg {
        Color::Indexed(n) => {
            let _ = write!(out, ";48;5;{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";48;2;{r};{g};{b}");
        }
        Color::Default => {}
    }
    out.push('m');
}

const MODE_ALT_SCREEN: u16 = 1 << 11;

fn mode_is_cooked(mode: u16) -> bool {
    mode & MODE_ECHO != 0 && mode & MODE_ICANON != 0 && mode & MODE_ALT_SCREEN == 0
}

pub fn build_update_msg(
    pty_id: u16,
    current: &FrameState,
    previous: &FrameState,
) -> Option<Vec<u8>> {
    let title_changed = current.title != previous.title;
    let same_size = previous.rows == current.rows
        && previous.cols == current.cols
        && previous.cells.len() == current.cells.len();

    // Try scroll-aware ops when dimensions match and content differs.
    let mut ops = Vec::new();
    let mut op_count = 0u16;

    // Scroll-aware ops apply when content is "cooked" (shell output) or when
    // either frame has mode 0 (scrollback frames use mode=0, and their content
    // is always static text that benefits from COPY_RECT).
    let scroll_eligible = (mode_is_cooked(current.mode) && mode_is_cooked(previous.mode))
        || current.mode == 0
        || previous.mode == 0;
    if ENABLE_SCROLL_OPS && same_size && previous.cells != current.cells && scroll_eligible {
        if let Some(delta_rows) = detect_vertical_scroll(current, previous) {
            let mut basis = previous.clone();
            encode_copy_rect_op(&mut ops, current, delta_rows);
            apply_vertical_scroll_copy(&mut basis, delta_rows);
            op_count += 1;
            append_full_width_fill_ops(current, &mut basis, &mut ops, &mut op_count);
            if let Some(patch_op) = build_patch_op(current, &basis) {
                ops.extend_from_slice(&patch_op);
                op_count += 1;
            }
        }
    }

    // Fallback: bare PATCH_CELLS against previous (or a blank frame on resize).
    if op_count == 0 {
        let basis = if same_size {
            previous
        } else {
            &FrameState::new(current.rows, current.cols)
        };
        if let Some(patch_op) = build_patch_op(current, basis) {
            ops = patch_op;
            op_count = 1;
        }
    }

    if op_count == 0 {
        // No cell changes — still emit a frame if cursor/mode/title changed.
        if !title_changed
            && current.cursor_row == previous.cursor_row
            && current.cursor_col == previous.cursor_col
            && current.mode == previous.mode
        {
            return None;
        }
    }

    // Collect overflow strings that need to be transmitted.
    // We send all overflow entries from the current frame that correspond
    // to cells that changed (are in the dirty set).  For a resize (not
    // same_size), all cells are "dirty", so we send all overflow entries.
    let has_overflow = !current.overflow.is_empty();
    let overflow_section = if has_overflow {
        serialize_overflow_strings(current)
    } else {
        Vec::new()
    };

    let line_flags_changed =
        current.line_flags != previous.line_flags || current.rows != previous.rows;
    let has_line_flags = line_flags_changed && !current.line_flags.iter().all(|&f| f == 0);

    let title_bytes = if title_changed {
        current.title.as_bytes()
    } else {
        &[]
    };
    let title_len = title_bytes.len().min(TITLE_LEN_MASK as usize);
    let title_field = OPS_PRESENT
        | if has_overflow { STRINGS_PRESENT } else { 0 }
        | if has_line_flags {
            LINE_FLAGS_PRESENT
        } else {
            0
        }
        | if title_changed {
            TITLE_PRESENT | title_len as u16
        } else {
            0
        };

    let mut payload = Vec::with_capacity(
        12 + title_len
            + 2
            + ops.len()
            + overflow_section.len()
            + if has_line_flags {
                current.rows as usize
            } else {
                0
            }
            + 4,
    );
    payload.extend_from_slice(&current.rows.to_le_bytes());
    payload.extend_from_slice(&current.cols.to_le_bytes());
    payload.extend_from_slice(&current.cursor_row.to_le_bytes());
    payload.extend_from_slice(&current.cursor_col.to_le_bytes());
    payload.extend_from_slice(&current.mode.to_le_bytes());
    payload.extend_from_slice(&title_field.to_le_bytes());
    if title_changed {
        payload.extend_from_slice(&title_bytes[..title_len]);
    }
    payload.extend_from_slice(&op_count.to_le_bytes());
    payload.extend_from_slice(&ops);
    payload.extend_from_slice(&overflow_section);
    if has_line_flags {
        payload.extend_from_slice(&current.line_flags);
    }
    // Trailing scrollback count — old clients ignore extra bytes.
    payload.extend_from_slice(&current.scrollback_lines.to_le_bytes());

    let compressed = compress_prepend_size(&payload);
    let mut msg = Vec::with_capacity(3 + compressed.len());
    msg.push(S2C_UPDATE);
    msg.extend_from_slice(&pty_id.to_le_bytes());
    msg.extend_from_slice(&compressed);
    Some(msg)
}

/// Serialize overflow strings: [u16 count] [for each: u32 cell_index, u16 len, utf8 bytes]
fn serialize_overflow_strings(frame: &FrameState) -> Vec<u8> {
    let count = frame.overflow.len().min(u16::MAX as usize);
    let mut out = Vec::with_capacity(2 + count * 8);
    out.extend_from_slice(&(count as u16).to_le_bytes());
    for (&cell_idx, s) in frame.overflow.iter().take(count) {
        let bytes = s.as_bytes();
        let len = bytes.len().min(u16::MAX as usize);
        out.extend_from_slice(&(cell_idx as u32).to_le_bytes());
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(&bytes[..len]);
    }
    out
}

fn build_patch_op(current: &FrameState, previous: &FrameState) -> Option<Vec<u8>> {
    let total_cells = current.rows as usize * current.cols as usize;
    let bitmask_len = total_cells.div_ceil(8);
    let mut bitmask = vec![0u8; bitmask_len];
    let mut dirty_count = 0usize;
    for i in 0..total_cells {
        let off = i * CELL_SIZE;
        if current.cells[off..off + CELL_SIZE] != previous.cells[off..off + CELL_SIZE] {
            bitmask[i / 8] |= 1 << (i % 8);
            dirty_count += 1;
        }
    }
    if dirty_count == 0 {
        return None;
    }

    let mut op = Vec::with_capacity(1 + bitmask_len + dirty_count * CELL_SIZE);
    op.push(OP_PATCH_CELLS);
    op.extend_from_slice(&bitmask);
    for byte_pos in 0..CELL_SIZE {
        for i in 0..total_cells {
            if bitmask[i / 8] & (1 << (i % 8)) != 0 {
                op.push(current.cells[i * CELL_SIZE + byte_pos]);
            }
        }
    }
    Some(op)
}

fn detect_vertical_scroll(current: &FrameState, previous: &FrameState) -> Option<i16> {
    let rows = current.rows as usize;
    let cols = current.cols as usize;
    if rows < 4 || cols == 0 {
        return None;
    }
    let row_bytes = cols * CELL_SIZE;
    let max_delta = rows.saturating_sub(1).min(8);
    let mut best: Option<(usize, i16)> = None;

    for delta in 1..=max_delta {
        let overlap = rows - delta;
        if overlap < 3 {
            continue;
        }
        for signed_delta in [-(delta as i16), delta as i16] {
            let mut matched = 0usize;
            for row in 0..rows {
                let src_row = row as i32 - signed_delta as i32;
                if src_row < 0 || src_row >= rows as i32 {
                    continue;
                }
                let cur_off = row * row_bytes;
                let prev_off = src_row as usize * row_bytes;
                if current.cells[cur_off..cur_off + row_bytes]
                    == previous.cells[prev_off..prev_off + row_bytes]
                {
                    matched += 1;
                }
            }
            if matched * 5 < overlap * 4 {
                continue;
            }
            let replace = match best {
                None => true,
                Some((best_matched, best_delta)) => {
                    matched > best_matched
                        || (matched == best_matched
                            && signed_delta.unsigned_abs() < best_delta.unsigned_abs())
                }
            };
            if replace {
                best = Some((matched, signed_delta));
            }
        }
    }

    best.map(|(_, delta)| delta)
}

fn encode_copy_rect_op(out: &mut Vec<u8>, current: &FrameState, delta_rows: i16) {
    let rows = current.rows;
    let cols = current.cols;
    let delta = delta_rows.unsigned_abs();
    let (src_row, dst_row, copy_rows) = if delta_rows > 0 {
        (0, delta, rows.saturating_sub(delta))
    } else {
        (delta, 0, rows.saturating_sub(delta))
    };
    out.push(OP_COPY_RECT);
    out.extend_from_slice(&src_row.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&dst_row.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&copy_rows.to_le_bytes());
    out.extend_from_slice(&cols.to_le_bytes());
}

fn apply_vertical_scroll_copy(frame: &mut FrameState, delta_rows: i16) {
    let delta = delta_rows.unsigned_abs();
    if delta == 0 || delta >= frame.rows {
        return;
    }
    let (src_row, dst_row, rows) = if delta_rows > 0 {
        (0, delta, frame.rows - delta)
    } else {
        (delta, 0, frame.rows - delta)
    };
    apply_copy_rect_frame(frame, src_row, 0, dst_row, 0, rows, frame.cols);
}

fn apply_copy_rect_frame(
    frame: &mut FrameState,
    src_row: u16,
    src_col: u16,
    dst_row: u16,
    dst_col: u16,
    rows: u16,
    cols: u16,
) {
    let rows = rows
        .min(frame.rows.saturating_sub(src_row))
        .min(frame.rows.saturating_sub(dst_row));
    let cols = cols
        .min(frame.cols.saturating_sub(src_col))
        .min(frame.cols.saturating_sub(dst_col));
    if rows == 0 || cols == 0 {
        return;
    }
    let mut temp = vec![0u8; rows as usize * cols as usize * CELL_SIZE];
    for r in 0..rows as usize {
        let src_off = frame.cell_offset(src_row + r as u16, src_col);
        let src_end = src_off + cols as usize * CELL_SIZE;
        let dst_off = r * cols as usize * CELL_SIZE;
        temp[dst_off..dst_off + cols as usize * CELL_SIZE]
            .copy_from_slice(&frame.cells[src_off..src_end]);
    }
    for r in 0..rows as usize {
        let dst_off = frame.cell_offset(dst_row + r as u16, dst_col);
        let dst_end = dst_off + cols as usize * CELL_SIZE;
        let src_off = r * cols as usize * CELL_SIZE;
        frame.cells[dst_off..dst_end]
            .copy_from_slice(&temp[src_off..src_off + cols as usize * CELL_SIZE]);
    }
}

fn append_full_width_fill_ops(
    current: &FrameState,
    basis: &mut FrameState,
    out: &mut Vec<u8>,
    op_count: &mut u16,
) {
    let rows = current.rows as usize;
    let cols = current.cols as usize;
    if rows == 0 || cols == 0 {
        return;
    }

    let row_bytes = cols * CELL_SIZE;
    let mut row = 0usize;
    while row < rows {
        let row_off = row * row_bytes;
        if current.cells[row_off..row_off + row_bytes] == basis.cells[row_off..row_off + row_bytes]
        {
            row += 1;
            continue;
        }
        let Some(cell) = uniform_row_cell(current, row) else {
            row += 1;
            continue;
        };
        let mut end = row + 1;
        while end < rows {
            if uniform_row_cell(current, end).as_ref() != Some(&cell) {
                break;
            }
            end += 1;
        }

        if *op_count == u16::MAX {
            break;
        }
        out.push(OP_FILL_RECT);
        out.extend_from_slice(&(row as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&((end - row) as u16).to_le_bytes());
        out.extend_from_slice(&current.cols.to_le_bytes());
        out.extend_from_slice(&cell);
        *op_count = op_count.saturating_add(1);

        for r in row..end {
            let row_off = basis.cell_offset(r as u16, 0);
            for c in 0..cols {
                let off = row_off + c * CELL_SIZE;
                basis.cells[off..off + CELL_SIZE].copy_from_slice(&cell);
            }
        }

        row = end;
    }
}

fn uniform_row_cell(frame: &FrameState, row: usize) -> Option<[u8; CELL_SIZE]> {
    let cols = frame.cols as usize;
    if row >= frame.rows as usize || cols == 0 {
        return None;
    }
    let start = row * cols * CELL_SIZE;
    let mut first = [0u8; CELL_SIZE];
    first.copy_from_slice(&frame.cells[start..start + CELL_SIZE]);
    if first[1] & 0b110 != 0 {
        return None;
    }
    for col in 1..cols {
        let off = start + col * CELL_SIZE;
        if frame.cells[off..off + CELL_SIZE] != first {
            return None;
        }
    }
    Some(first)
}

fn encode_cell(dst: &mut [u8], ch: Option<char>, style: CellStyle, wide: bool, wide_cont: bool) {
    dst.fill(0);

    let mut f0 = 0u8;
    encode_color(style.fg, &mut f0, &mut dst[2..5], false);
    encode_color(style.bg, &mut f0, &mut dst[5..8], true);
    if style.bold {
        f0 |= 1 << 4;
    }
    if style.dim {
        f0 |= 1 << 5;
    }
    if style.italic {
        f0 |= 1 << 6;
    }
    if style.underline {
        f0 |= 1 << 7;
    }
    dst[0] = f0;

    let mut f1 = 0u8;
    if style.inverse {
        f1 |= 1;
    }
    if wide {
        f1 |= 1 << 1;
    }
    if wide_cont {
        f1 |= 1 << 2;
    }
    if let Some(ch) = ch {
        let mut buf = [0u8; 4];
        let encoded = ch.encode_utf8(&mut buf).as_bytes();
        let len = encoded.len().min(4);
        dst[8..8 + len].copy_from_slice(&encoded[..len]);
        f1 |= (len as u8) << 3;
    }
    dst[1] = f1;
}

fn encode_color(color: Color, flags: &mut u8, dst: &mut [u8], is_bg: bool) {
    let shift = if is_bg { 2 } else { 0 };
    match color {
        Color::Default => {}
        Color::Indexed(idx) => {
            *flags |= 1 << shift;
            dst[0] = idx;
        }
        Color::Rgb(r, g, b) => {
            *flags |= 2 << shift;
            dst[0] = r;
            dst[1] = g;
            dst[2] = b;
        }
    }
}

fn wrap_text_lines(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_width = 0usize;
        for word in paragraph.split_whitespace() {
            push_wrapped_word(word, width, &mut out, &mut line, &mut line_width);
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn push_wrapped_word(
    word: &str,
    width: usize,
    out: &mut Vec<String>,
    line: &mut String,
    line_width: &mut usize,
) {
    let word_width = UnicodeWidthStr::width(word);
    if line.is_empty() {
        if word_width <= width {
            line.push_str(word);
            *line_width = word_width;
            return;
        }
    } else if *line_width + 1 + word_width <= width {
        line.push(' ');
        line.push_str(word);
        *line_width += 1 + word_width;
        return;
    } else {
        out.push(std::mem::take(line));
        *line_width = 0;
        if word_width <= width {
            line.push_str(word);
            *line_width = word_width;
            return;
        }
    }

    for ch in word.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
        if *line_width + ch_width > width && !line.is_empty() {
            out.push(std::mem::take(line));
            *line_width = 0;
        }
        line.push(ch);
        *line_width += ch_width;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_round_trip_preserves_title_and_cells() {
        let style = CellStyle::default();
        let mut prev = FrameState::new(2, 8);
        prev.set_title("one");
        prev.write_text(0, 0, "hello", style);

        let mut next = prev.clone();
        next.set_title("two");
        next.write_text(1, 0, "world", style);

        let baseline = build_update_msg(7, &prev, &FrameState::default()).unwrap();
        let delta = build_update_msg(7, &next, &prev).unwrap();

        let mut term = TerminalState::new(2, 8);
        let ServerMsg::Update { payload, .. } = parse_server_msg(&baseline).unwrap() else {
            panic!("expected update");
        };
        assert!(term.feed_compressed(payload));
        assert_eq!(term.title(), "one");

        let ServerMsg::Update { payload, .. } = parse_server_msg(&delta).unwrap() else {
            panic!("expected update");
        };
        assert!(term.feed_compressed(payload));
        assert_eq!(term.title(), "two");
        assert_eq!(term.get_all_text(), "hello\nworld");
    }

    #[test]
    fn title_can_be_cleared_via_update() {
        let style = CellStyle::default();
        let mut prev = FrameState::new(1, 4);
        prev.set_title("busy");
        prev.write_text(0, 0, "ping", style);

        let mut next = prev.clone();
        next.set_title("");

        let baseline = build_update_msg(1, &prev, &FrameState::default()).unwrap();
        let delta = build_update_msg(1, &next, &prev).unwrap();

        let mut term = TerminalState::new(1, 4);
        let ServerMsg::Update { payload, .. } = parse_server_msg(&baseline).unwrap() else {
            panic!("expected update");
        };
        term.feed_compressed(payload);
        let ServerMsg::Update { payload, .. } = parse_server_msg(&delta).unwrap() else {
            panic!("expected update");
        };
        term.feed_compressed(payload);
        assert_eq!(term.title(), "");
    }

    #[test]
    fn scroll_heavy_update_can_use_ops_payload() {
        let style = CellStyle::default();
        let mut prev = FrameState::new(5, 6);
        prev.write_text(0, 0, "one", style);
        prev.write_text(1, 0, "two", style);
        prev.write_text(2, 0, "three", style);
        prev.write_text(3, 0, "four", style);
        prev.write_text(4, 0, "five", style);

        let mut next = FrameState::new(5, 6);
        next.write_text(0, 0, "two", style);
        next.write_text(1, 0, "three", style);
        next.write_text(2, 0, "four", style);
        next.write_text(3, 0, "five", style);

        let delta = build_update_msg(9, &next, &prev).unwrap();
        let ServerMsg::Update { payload, .. } = parse_server_msg(&delta).unwrap() else {
            panic!("expected update");
        };
        let decoded = decompress_size_prepended(payload).unwrap();
        let title_field = u16::from_le_bytes([decoded[10], decoded[11]]);
        assert_ne!(title_field & OPS_PRESENT, 0);

        let mut term = TerminalState::new(5, 6);
        let baseline = build_update_msg(9, &prev, &FrameState::default()).unwrap();
        let ServerMsg::Update { payload, .. } = parse_server_msg(&baseline).unwrap() else {
            panic!("expected update");
        };
        assert!(term.feed_compressed(payload));
        let ServerMsg::Update { payload, .. } = parse_server_msg(&delta).unwrap() else {
            panic!("expected update");
        };
        assert!(term.feed_compressed(payload));
        assert_eq!(term.get_all_text(), "two\nthree\nfour\nfive\n");
    }

    #[test]
    fn cooked_scroll_heavy_update_uses_copy_rect_op() {
        let style = CellStyle::default();
        let mut prev = FrameState::new(5, 6);
        prev.set_mode(MODE_ECHO | MODE_ICANON);
        prev.write_text(0, 0, "one", style);
        prev.write_text(1, 0, "two", style);
        prev.write_text(2, 0, "three", style);
        prev.write_text(3, 0, "four", style);
        prev.write_text(4, 0, "five", style);

        let mut next = FrameState::new(5, 6);
        next.set_mode(MODE_ECHO | MODE_ICANON);
        next.write_text(0, 0, "two", style);
        next.write_text(1, 0, "three", style);
        next.write_text(2, 0, "four", style);
        next.write_text(3, 0, "five", style);

        let delta = build_update_msg(9, &next, &prev).unwrap();
        let ServerMsg::Update { payload, .. } = parse_server_msg(&delta).unwrap() else {
            panic!("expected update");
        };
        let decoded = decompress_size_prepended(payload).unwrap();
        let op_count = u16::from_le_bytes([decoded[12], decoded[13]]);
        assert!(op_count >= 1);
        assert_eq!(decoded[14], OP_COPY_RECT);
    }

    #[test]
    fn mode_zero_scroll_uses_copy_rect() {
        let style = CellStyle::default();
        let mut prev = FrameState::new(5, 6);
        prev.write_text(0, 0, "one", style);
        prev.write_text(1, 0, "two", style);
        prev.write_text(2, 0, "three", style);
        prev.write_text(3, 0, "four", style);
        prev.write_text(4, 0, "five", style);

        let mut next = FrameState::new(5, 6);
        next.write_text(0, 0, "two", style);
        next.write_text(1, 0, "three", style);
        next.write_text(2, 0, "four", style);
        next.write_text(3, 0, "five", style);

        let delta = build_update_msg(9, &next, &prev).unwrap();
        let ServerMsg::Update { payload, .. } = parse_server_msg(&delta).unwrap() else {
            panic!("expected update");
        };
        let decoded = decompress_size_prepended(payload).unwrap();
        let op_count = u16::from_le_bytes([decoded[12], decoded[13]]);
        assert!(op_count >= 1);
        // mode=0 frames (scrollback) now use COPY_RECT for efficient scrolling
        assert_eq!(decoded[14], OP_COPY_RECT);

        // Verify round-trip correctness
        let baseline = build_update_msg(9, &prev, &FrameState::new(5, 6)).unwrap();
        let mut state = TerminalState::new(5, 6);
        let ServerMsg::Update { payload: bp, .. } = parse_server_msg(&baseline).unwrap() else {
            panic!("expected update");
        };
        state.feed_compressed(bp);
        state.feed_compressed(payload);
        assert_eq!(state.frame().cells(), next.cells());
    }

    #[test]
    fn callback_renderer_wraps_text() {
        let mut renderer = CallbackRenderer::new(2, 8);
        renderer.render(|dom| {
            dom.wrapped_text(
                Rect::new(0, 0, 2, 8),
                "alpha beta gamma",
                CellStyle::default(),
            );
        });
        assert_eq!(renderer.frame().get_all_text(), "alpha\nbeta");
    }

    #[test]
    fn scrolling_text_shows_tail() {
        let mut frame = FrameState::new(3, 8);
        frame.write_scrolling_text(
            Rect::new(0, 0, 3, 8),
            &["one", "two", "three", "four"],
            0,
            CellStyle::default(),
        );
        assert_eq!(frame.get_all_text(), "two\nthree\nfour");
    }

    #[test]
    fn search_results_round_trip_with_context() {
        let msg = [
            vec![S2C_SEARCH_RESULTS],
            7u16.to_le_bytes().to_vec(),
            1u16.to_le_bytes().to_vec(),
            42u16.to_le_bytes().to_vec(),
            1234u32.to_le_bytes().to_vec(),
            vec![1, 0b111],
            9u32.to_le_bytes().to_vec(),
            5u16.to_le_bytes().to_vec(),
            b"hello".to_vec(),
        ]
        .concat();

        let ServerMsg::SearchResults {
            request_id,
            results,
        } = parse_server_msg(&msg).unwrap()
        else {
            panic!("expected search results");
        };
        assert_eq!(request_id, 7);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].pty_id, 42);
        assert_eq!(results[0].score, 1234);
        assert_eq!(results[0].primary_source, 1);
        assert_eq!(results[0].matched_sources, 0b111);
        assert_eq!(results[0].scroll_offset, Some(9));
        assert_eq!(results[0].context, b"hello");
    }

    // --- Tag tests ---

    #[test]
    fn msg_create_no_tag_has_zero_tag_len() {
        let msg = msg_create(24, 80);
        assert_eq!(msg.len(), 7);
        assert_eq!(msg[0], C2S_CREATE);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 24);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 80);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 0);
    }

    #[test]
    fn msg_create_tagged_encodes_tag() {
        let msg = msg_create_tagged(24, 80, "my-pty");
        assert_eq!(msg[0], C2S_CREATE);
        let tag_len = u16::from_le_bytes([msg[5], msg[6]]) as usize;
        assert_eq!(tag_len, 6);
        assert_eq!(&msg[7..7 + tag_len], b"my-pty");
        assert_eq!(msg.len(), 7 + tag_len);
    }

    #[test]
    fn msg_create_tagged_command_encodes_both() {
        let msg = msg_create_tagged_command(30, 120, "editor", "vim");
        let tag_len = u16::from_le_bytes([msg[5], msg[6]]) as usize;
        assert_eq!(tag_len, 6);
        assert_eq!(&msg[7..13], b"editor");
        assert_eq!(&msg[13..], b"vim");
    }

    #[test]
    fn msg_create_command_has_empty_tag() {
        let msg = msg_create_command(24, 80, "ls");
        let tag_len = u16::from_le_bytes([msg[5], msg[6]]) as usize;
        assert_eq!(tag_len, 0);
        assert_eq!(&msg[7..], b"ls");
    }

    #[test]
    fn msg_create_tagged_empty_tag() {
        let msg = msg_create_tagged(24, 80, "");
        assert_eq!(msg.len(), 7);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 0);
    }

    #[test]
    fn msg_create_tagged_unicode_tag() {
        let msg = msg_create_tagged(24, 80, "日本語");
        let tag_len = u16::from_le_bytes([msg[5], msg[6]]) as usize;
        assert_eq!(tag_len, "日本語".len());
        assert_eq!(std::str::from_utf8(&msg[7..7 + tag_len]).unwrap(), "日本語");
    }

    #[test]
    fn parse_created_with_tag() {
        let mut wire = vec![S2C_CREATED, 0x05, 0x00];
        wire.extend_from_slice(b"hello");
        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::Created { pty_id, tag } => {
                assert_eq!(pty_id, 5);
                assert_eq!(tag, "hello");
            }
            _ => panic!("expected Created"),
        }
    }

    #[test]
    fn parse_created_without_tag() {
        let wire = vec![S2C_CREATED, 0x03, 0x00];
        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::Created { pty_id, tag } => {
                assert_eq!(pty_id, 3);
                assert_eq!(tag, "");
            }
            _ => panic!("expected Created"),
        }
    }

    #[test]
    fn parse_created_n_with_tag() {
        let mut wire = vec![S2C_CREATED_N, 0x2a, 0x00, 0x05, 0x00];
        wire.extend_from_slice(b"hello");
        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::CreatedN { nonce, pty_id, tag } => {
                assert_eq!(nonce, 42);
                assert_eq!(pty_id, 5);
                assert_eq!(tag, "hello");
            }
            _ => panic!("expected CreatedN"),
        }
    }

    #[test]
    fn msg_create_n_format() {
        let msg = msg_create_n(42, 24, 80, "test");
        assert_eq!(msg[0], C2S_CREATE_N);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 42);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 24);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 80);
        assert_eq!(u16::from_le_bytes([msg[7], msg[8]]), 4);
        assert_eq!(&msg[9..], b"test");
    }

    #[test]
    fn msg_create_n_command_format() {
        let msg = msg_create_n_command(7, 30, 120, "bg", "make build");
        assert_eq!(msg[0], C2S_CREATE_N);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 7);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 30);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 120);
        let tag_len = u16::from_le_bytes([msg[7], msg[8]]) as usize;
        assert_eq!(tag_len, 2);
        assert_eq!(&msg[9..9 + tag_len], b"bg");
        assert_eq!(&msg[9 + tag_len..], b"make build");
    }

    #[test]
    fn parse_list_with_tags() {
        // 2 entries: id=1 tag="ab", id=2 tag=""
        let mut wire = vec![S2C_LIST, 0x02, 0x00];
        // entry 1: id=1, tag_len=2, tag="ab", cmd_len=0
        wire.extend_from_slice(&1u16.to_le_bytes());
        wire.extend_from_slice(&2u16.to_le_bytes());
        wire.extend_from_slice(b"ab");
        wire.extend_from_slice(&0u16.to_le_bytes());
        // entry 2: id=2, tag_len=0, cmd_len=0
        wire.extend_from_slice(&2u16.to_le_bytes());
        wire.extend_from_slice(&0u16.to_le_bytes());
        wire.extend_from_slice(&0u16.to_le_bytes());

        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::List { entries } => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].pty_id, 1);
                assert_eq!(entries[0].tag, "ab");
                assert_eq!(entries[1].pty_id, 2);
                assert_eq!(entries[1].tag, "");
            }
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn parse_list_empty() {
        let wire = vec![S2C_LIST, 0x00, 0x00];
        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::List { entries } => assert_eq!(entries.len(), 0),
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn parse_list_truncated_gracefully() {
        // count=2 but only 1 complete entry
        let mut wire = vec![S2C_LIST, 0x02, 0x00];
        wire.extend_from_slice(&1u16.to_le_bytes());
        wire.extend_from_slice(&0u16.to_le_bytes());
        // missing second entry
        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::List { entries } => assert_eq!(entries.len(), 1),
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn parse_list_with_long_tags() {
        let long_tag = "a".repeat(300);
        let mut wire = vec![S2C_LIST, 0x01, 0x00];
        wire.extend_from_slice(&42u16.to_le_bytes());
        wire.extend_from_slice(&(long_tag.len() as u16).to_le_bytes());
        wire.extend_from_slice(long_tag.as_bytes());

        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::List { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].pty_id, 42);
                assert_eq!(entries[0].tag, long_tag);
            }
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn create_and_created_tag_round_trip() {
        // Simulate: client sends create with tag, server echoes tag in created
        let create_msg = msg_create_tagged(24, 80, "my-session");
        let tag_len = u16::from_le_bytes([create_msg[5], create_msg[6]]) as usize;
        let tag = std::str::from_utf8(&create_msg[7..7 + tag_len]).unwrap();

        // Server builds S2C_CREATED with the tag
        let mut created_wire = vec![S2C_CREATED, 0x07, 0x00]; // pty_id = 7
        created_wire.extend_from_slice(tag.as_bytes());

        let msg = parse_server_msg(&created_wire).unwrap();
        match msg {
            ServerMsg::Created {
                pty_id,
                tag: parsed_tag,
            } => {
                assert_eq!(pty_id, 7);
                assert_eq!(parsed_tag, "my-session");
            }
            _ => panic!("expected Created"),
        }
    }

    // --- FrameState tests ---

    #[test]
    fn frame_state_accessors() {
        let mut f = FrameState::new(4, 10);
        assert_eq!(f.rows(), 4);
        assert_eq!(f.cols(), 10);
        assert_eq!(f.cursor_row(), 0);
        assert_eq!(f.cursor_col(), 0);
        assert_eq!(f.mode(), 0);
        assert_eq!(f.title(), "");
        assert_eq!(f.cells().len(), 4 * 10 * CELL_SIZE);
        assert_eq!(f.cells_mut().len(), 4 * 10 * CELL_SIZE);
        assert!(f.overflow().is_empty());
        assert!(f.overflow_mut().is_empty());
    }

    #[test]
    fn frame_state_from_parts() {
        let cells = vec![0u8; 2 * 4 * CELL_SIZE];
        let f = FrameState::from_parts(2, 4, 1, 3, 0x0F, "hello", cells.clone());
        assert_eq!(f.rows(), 2);
        assert_eq!(f.cols(), 4);
        assert_eq!(f.cursor_row(), 1);
        assert_eq!(f.cursor_col(), 3);
        assert_eq!(f.mode(), 0x0F);
        assert_eq!(f.title(), "hello");
        assert_eq!(f.cells(), &cells[..]);
    }

    #[test]
    fn frame_state_from_parts_wrong_size() {
        // cells with wrong size should be ignored (stays zeroed)
        let cells = vec![0u8; 10]; // wrong size
        let f = FrameState::from_parts(2, 4, 0, 0, 0, "", cells);
        assert_eq!(f.cells().len(), 2 * 4 * CELL_SIZE);
    }

    #[test]
    fn frame_state_resize() {
        let mut f = FrameState::new(4, 10);
        f.set_cursor(3, 9);
        f.resize(2, 5);
        assert_eq!(f.rows(), 2);
        assert_eq!(f.cols(), 5);
        assert_eq!(f.cursor_row(), 1); // clamped
        assert_eq!(f.cursor_col(), 4); // clamped
        assert_eq!(f.cells().len(), 2 * 5 * CELL_SIZE);
    }

    #[test]
    fn frame_state_resize_noop() {
        let mut f = FrameState::new(4, 10);
        let ptr_before = f.cells().as_ptr();
        f.resize(4, 10); // same size
        let ptr_after = f.cells().as_ptr();
        assert_eq!(ptr_before, ptr_after); // no realloc
    }

    #[test]
    fn frame_state_set_cursor_clamps() {
        let mut f = FrameState::new(4, 10);
        f.set_cursor(100, 200);
        assert_eq!(f.cursor_row(), 3);
        assert_eq!(f.cursor_col(), 9);
    }

    #[test]
    fn frame_state_set_title() {
        let mut f = FrameState::new(2, 2);
        assert!(f.set_title("new title"));
        assert_eq!(f.title(), "new title");
        assert!(!f.set_title("new title")); // same title returns false
        assert!(f.set_title("other"));
    }

    #[test]
    fn frame_state_get_text_and_write_text() {
        let mut f = FrameState::new(2, 10);
        f.write_text(0, 0, "Hello", CellStyle::default());
        f.write_text(1, 0, "World", CellStyle::default());
        let text = f.get_text(0, 0, 1, 9);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        let all = f.get_all_text();
        assert!(all.contains("Hello"));
    }

    #[test]
    fn frame_state_get_text_empty() {
        let f = FrameState::new(0, 0);
        assert_eq!(f.get_text(0, 0, 0, 0), "");
        assert_eq!(f.get_all_text(), "");
    }

    #[test]
    fn frame_state_get_cell() {
        let f = FrameState::new(2, 4);
        let cell = f.get_cell(0, 0);
        assert_eq!(cell.len(), CELL_SIZE);
        // Out of bounds
        assert!(f.get_cell(100, 100).is_empty());
    }

    #[test]
    fn frame_state_cell_content_blank() {
        let f = FrameState::new(2, 4);
        assert_eq!(f.cell_content(0, 0), " "); // blank cell
        assert_eq!(f.cell_content(100, 0), ""); // out of bounds
    }

    #[test]
    fn frame_state_cell_content_with_text() {
        let mut f = FrameState::new(2, 10);
        f.write_text(0, 0, "A", CellStyle::default());
        assert_eq!(f.cell_content(0, 0), "A");
    }

    #[test]
    fn frame_state_fill_rect() {
        let mut f = FrameState::new(4, 10);
        f.fill_rect(Rect::new(0, 0, 2, 5), 'X', CellStyle::default());
        assert_eq!(f.cell_content(0, 0), "X");
        assert_eq!(f.cell_content(1, 4), "X");
        assert_eq!(f.cell_content(2, 0), " "); // outside rect
    }

    #[test]
    fn frame_state_wrapped_text() {
        let mut f = FrameState::new(4, 10);
        let lines =
            f.write_wrapped_text(Rect::new(0, 0, 4, 5), "hello world", CellStyle::default());
        assert!(lines >= 2); // "hello world" wraps at width 5
    }

    #[test]
    fn frame_state_wrapped_text_empty_rect() {
        let mut f = FrameState::new(4, 10);
        assert_eq!(
            f.write_wrapped_text(Rect::new(0, 0, 0, 0), "hi", CellStyle::default()),
            0
        );
    }

    #[test]
    fn frame_state_scrolling_text() {
        let mut f = FrameState::new(4, 10);
        f.write_scrolling_text(
            Rect::new(0, 0, 3, 10),
            &["line1", "line2", "line3", "line4"],
            0,
            CellStyle::default(),
        );
        // Last 3 lines visible with offset_from_bottom=0
        assert_eq!(f.cell_content(0, 0), "l"); // "line2"
    }

    #[test]
    fn frame_state_scrolling_text_empty_rect() {
        let mut f = FrameState::new(4, 10);
        f.write_scrolling_text(Rect::new(0, 0, 0, 0), &["hi"], 0, CellStyle::default());
        // Should not panic
    }

    #[test]
    fn frame_state_clear() {
        let mut f = FrameState::new(2, 4);
        f.write_text(0, 0, "AB", CellStyle::default());
        f.clear(CellStyle::default());
        assert_eq!(f.cell_content(0, 0), " ");
    }

    // --- TerminalState tests ---

    #[test]
    fn terminal_state_accessors() {
        let t = TerminalState::new(24, 80);
        assert_eq!(t.rows(), 24);
        assert_eq!(t.cols(), 80);
        assert_eq!(t.cursor_row(), 0);
        assert_eq!(t.cursor_col(), 0);
        assert_eq!(t.mode(), 0);
        assert_eq!(t.title(), "");
        assert_eq!(t.cells().len(), 24 * 80 * CELL_SIZE);
        assert_eq!(t.frame().rows(), 24);
    }

    #[test]
    fn terminal_state_mutators() {
        let mut t = TerminalState::new(4, 10);
        t.frame_mut().set_title("test");
        assert_eq!(t.title(), "test");
    }

    #[test]
    fn terminal_state_set_title() {
        let mut t = TerminalState::new(4, 10);
        assert!(t.frame_mut().set_title("hello"));
        assert_eq!(t.title(), "hello");
        assert!(!t.frame_mut().set_title("hello")); // same
    }

    #[test]
    fn terminal_state_get_text() {
        let t = TerminalState::new(2, 10);
        let text = t.get_text(0, 0, 0, 9);
        assert!(text.is_empty() || text.chars().all(|c| c == ' ' || c == '\n'));
        assert!(t.get_cell(0, 0).len() == CELL_SIZE);
        assert!(t.get_cell(100, 100).is_empty());
    }

    #[test]
    fn terminal_state_resize() {
        let mut t = TerminalState::new(4, 10);
        t.frame_mut().resize(2, 5);
        // Note: TerminalState.dirty isn't updated by frame_mut().resize()
        // directly — that happens through feed_compressed. So just check frame.
        assert_eq!(t.rows(), 2);
        assert_eq!(t.cols(), 5);
    }

    #[test]
    fn terminal_state_feed_compressed_invalid() {
        let mut t = TerminalState::new(4, 10);
        assert!(!t.feed_compressed(b"garbage"));
        assert!(!t.feed_compressed(&[]));
    }

    #[test]
    fn terminal_state_feed_compressed_batch_empty() {
        let mut t = TerminalState::new(4, 10);
        assert!(!t.feed_compressed_batch(&[]));
    }

    #[test]
    fn terminal_state_feed_compressed_batch_truncated() {
        let mut t = TerminalState::new(4, 10);
        // Length header says 100 bytes but only 4 bytes present
        let batch = &[100, 0, 0, 0];
        assert!(!t.feed_compressed_batch(batch));
    }

    // --- Client message builder tests ---

    #[test]
    fn msg_input_format() {
        let msg = msg_input(5, b"hello");
        assert_eq!(msg[0], C2S_INPUT);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 5);
        assert_eq!(&msg[3..], b"hello");
    }

    #[test]
    fn msg_resize_format() {
        let msg = msg_resize(3, 24, 80);
        assert_eq!(msg[0], C2S_RESIZE);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 3);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 24);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 80);
    }

    #[test]
    fn msg_resize_batch_format() {
        let msg = msg_resize_batch(&[(3, 24, 80), (5, 40, 120)]);
        assert_eq!(msg[0], C2S_RESIZE);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 3);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 24);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 80);
        assert_eq!(u16::from_le_bytes([msg[7], msg[8]]), 5);
        assert_eq!(u16::from_le_bytes([msg[9], msg[10]]), 40);
        assert_eq!(u16::from_le_bytes([msg[11], msg[12]]), 120);
    }

    #[test]
    fn msg_focus_format() {
        let msg = msg_focus(7);
        assert_eq!(msg[0], C2S_FOCUS);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 7);
        assert_eq!(msg.len(), 3);
    }

    #[test]
    fn msg_close_format() {
        let msg = msg_close(9);
        assert_eq!(msg[0], C2S_CLOSE);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 9);
    }

    #[test]
    fn msg_subscribe_unsubscribe_format() {
        let sub = msg_subscribe(1);
        assert_eq!(sub[0], C2S_SUBSCRIBE);
        assert_eq!(u16::from_le_bytes([sub[1], sub[2]]), 1);

        let unsub = msg_unsubscribe(2);
        assert_eq!(unsub[0], C2S_UNSUBSCRIBE);
        assert_eq!(u16::from_le_bytes([unsub[1], unsub[2]]), 2);
    }

    #[test]
    fn msg_search_format() {
        let msg = msg_search(42, "test query");
        assert_eq!(msg[0], C2S_SEARCH);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 42);
        assert_eq!(&msg[3..], b"test query");
    }

    #[test]
    fn msg_ack_format() {
        let msg = msg_ack();
        assert_eq!(msg, vec![C2S_ACK]);
    }

    #[test]
    fn msg_scroll_format() {
        let msg = msg_scroll(5, 1000);
        assert_eq!(msg[0], C2S_SCROLL);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 5);
        assert_eq!(u32::from_le_bytes([msg[3], msg[4], msg[5], msg[6]]), 1000);
    }

    #[test]
    fn msg_display_rate_format() {
        let msg = msg_display_rate(120);
        assert_eq!(msg[0], C2S_DISPLAY_RATE);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 120);
    }

    #[test]
    fn msg_client_metrics_format() {
        let msg = msg_client_metrics(3, 5, 100);
        assert_eq!(msg[0], C2S_CLIENT_METRICS);
        assert_eq!(u16::from_le_bytes([msg[1], msg[2]]), 3);
        assert_eq!(u16::from_le_bytes([msg[3], msg[4]]), 5);
        assert_eq!(u16::from_le_bytes([msg[5], msg[6]]), 100);
    }

    // --- CallbackRenderer tests ---

    #[test]
    fn callback_renderer_resize() {
        let mut r = CallbackRenderer::new(2, 8);
        assert_eq!(r.frame().rows(), 2);
        r.resize(4, 16);
        assert_eq!(r.frame().rows(), 4);
        assert_eq!(r.frame().cols(), 16);
    }

    #[test]
    fn callback_renderer_fill() {
        let mut r = CallbackRenderer::new(4, 10);
        r.render(|dom| {
            dom.fill(Rect::new(0, 0, 2, 5), '#', CellStyle::default());
        });
        assert_eq!(r.frame().cell_content(0, 0), "#");
        assert_eq!(r.frame().cell_content(1, 4), "#");
    }

    #[test]
    fn callback_renderer_text() {
        let mut r = CallbackRenderer::new(4, 20);
        r.render(|dom| {
            dom.text(0, 0, "Hello", CellStyle::default());
        });
        assert_eq!(r.frame().cell_content(0, 0), "H");
        assert_eq!(r.frame().cell_content(0, 4), "o");
    }

    #[test]
    fn callback_renderer_set_title() {
        let mut r = CallbackRenderer::new(2, 8);
        r.render(|dom| {
            dom.set_title("my title");
        });
        assert_eq!(r.frame().title(), "my title");
    }

    #[test]
    fn callback_renderer_set_background() {
        let mut r = CallbackRenderer::new(2, 4);
        let style = CellStyle {
            bg: Color::Rgb(255, 0, 0),
            ..CellStyle::default()
        };
        r.render(|dom| {
            dom.set_background(style);
        });
        // Background fill should have been applied to all cells
        assert_eq!(r.frame().cells().len(), 2 * 4 * CELL_SIZE);
    }

    #[test]
    fn callback_renderer_scrolling_text() {
        let mut r = CallbackRenderer::new(4, 20);
        r.render(|dom| {
            dom.scrolling_text(
                Rect::new(0, 0, 3, 20),
                ["a", "b", "c", "d", "e"].map(String::from),
                0,
                CellStyle::default(),
            );
        });
        // Should show the last 3 lines
        assert_eq!(r.frame().cell_content(0, 0), "c");
    }

    // --- parse_server_msg edge cases ---

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_server_msg(&[]).is_none());
    }

    #[test]
    fn parse_unknown_type_returns_none() {
        assert!(parse_server_msg(&[0xFF, 0x00, 0x00]).is_none());
    }

    #[test]
    fn parse_update_too_short() {
        assert!(parse_server_msg(&[S2C_UPDATE, 0x00]).is_none());
    }

    #[test]
    fn parse_closed() {
        let msg = parse_server_msg(&[S2C_CLOSED, 0x05, 0x00]).unwrap();
        match msg {
            ServerMsg::Closed { pty_id } => assert_eq!(pty_id, 5),
            _ => panic!("expected Closed"),
        }
    }

    #[test]
    fn parse_title() {
        let mut wire = vec![S2C_TITLE, 0x01, 0x00];
        wire.extend_from_slice(b"mytitle");
        let msg = parse_server_msg(&wire).unwrap();
        match msg {
            ServerMsg::Title { pty_id, title } => {
                assert_eq!(pty_id, 1);
                assert_eq!(title, b"mytitle");
            }
            _ => panic!("expected Title"),
        }
    }

    // --- build_update_msg round-trip ---

    #[test]
    fn build_update_msg_round_trip_with_resize() {
        let style = CellStyle::default();
        let mut prev = FrameState::new(2, 4);
        prev.write_text(0, 0, "AB", style);

        let mut next = FrameState::new(3, 5); // different size
        next.write_text(0, 0, "XY", style);
        next.set_title("resized");

        let msg = build_update_msg(1, &next, &prev).unwrap();
        assert!(!msg.is_empty());

        // Apply to a terminal
        let mut t = TerminalState::new(2, 4);
        assert!(t.feed_compressed(&msg[3..])); // skip pty_id header
        assert_eq!(t.rows(), 3);
        assert_eq!(t.cols(), 5);
        assert_eq!(t.title(), "resized");
    }

    #[test]
    fn build_update_msg_cursor_change() {
        let mut prev = FrameState::new(4, 10);
        prev.set_cursor(0, 0);

        let mut next = prev.clone();
        next.set_cursor(2, 5);

        let msg = build_update_msg(0, &next, &prev).unwrap();

        let mut t = TerminalState::new(4, 10);
        assert!(t.feed_compressed(&msg[3..]));
        assert_eq!(t.cursor_row(), 2);
        assert_eq!(t.cursor_col(), 5);
    }

    #[test]
    fn build_update_msg_mode_change() {
        let prev = FrameState::new(2, 4);
        let mut next = prev.clone();
        next.set_mode(0x0F);

        let msg = build_update_msg(0, &next, &prev).unwrap();
        let mut t = TerminalState::new(2, 4);
        assert!(t.feed_compressed(&msg[3..]));
        assert_eq!(t.mode(), 0x0F);
    }

    #[test]
    fn feed_compressed_batch_multiple_frames() {
        let style = CellStyle::default();
        let prev = FrameState::new(2, 4);

        let mut mid = prev.clone();
        mid.write_text(0, 0, "AB", style);
        let msg1 = build_update_msg(0, &mid, &prev).unwrap();

        let mut next = mid.clone();
        next.write_text(1, 0, "CD", style);
        let msg2 = build_update_msg(0, &next, &mid).unwrap();

        // Build batch: [len1:4][compressed1][len2:4][compressed2]
        let payload1 = &msg1[3..];
        let payload2 = &msg2[3..];
        let mut batch = Vec::new();
        batch.extend_from_slice(&(payload1.len() as u32).to_le_bytes());
        batch.extend_from_slice(payload1);
        batch.extend_from_slice(&(payload2.len() as u32).to_le_bytes());
        batch.extend_from_slice(payload2);

        let mut t = TerminalState::new(2, 4);
        assert!(t.feed_compressed_batch(&batch));
        let text = t.get_all_text();
        assert!(text.contains("AB"));
        assert!(text.contains("CD"));
    }
}
