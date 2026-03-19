use wasm_bindgen::prelude::*;
use web_sys::CanvasRenderingContext2d;

const CELL_SIZE: usize = 12;

const ANSI_COLORS: [[u8; 3]; 16] = [
    [0, 0, 0],
    [170, 0, 0],
    [0, 170, 0],
    [170, 85, 0],
    [0, 0, 170],
    [170, 0, 170],
    [0, 170, 170],
    [170, 170, 170],
    [85, 85, 85],
    [255, 85, 85],
    [85, 255, 85],
    [255, 255, 85],
    [85, 85, 255],
    [255, 85, 255],
    [85, 255, 255],
    [255, 255, 255],
];

fn idx_to_rgb(idx: u8) -> (u8, u8, u8) {
    if idx < 16 {
        let c = ANSI_COLORS[idx as usize];
        return (c[0], c[1], c[2]);
    }
    if idx < 232 {
        let i = idx - 16;
        let r = (i / 36) * 51;
        let g = ((i % 36) / 6) * 51;
        let b = (i % 6) * 51;
        return (r, g, b);
    }
    let v = 8 + (idx - 232) * 10;
    (v, v, v)
}

/// Compact color representation — avoids String allocations in the hot render path.
#[derive(Clone, Copy, PartialEq, Eq)]
struct CellColor {
    r: u8,
    g: u8,
    b: u8,
    is_default: bool,
}

impl CellColor {
    const DEFAULT_FG: Self = Self { r: 204, g: 204, b: 204, is_default: true };
    const DEFAULT_BG: Self = Self { r: 0,   g: 0,   b: 0,   is_default: true };
    const DIM_FG:     Self = Self { r: 102, g: 102, b: 102, is_default: false };
    const DIM_BG:     Self = Self { r: 0,   g: 0,   b: 0,   is_default: false };

    fn resolve(color_type: u8, r: u8, g: u8, b: u8, is_fg: bool, dim: bool) -> Self {
        match color_type {
            0 => {
                if dim {
                    if is_fg { Self::DIM_FG } else { Self::DIM_BG }
                } else if is_fg {
                    Self::DEFAULT_FG
                } else {
                    Self::DEFAULT_BG
                }
            }
            1 => {
                let (cr, cg, cb) = idx_to_rgb(r);
                if dim {
                    Self { r: cr / 2, g: cg / 2, b: cb / 2, is_default: false }
                } else {
                    Self { r: cr, g: cg, b: cb, is_default: false }
                }
            }
            2 => {
                if dim {
                    Self { r: r / 2, g: g / 2, b: b / 2, is_default: false }
                } else {
                    Self { r, g, b, is_default: false }
                }
            }
            _ => if is_fg { Self::DEFAULT_FG } else { Self::DEFAULT_BG },
        }
    }

    fn to_css(&self) -> String {
        if self.is_default {
            if self.r == 204 { "#ccc".into() } else { "#000".into() }
        } else {
            format!("rgb({},{},{})", self.r, self.g, self.b)
        }
    }

    /// Pack into u32 for fill-style change detection (no String needed).
    fn pack(&self) -> u32 {
        ((self.is_default as u32) << 24)
            | ((self.r as u32) << 16)
            | ((self.g as u32) << 8)
            | self.b as u32
    }
}

fn color_css(color_type: u8, r: u8, g: u8, b: u8, default: &str, dim: bool) -> String {
    let (cr, cg, cb) = match color_type {
        0 => {
            return if dim {
                if default == "#ccc" {
                    "rgb(102,102,102)".into()
                } else {
                    "rgb(0,0,0)".into()
                }
            } else {
                default.into()
            };
        }
        1 => idx_to_rgb(r),
        2 => (r, g, b),
        _ => return default.into(),
    };
    if dim {
        format!("rgb({},{},{})", cr / 2, cg / 2, cb / 2)
    } else {
        format!("rgb({cr},{cg},{cb})")
    }
}

#[wasm_bindgen]
pub struct Terminal {
    rows: u16,
    cols: u16,
    cell_width: f64,
    cell_height: f64,
    cells: Vec<u8>,
    cursor_row: u16,
    cursor_col: u16,
    mode: u16,
    dirty: Vec<bool>,
    all_dirty: bool,
}

