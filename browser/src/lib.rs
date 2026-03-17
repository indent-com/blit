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

    // mode bits: 0=cursor_vis, 1=app_cursor, 2=app_keypad, 3=bracketed_paste, 4-6=mouse_mode, 7-8=mouse_enc
    pub fn mouse_mode(&self) -> u8 {
        ((self.mode >> 4) & 7) as u8
    }
    pub fn mouse_encoding(&self) -> u8 {
        ((self.mode >> 7) & 3) as u8
    }
    pub fn app_cursor(&self) -> bool {
        self.mode & 2 != 0
    }
    pub fn bracketed_paste(&self) -> bool {
        self.mode & 8 != 0
    }
    pub fn echo(&self) -> bool {
        self.mode & (1 << 9) != 0
    }
    pub fn icanon(&self) -> bool {
        self.mode & (1 << 10) != 0
    }
    #[wasm_bindgen(getter)]
    pub fn cursor_row(&self) -> u16 {
        self.cursor_row
    }
    #[wasm_bindgen(getter)]
    pub fn cursor_col(&self) -> u16 {
        self.cursor_col
    }

    /// Feed LZ4-compressed binary update.
    /// Header (10): rows(2) + cols(2) + cursor(4) + mode(2)
    /// Bitmask: ceil(rows*cols/8) bytes
    /// Cells: popcount × 12 bytes
    pub fn feed_compressed(&mut self, data: &[u8]) {
        let payload = match lz4_flex::decompress_size_prepended(data) {
            Ok(d) => d,
            Err(_) => return,
        };
        if payload.len() < 10 {
            return;
        }

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
        if payload.len() < 10 + bitmask_len {
            return;
        }

        let bitmask = &payload[10..10 + bitmask_len];
        let mut cell_off = 10 + bitmask_len;

        for i in 0..total_cells {
            if bitmask[i / 8] & (1 << (i % 8)) != 0 {
                if cell_off + CELL_SIZE > payload.len() {
                    break;
                }
                let idx = i * CELL_SIZE;
                self.cells[idx..idx + CELL_SIZE]
                    .copy_from_slice(&payload[cell_off..cell_off + CELL_SIZE]);
                if !self.all_dirty {
                    self.dirty[i] = true;
                }
                cell_off += CELL_SIZE;
            }
        }

        // Mark cursor positions dirty when cursor moves or visibility changes
        if !self.all_dirty {
            let cursor_moved =
                new_cursor_row != self.cursor_row || new_cursor_col != self.cursor_col;
            let vis_changed = (new_mode & 1) != (self.mode & 1);
            if cursor_moved || vis_changed {
                let old_idx =
                    self.cursor_row as usize * self.cols as usize + self.cursor_col as usize;
                if old_idx < total_cells {
                    self.dirty[old_idx] = true;
                }
            }
            let new_idx =
                new_cursor_row as usize * self.cols as usize + new_cursor_col as usize;
            if new_idx < total_cells {
                self.dirty[new_idx] = true;
            }
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
            let c1 = if row == end_row {
                end_col
            } else {
                self.cols - 1
            };
            let mut line = String::new();
            let mut col = c0;
            while col <= c1.min(self.cols - 1) {
                let idx = (row as usize * self.cols as usize + col as usize) * CELL_SIZE;
                let cell = &self.cells[idx..idx + CELL_SIZE];
                let f0 = cell[0];
                let f1 = cell[1];
                if f1 & 4 != 0 {
                    col += 1;
                    continue;
                }
                let fg_type = f0 & 3;
                let bg_type = (f0 >> 2) & 3;
                let bold = f0 & (1 << 4) != 0;
                let dim = f0 & (1 << 5) != 0;
                let italic = f0 & (1 << 6) != 0;
                let underline = f0 & (1 << 7) != 0;
                let inverse = f1 & 1 != 0;
                let content_len = ((f1 >> 3) & 7) as usize;

                let (fg, bg) = if inverse {
                    (
                        color_css(bg_type, cell[5], cell[6], cell[7], "#000", dim),
                        color_css(fg_type, cell[2], cell[3], cell[4], "#ccc", false),
                    )
                } else {
                    (
                        color_css(fg_type, cell[2], cell[3], cell[4], "#ccc", dim),
                        color_css(bg_type, cell[5], cell[6], cell[7], "#000", false),
                    )
                };

                let ch = if content_len > 0 {
                    std::str::from_utf8(&cell[8..8 + content_len])
                        .unwrap_or(" ")
                        .to_string()
                } else {
                    " ".to_string()
                };

                let has_style = fg != "#ccc" || bg != "#000" || bold || italic || underline;
                if has_style {
                    let mut style = String::new();
                    if fg != "#ccc" {
                        style.push_str("color:");
                        style.push_str(&fg);
                        style.push(';');
                    }
                    if bg != "#000" {
                        style.push_str("background:");
                        style.push_str(&bg);
                        style.push(';');
                    }
                    if bold {
                        style.push_str("font-weight:bold;");
                    }
                    if italic {
                        style.push_str("font-style:italic;");
                    }
                    if underline {
                        style.push_str("text-decoration:underline;");
                    }
                    line.push_str("<span style=\"");
                    line.push_str(&style);
                    line.push_str("\">");
                    match ch.as_str() {
                        "&" => line.push_str("&amp;"),
                        "<" => line.push_str("&lt;"),
                        ">" => line.push_str("&gt;"),
                        _ => line.push_str(&ch),
                    }
                    line.push_str("</span>");
                } else {
                    match ch.as_str() {
                        "&" => line.push_str("&amp;"),
                        "<" => line.push_str("&lt;"),
                        ">" => line.push_str("&gt;"),
                        _ => line.push_str(&ch),
                    }
                }
                col += 1;
            }
            let trimmed = line.trim_end();
            html.push_str(trimmed);
            if row < end_row.min(self.rows - 1) {
                html.push('\n');
            }
        }
        html.push_str("</pre>");
        html
    }

    pub fn get_text(&self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> String {
        let mut result = String::new();
        for row in start_row..=end_row.min(self.rows - 1) {
            let c0 = if row == start_row { start_col } else { 0 };
            let c1 = if row == end_row {
                end_col
            } else {
                self.cols - 1
            };
            let mut line = String::new();
            let mut col = c0;
            while col <= c1.min(self.cols - 1) {
                let idx = (row as usize * self.cols as usize + col as usize) * CELL_SIZE;
                let f1 = self.cells[idx + 1];
                if f1 & 4 != 0 {
                    // wide continuation
                    col += 1;
                    continue;
                }
                let content_len = ((f1 >> 3) & 7) as usize;
                if content_len > 0 {
                    if let Ok(s) =
                        std::str::from_utf8(&self.cells[idx + 8..idx + 8 + content_len])
                    {
                        line.push_str(s);
                    }
                } else {
                    line.push(' ');
                }
                col += 1;
            }
            let trimmed = line.trim_end();
            result.push_str(trimmed);
            if row < end_row.min(self.rows - 1) {
                result.push('\n');
            }
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
        ctx.set_font(&normal_font);
        ctx.set_text_baseline("bottom");

        let do_all = self.all_dirty;
        if do_all {
            ctx.set_fill_style_str("#000");
            ctx.fill_rect(0.0, 0.0, cols as f64 * cw, self.rows as f64 * ch);
            self.all_dirty = false;
        }

        for i in 0..total {
            if !do_all && !self.dirty[i] {
                continue;
            }
            self.dirty[i] = false;

            let row = i / cols;
            let col = i % cols;
            let idx = i * CELL_SIZE;
            let x = col as f64 * cw;
            let y = row as f64 * ch;

            let f0 = self.cells[idx];
            let f1 = self.cells[idx + 1];

            if f1 & 4 != 0 {
                // wide continuation — only needs clearing in partial mode
                if !do_all {
                    ctx.set_fill_style_str("#000");
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

            let (fg_color, bg_color) = {
                let cell = &self.cells[idx..idx + CELL_SIZE];
                if inverse {
                    (
                        color_css(bg_type, cell[5], cell[6], cell[7], "#000", dim),
                        color_css(fg_type, cell[2], cell[3], cell[4], "#ccc", false),
                    )
                } else {
                    (
                        color_css(fg_type, cell[2], cell[3], cell[4], "#ccc", dim),
                        color_css(bg_type, cell[5], cell[6], cell[7], "#000", false),
                    )
                }
            };

            let cell_w = if wide { cw * 2.0 } else { cw };

            // In partial mode, clear the cell area first
            if !do_all {
                ctx.set_fill_style_str("#000");
                ctx.fill_rect(x, y, cell_w, ch);
            }

            if bg_color != "#000" {
                ctx.set_fill_style_str(&bg_color);
                ctx.fill_rect(x, y, cell_w, ch);
            }

            if cursor_visible
                && self.cursor_row == row as u16
                && self.cursor_col == col as u16
            {
                ctx.set_fill_style_str("rgba(204,204,204,0.5)");
                ctx.fill_rect(x, y, cw, ch);
            }

            if content_len > 0 {
                let content =
                    std::str::from_utf8(&self.cells[idx + 8..idx + 8 + content_len])
                        .unwrap_or("");
                if !content.is_empty() && content != " " {
                    ctx.save();
                    ctx.begin_path();
                    ctx.rect(x, y, cell_w, ch);
                    ctx.clip();

                    if bold || italic {
                        let mut font = String::new();
                        if bold {
                            font.push_str("bold ");
                        }
                        if italic {
                            font.push_str("italic ");
                        }
                        font.push_str(&normal_font);
                        ctx.set_font(&font);
                    }

                    ctx.set_fill_style_str(&fg_color);
                    let _ = ctx.fill_text(content, x, y + ch);

                    if underline {
                        ctx.set_stroke_style_str(&fg_color);
                        ctx.begin_path();
                        ctx.move_to(x, y + ch - 1.0);
                        ctx.line_to(x + cell_w, y + ch - 1.0);
                        ctx.stroke();
                    }

                    ctx.restore();
                }
            }
        }
    }
}
