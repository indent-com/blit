use std::collections::BTreeMap;

use lz4_flex::{compress_prepend_size, decompress_size_prepended};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const CELL_SIZE: usize = 12;
const TITLE_PRESENT: u16 = 1 << 15;
const OPS_PRESENT: u16 = 1 << 14;
const STRINGS_PRESENT: u16 = 1 << 13;
const TITLE_LEN_MASK: u16 = STRINGS_PRESENT - 1;

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
pub const C2S_RESIZE: u8 = 0x01;
pub const C2S_SCROLL: u8 = 0x02;
pub const C2S_ACK: u8 = 0x03;
pub const C2S_DISPLAY_RATE: u8 = 0x04;
pub const C2S_CLIENT_METRICS: u8 = 0x05;
pub const C2S_CREATE: u8 = 0x10;
pub const C2S_FOCUS: u8 = 0x11;
pub const C2S_CLOSE: u8 = 0x12;
pub const C2S_SUBSCRIBE: u8 = 0x13;
pub const C2S_UNSUBSCRIBE: u8 = 0x14;
pub const C2S_SEARCH: u8 = 0x15;

pub const S2C_UPDATE: u8 = 0x00;
pub const S2C_CREATED: u8 = 0x01;
pub const S2C_CLOSED: u8 = 0x02;
pub const S2C_LIST: u8 = 0x03;
pub const S2C_TITLE: u8 = 0x04;
pub const S2C_SEARCH_RESULTS: u8 = 0x05;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Default for Color {
    fn default() -> Self {
        Self::Default
    }
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
                break;
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
        let mut wrapped = Vec::new();
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
            if row < end_row.min(self.rows.saturating_sub(1)) {
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
        let width = UnicodeWidthChar::width(ch).unwrap_or(1).max(1) as u16;
        let width = if width > 1 && col + 1 < self.cols {
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
    dirty: Vec<bool>,
    all_dirty: bool,
}

impl TerminalState {
    pub fn new(rows: u16, cols: u16) -> Self {
        let frame = FrameState::new(rows, cols);
        let total = rows as usize * cols as usize;
        Self {
            frame,
            dirty: vec![true; total],
            all_dirty: true,
        }
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

    pub fn dirty_flags(&self) -> &[bool] {
        &self.dirty
    }

    pub fn dirty_flags_mut(&mut self) -> &mut [bool] {
        &mut self.dirty
    }

    pub fn all_dirty(&self) -> bool {
        self.all_dirty
    }

    pub fn clear_all_dirty(&mut self) {
        self.all_dirty = false;
        self.dirty.fill(false);
    }

    pub fn mark_all_dirty(&mut self) {
        self.all_dirty = true;
        self.dirty.fill(true);
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
        if new_rows != self.frame.rows || new_cols != self.frame.cols {
            self.frame.resize(new_rows, new_cols);
            let total = new_rows as usize * new_cols as usize;
            self.dirty = vec![true; total];
            self.all_dirty = true;
        }

        let total_cells = self.frame.rows as usize * self.frame.cols as usize;
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

        if strings_present {
            self.apply_overflow_strings(&payload[ops_end..]);
        }

        if !self.all_dirty {
            let cursor_moved = new_cursor_row != old_cursor_row || new_cursor_col != old_cursor_col;
            let vis_changed = (new_mode & 1) != (old_mode & 1);
            if cursor_moved || vis_changed {
                let old_idx =
                    old_cursor_row as usize * self.frame.cols as usize + old_cursor_col as usize;
                if old_idx < total_cells {
                    self.dirty[old_idx] = true;
                }
            }
            let new_idx =
                new_cursor_row as usize * self.frame.cols as usize + new_cursor_col as usize;
            if new_idx < total_cells {
                self.dirty[new_idx] = true;
            }
        }

        self.frame.cursor_row = new_cursor_row;
        self.frame.cursor_col = new_cursor_col;
        self.frame.mode = new_mode;
        resized
            || title_changed
            || content_changed
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
            if !self.all_dirty {
                self.dirty[i] = true;
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

        // Apply copied overflow strings.
        for (idx, s) in overflow_temp {
            self.frame.overflow.insert(idx, s);
        }

        self.mark_rect_dirty(dst_row, dst_col, rows, cols);
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
                self.frame.overflow.remove(&(r as usize * frame_cols + c as usize));
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
        self.mark_rect_dirty(row, col, row_end - row, col_end - col);
        true
    }

    /// Parse and apply overflow string table: [u16 count] [for each: u32 cell_index, u16 len, utf8]
    fn apply_overflow_strings(&mut self, data: &[u8]) {
        if data.len() < 2 {
            return;
        }
        let count = u16::from_le_bytes([data[0], data[1]]) as usize;
        let mut off = 2usize;
        for _ in 0..count {
            if off + 6 > data.len() {
                break;
            }
            let cell_idx = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
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
    }

    fn mark_rect_dirty(&mut self, row: u16, col: u16, rows: u16, cols: u16) {
        if self.all_dirty {
            return;
        }
        let row_end = row.saturating_add(rows).min(self.frame.rows);
        let col_end = col.saturating_add(cols).min(self.frame.cols);
        for r in row..row_end {
            let start = r as usize * self.frame.cols as usize + col as usize;
            let end = r as usize * self.frame.cols as usize + col_end as usize;
            self.dirty[start..end].fill(true);
        }
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
    Update { pty_id: u16, payload: &'a [u8] },
    Created { pty_id: u16 },
    Closed { pty_id: u16 },
    List { pty_ids: Vec<u16> },
    Title { pty_id: u16, title: &'a [u8] },
    SearchResults { request_id: u16, results: Vec<SearchResultEntry<'a>> },
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
            Some(ServerMsg::Created {
                pty_id: u16::from_le_bytes([data[1], data[2]]),
            })
        }
        S2C_CLOSED => {
            if data.len() < 3 {
                return None;
            }
            Some(ServerMsg::Closed {
                pty_id: u16::from_le_bytes([data[1], data[2]]),
            })
        }
        S2C_LIST => {
            if data.len() < 3 {
                return None;
            }
            let count = u16::from_le_bytes([data[1], data[2]]) as usize;
            let mut pty_ids = Vec::with_capacity(count);
            for chunk in data[3..].chunks_exact(2).take(count) {
                pty_ids.push(u16::from_le_bytes([chunk[0], chunk[1]]));
            }
            Some(ServerMsg::List { pty_ids })
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
        _ => None,
    }
}

pub fn msg_create(rows: u16, cols: u16) -> Vec<u8> {
    vec![
        C2S_CREATE,
        (rows & 0xff) as u8,
        (rows >> 8) as u8,
        (cols & 0xff) as u8,
        (cols >> 8) as u8,
    ]
}

pub fn msg_create_command(rows: u16, cols: u16, command: &str) -> Vec<u8> {
    let mut msg = msg_create(rows, cols);
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

    if ENABLE_SCROLL_OPS
        && same_size
        && previous.cells != current.cells
        && mode_is_cooked(current.mode)
        && mode_is_cooked(previous.mode)
    {
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

    let title_bytes = if title_changed {
        current.title.as_bytes()
    } else {
        &[]
    };
    let title_len = title_bytes.len().min(TITLE_LEN_MASK as usize);
    let title_field = OPS_PRESENT
        | if has_overflow { STRINGS_PRESENT } else { 0 }
        | if title_changed {
            TITLE_PRESENT | title_len as u16
        } else {
            0
        };

    let mut payload =
        Vec::with_capacity(12 + title_len + 2 + ops.len() + overflow_section.len());
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
    let mut out = Vec::new();
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
    let delta = delta_rows.unsigned_abs() as u16;
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
    let delta = delta_rows.unsigned_abs() as u16;
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

        out.push(OP_FILL_RECT);
        out.extend_from_slice(&(row as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&((end - row) as u16).to_le_bytes());
        out.extend_from_slice(&current.cols.to_le_bytes());
        out.extend_from_slice(&cell);
        *op_count += 1;

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
    fn raw_scroll_heavy_update_falls_back_to_patch_op() {
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
        assert_eq!(decoded[14], OP_PATCH_CELLS);
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

        let ServerMsg::SearchResults { request_id, results } = parse_server_msg(&msg).unwrap() else {
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
}