#[wasm_bindgen]
impl Terminal {
    #[wasm_bindgen(constructor)]
    pub fn new(rows: u16, cols: u16, cell_width: f64, cell_height: f64) -> Self {
        let total = rows as usize * cols as usize;
        Terminal {
            rows,
            cols,
            cell_width,
            cell_height,
            cells: vec![0u8; total * CELL_SIZE],
            cursor_row: 0,
            cursor_col: 0,
            mode: 0,
            dirty: vec![true; total],
            all_dirty: true,
        }
    }

    pub fn set_cell_size(&mut self, cell_width: f64, cell_height: f64) {
        self.cell_width = cell_width;
        self.cell_height = cell_height;
        self.all_dirty = true;
        self.dirty.fill(true);
    }

    pub fn mouse_mode(&self) -> u8 { ((self.mode >> 4) & 7) as u8 }
    pub fn mouse_encoding(&self) -> u8 { ((self.mode >> 7) & 3) as u8 }
    pub fn app_cursor(&self) -> bool { self.mode & 2 != 0 }
    pub fn bracketed_paste(&self) -> bool { self.mode & 8 != 0 }
    pub fn echo(&self) -> bool { self.mode & (1 << 9) != 0 }
    pub fn icanon(&self) -> bool { self.mode & (1 << 10) != 0 }
    #[wasm_bindgen(getter)] pub fn cursor_row(&self) -> u16 { self.cursor_row }
    #[wasm_bindgen(getter)] pub fn cursor_col(&self) -> u16 { self.cursor_col }
    #[wasm_bindgen(getter)] pub fn rows(&self) -> u16 { self.rows }
    #[wasm_bindgen(getter)] pub fn cols(&self) -> u16 { self.cols }

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
                batch[off],
                batch[off + 1],
                batch[off + 2],
                batch[off + 3],
            ]) as usize;
            off += 4;
            if off + len > batch.len() {
                break;
            }
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
        let new_cursor_row = u16::from_le_bytes([payload[4], payload[5]]);
        let new_cursor_col = u16::from_le_bytes([payload[6], payload[7]]);
        let new_mode = u16::from_le_bytes([payload[8], payload[9]]);

        if new_rows != self.rows || new_cols != self.cols {
            self.rows = new_rows;
            self.cols = new_cols;
            let total = new_rows as usize * new_cols as usize;
            self.cells = vec![0u8; total * CELL_SIZE];
            self.dirty = vec![true; total];
            self.all_dirty = true;
        }

        let total_cells = self.rows as usize * self.cols as usize;
        let bitmask_len = (total_cells + 7) / 8;
        if payload.len() < 10 + bitmask_len { return; }

        let bitmask = &payload[10..10 + bitmask_len];
        let data_start = 10 + bitmask_len;

        // Count dirty cells to compute SoA column offsets.
        let dirty_count = (0..total_cells)
            .filter(|&i| bitmask[i / 8] & (1 << (i % 8)) != 0)
            .count();
        if payload.len() < data_start + dirty_count * CELL_SIZE { return; }

        // Decode struct-of-arrays: column `byte_pos` starts at data_start + byte_pos * dirty_count.
        let mut dirty_idx = 0usize;
        for i in 0..total_cells {
            if bitmask[i / 8] & (1 << (i % 8)) != 0 {
                let cell_idx = i * CELL_SIZE;
                for byte_pos in 0..CELL_SIZE {
                    self.cells[cell_idx + byte_pos] =
                        payload[data_start + byte_pos * dirty_count + dirty_idx];
                }
                if !self.all_dirty { self.dirty[i] = true; }
                dirty_idx += 1;
            }
        }

        if !self.all_dirty {
            let cursor_moved = new_cursor_row != self.cursor_row || new_cursor_col != self.cursor_col;
            let vis_changed = (new_mode & 1) != (self.mode & 1);
            if cursor_moved || vis_changed {
                let old_idx = self.cursor_row as usize * self.cols as usize + self.cursor_col as usize;
                if old_idx < total_cells { self.dirty[old_idx] = true; }
            }
            let new_idx = new_cursor_row as usize * self.cols as usize + new_cursor_col as usize;
            if new_idx < total_cells { self.dirty[new_idx] = true; }
        }

        self.cursor_row = new_cursor_row;
        self.cursor_col = new_cursor_col;
        self.mode = new_mode;
    }

    pub fn get_html(&self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> String {
        let mut html = String::from(
            "<pre style=\"font-family:ui-monospace,monospace;background:#000;color:#ccc;padding:4px\">",
        );
        for row in start_row..=end_row.min(self.rows - 1) {
            let c0 = if row == start_row { start_col } else { 0 };
            let c1 = if row == end_row { end_col } else { self.cols - 1 };
            let mut line = String::new();
            let mut col = c0;
            while col <= c1.min(self.cols - 1) {
                let idx = (row as usize * self.cols as usize + col as usize) * CELL_SIZE;
                let cell = &self.cells[idx..idx + CELL_SIZE];
                let f0 = cell[0]; let f1 = cell[1];
                if f1 & 4 != 0 { col += 1; continue; }
                let fg_type = f0 & 3; let bg_type = (f0 >> 2) & 3;
                let bold = f0 & (1 << 4) != 0; let dim = f0 & (1 << 5) != 0;
                let italic = f0 & (1 << 6) != 0; let underline = f0 & (1 << 7) != 0;
                let inverse = f1 & 1 != 0;
                let content_len = ((f1 >> 3) & 7) as usize;
                let (fg, bg) = if inverse {
                    (color_css(bg_type, cell[5], cell[6], cell[7], "#000", dim),
                     color_css(fg_type, cell[2], cell[3], cell[4], "#ccc", false))
                } else {
                    (color_css(fg_type, cell[2], cell[3], cell[4], "#ccc", dim),
                     color_css(bg_type, cell[5], cell[6], cell[7], "#000", false))
                };
                let ch = if content_len > 0 {
                    std::str::from_utf8(&cell[8..8 + content_len]).unwrap_or(" ").to_string()
                } else { " ".to_string() };
                let has_style = fg != "#ccc" || bg != "#000" || bold || italic || underline;
                if has_style {
                    let mut style = String::new();
                    if fg != "#ccc" { style.push_str("color:"); style.push_str(&fg); style.push(';'); }
                    if bg != "#000" { style.push_str("background:"); style.push_str(&bg); style.push(';'); }
                    if bold { style.push_str("font-weight:bold;"); }
                    if italic { style.push_str("font-style:italic;"); }
                    if underline { style.push_str("text-decoration:underline;"); }
                    line.push_str("<span style=\""); line.push_str(&style); line.push_str("\">");
                    match ch.as_str() { "&" => line.push_str("&amp;"), "<" => line.push_str("&lt;"), ">" => line.push_str("&gt;"), _ => line.push_str(&ch) }
                    line.push_str("</span>");
                } else {
                    match ch.as_str() { "&" => line.push_str("&amp;"), "<" => line.push_str("&lt;"), ">" => line.push_str("&gt;"), _ => line.push_str(&ch) }
                }
                col += 1;
            }
            html.push_str(line.trim_end());
            if row < end_row.min(self.rows - 1) { html.push('\n'); }
        }
        html.push_str("</pre>");
        html
    }

    pub fn get_text(&self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> String {
        let mut result = String::new();
        for row in start_row..=end_row.min(self.rows - 1) {
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
            if row < end_row.min(self.rows - 1) { result.push('\n'); }
        }
        result
    }

    pub fn render(&mut self, ctx: &CanvasRenderingContext2d) {
        let cw = self.cell_width;
        let ch = self.cell_height;
        let cursor_visible = self.mode & 1 != 0;
        let cols = self.cols as usize;
        let total = self.rows as usize * cols;
        let normal_font = format!("{}px ui-monospace, monospace", ch as u32);

        let do_all = self.all_dirty;
        if do_all {
            ctx.set_fill_style_str("#000");
            ctx.fill_rect(0.0, 0.0, cols as f64 * cw, self.rows as f64 * ch);
            self.all_dirty = false;
        }

        // Track fill style as packed u32 to skip redundant set_fill_style_str calls.
        // Valid only if fill_known is true; starts known-black only after do_all cleared the canvas.
        let mut fill_known = do_all;
        let mut fill_packed: u32 = CellColor::DEFAULT_BG.pack();
        let mut current_font_bold = false;
        let mut current_font_italic = false;
        ctx.set_font(&normal_font);
        ctx.set_text_baseline("bottom");

        for i in 0..total {
            if !do_all && !self.dirty[i] { continue; }
            self.dirty[i] = false;

            let row = i / cols;
            let col = i % cols;
            let idx = i * CELL_SIZE;
            let x = col as f64 * cw;
            let y = row as f64 * ch;

            let f0 = self.cells[idx];
            let f1 = self.cells[idx + 1];

            if f1 & 4 != 0 {
                // wide continuation
                if !do_all {
                    let black = CellColor::DEFAULT_BG.pack();
                    if !fill_known || fill_packed != black {
                        ctx.set_fill_style_str("#000");
                        fill_packed = black;
                        fill_known = true;
                    }
                    ctx.fill_rect(x, y, cw, ch);
                }
                continue;
            }

            let fg_type = f0 & 3;
            let bg_type = (f0 >> 2) & 3;
            let bold = f0 & (1 << 4) != 0;
            let dim = f0 & (1 << 5) != 0;
            let italic = f0 & (1 << 6) != 0;
            let underline = f0 & (1 << 7) != 0;
            let inverse = f1 & 1 != 0;
            let wide = f1 & 2 != 0;
            let content_len = ((f1 >> 3) & 7) as usize;

            let cell = &self.cells[idx..idx + CELL_SIZE];
            let (fg, bg) = if inverse {
                (CellColor::resolve(bg_type, cell[5], cell[6], cell[7], false, dim),
                 CellColor::resolve(fg_type, cell[2], cell[3], cell[4], true, false))
            } else {
                (CellColor::resolve(fg_type, cell[2], cell[3], cell[4], true, dim),
                 CellColor::resolve(bg_type, cell[5], cell[6], cell[7], false, false))
            };

            let cell_w = if wide { cw * 2.0 } else { cw };
            let bg_black = bg == CellColor::DEFAULT_BG || bg == CellColor::DIM_BG;

            // Clear cell in partial mode
            if !do_all {
                let black = CellColor::DEFAULT_BG.pack();
                if !fill_known || fill_packed != black {
                    ctx.set_fill_style_str("#000");
                    fill_packed = black;
                    fill_known = true;
                }
                ctx.fill_rect(x, y, cell_w, ch);
            }

            if !bg_black {
                let p = bg.pack();
                if !fill_known || fill_packed != p {
                    let css = bg.to_css();
                    ctx.set_fill_style_str(&css);
                    fill_packed = p;
                    fill_known = true;
                }
                ctx.fill_rect(x, y, cell_w, ch);
            }

            if cursor_visible && self.cursor_row == row as u16 && self.cursor_col == col as u16 {
                ctx.set_fill_style_str("rgba(204,204,204,0.5)");
                fill_known = false; // rgba — don't try to match with pack()
                ctx.fill_rect(x, y, cw, ch);
            }

            if content_len > 0 {
                let content_bytes = &self.cells[idx + 8..idx + 8 + content_len];
                let content = std::str::from_utf8(content_bytes).unwrap_or("");
                if !content.is_empty() && content != " " {
                    // Set font and fill BEFORE save() so tracking stays valid after restore().
                    if bold != current_font_bold || italic != current_font_italic {
                        let f = if bold && italic { format!("bold italic {normal_font}") }
                                else if bold      { format!("bold {normal_font}") }
                                else if italic    { format!("italic {normal_font}") }
                                else              { normal_font.clone() };
                        ctx.set_font(&f);
                        current_font_bold = bold;
                        current_font_italic = italic;
                    }
                    let fg_p = fg.pack();
                    if !fill_known || fill_packed != fg_p {
                        ctx.set_fill_style_str(&fg.to_css());
                        fill_packed = fg_p;
                        fill_known = true;
                    }

                    // Clip to the cell so glyph overhang does not leave trails in neighbors.
                    // save/restore reverts the clip; fill+font state is unchanged.
                    ctx.save();
                    ctx.begin_path();
                    ctx.rect(x, y, cell_w, ch);
                    ctx.clip();
                    let _ = ctx.fill_text(content, x, y + ch);
                    if underline {
                        ctx.set_stroke_style_str(&fg.to_css());
                        ctx.begin_path();
                        ctx.move_to(x, y + ch - 1.0);
                        ctx.line_to(x + cell_w, y + ch - 1.0);
                        ctx.stroke();
                    }
                    ctx.restore();
                    // Canvas fill/font state is identical to before save(); tracking is still valid.
                }
            }
        }
    }
}
