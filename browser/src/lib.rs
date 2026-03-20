use std::collections::HashMap;

use blit_remote::{TerminalState, CELL_SIZE};
use wasm_bindgen::{prelude::*, JsCast};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

#[wasm_bindgen(inline_js = r#"
const glyphTextCache = new Map();

export function blitFillTextCodePoint(ctx, codePoint, x, y) {
  let text = glyphTextCache.get(codePoint);
  if (text === undefined) {
    text = String.fromCodePoint(codePoint);
    glyphTextCache.set(codePoint, text);
  }
  ctx.fillText(text, x, y);
}

export function blitFillText(ctx, text, x, y) {
  ctx.fillText(text, x, y);
}
"#)]
extern "C" {
    fn blitFillTextCodePoint(ctx: &CanvasRenderingContext2d, code_point: u32, x: f64, y: f64);
    fn blitFillText(ctx: &CanvasRenderingContext2d, text: &str, x: f64, y: f64);
}

const DEFAULT_FONT_FAMILY: &str = r#"PragmataPro, ui-monospace, monospace"#;
const BG_OP_STRIDE: usize = 4;
const ENABLE_SCROLL_COPY: bool = true;
const MODE_ECHO: u16 = 1 << 9;
const MODE_ICANON: u16 = 1 << 10;
const MODE_ALT_SCREEN: u16 = 1 << 11;

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
    const DEFAULT_FG: Self = Self {
        r: 204,
        g: 204,
        b: 204,
        is_default: true,
    };
    const DEFAULT_BG: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        is_default: true,
    };
    const DIM_FG: Self = Self {
        r: 102,
        g: 102,
        b: 102,
        is_default: false,
    };
    const DIM_BG: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        is_default: false,
    };

    fn resolve(color_type: u8, r: u8, g: u8, b: u8, is_fg: bool, dim: bool) -> Self {
        match color_type {
            0 => {
                if dim {
                    if is_fg {
                        Self::DIM_FG
                    } else {
                        Self::DIM_BG
                    }
                } else if is_fg {
                    Self::DEFAULT_FG
                } else {
                    Self::DEFAULT_BG
                }
            }
            1 => {
                let (cr, cg, cb) = idx_to_rgb(r);
                if dim {
                    Self {
                        r: cr / 2,
                        g: cg / 2,
                        b: cb / 2,
                        is_default: false,
                    }
                } else {
                    Self {
                        r: cr,
                        g: cg,
                        b: cb,
                        is_default: false,
                    }
                }
            }
            2 => {
                if dim {
                    Self {
                        r: r / 2,
                        g: g / 2,
                        b: b / 2,
                        is_default: false,
                    }
                } else {
                    Self {
                        r,
                        g,
                        b,
                        is_default: false,
                    }
                }
            }
            _ => {
                if is_fg {
                    Self::DEFAULT_FG
                } else {
                    Self::DEFAULT_BG
                }
            }
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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    bytes: [u8; 4],
    len: u8,
    bold: bool,
    italic: bool,
    underline: bool,
    wide: bool,
}

impl GlyphKey {
    fn new(
        bytes: &[u8],
        bold: bool,
        italic: bool,
        underline: bool,
        wide: bool,
    ) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > 4 {
            return None;
        }
        let mut packed = [0u8; 4];
        packed[..bytes.len()].copy_from_slice(bytes);
        Some(Self {
            bytes: packed,
            len: bytes.len() as u8,
            bold,
            italic,
            underline,
            wide,
        })
    }

    fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes[..self.len as usize]).ok()
    }

    fn code_point(&self) -> Option<u32> {
        let text = self.text()?;
        let mut chars = text.chars();
        let ch = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        Some(ch as u32)
    }
}

#[derive(Clone, Copy)]
struct GlyphSlot {
    src_x: f64,
    src_y: f64,
    width: f64,
    height: f64,
}

#[derive(Default)]
struct GlyphAtlas {
    canvas: Option<HtmlCanvasElement>,
    ctx: Option<CanvasRenderingContext2d>,
    slots: HashMap<GlyphKey, GlyphSlot>,
    version: u32,
    next_x: u32,
    next_y: u32,
    row_height: u32,
    cell_width: u32,
    cell_height: u32,
    wide_cell_width: u32,
}

impl GlyphAtlas {
    const MIN_SIZE: u32 = 2048;
    const MAX_SIZE: u32 = 8192;
    const PADDING: u32 = 1;

    fn invalidate(&mut self) {
        self.slots.clear();
        self.version = self.version.wrapping_add(1);
        self.next_x = Self::PADDING;
        self.next_y = Self::PADDING;
        self.row_height = 0;
        self.cell_width = 0;
        self.cell_height = 0;
        self.wide_cell_width = 0;
        if let Some(ctx) = &self.ctx {
            if let Some(c) = &self.canvas {
                ctx.clear_rect(0.0, 0.0, c.width() as f64, c.height() as f64);
            }
            ctx.set_text_baseline("bottom");
        }
    }

    fn atlas_size(&self) -> u32 {
        self.canvas.as_ref().map(|c| c.width()).unwrap_or(Self::MIN_SIZE)
    }

    fn ensure_canvas_sized(&mut self, size: u32) -> bool {
        let size = size.clamp(Self::MIN_SIZE, Self::MAX_SIZE);
        if let Some(canvas) = &self.canvas {
            if canvas.width() == size && canvas.height() == size && self.ctx.is_some() {
                return true;
            }
        }
        let Some(window) = web_sys::window() else {
            return false;
        };
        let Some(document) = window.document() else {
            return false;
        };
        let canvas = if let Some(c) = self.canvas.take() {
            c
        } else {
            let Ok(el) = document.create_element("canvas") else {
                return false;
            };
            let Ok(c) = el.dyn_into::<HtmlCanvasElement>() else {
                return false;
            };
            c
        };
        canvas.set_width(size);
        canvas.set_height(size);
        let ctx = if let Some(c) = self.ctx.take() {
            // Resizing the canvas resets context state.
            c.set_text_baseline("bottom");
            c
        } else {
            let Ok(Some(ctx)) = canvas.get_context("2d") else {
                return false;
            };
            let Ok(ctx) = ctx.dyn_into::<CanvasRenderingContext2d>() else {
                return false;
            };
            ctx.set_text_baseline("bottom");
            ctx
        };
        self.canvas = Some(canvas);
        self.ctx = Some(ctx);
        // Resize clears the canvas, so all cached slots are invalid.
        self.slots.clear();
        self.version = self.version.wrapping_add(1);
        self.next_x = Self::PADDING;
        self.next_y = Self::PADDING;
        self.row_height = 0;
        true
    }

    fn ensure_canvas(&mut self) -> bool {
        self.ensure_canvas_sized(self.atlas_size())
    }

    fn ensure_metrics(&mut self, cell_width: f64, cell_height: f64) -> bool {
        if !self.ensure_canvas() {
            return false;
        }
        let width = cell_width.ceil().max(1.0) as u32;
        let height = cell_height.ceil().max(1.0) as u32;
        let wide_width = (cell_width * 2.0).ceil().max(1.0) as u32;
        if self.cell_width != width
            || self.cell_height != height
            || self.wide_cell_width != wide_width
        {
            self.invalidate();
            self.cell_width = width;
            self.cell_height = height;
            self.wide_cell_width = wide_width;
        }
        true
    }

    /// Compute the atlas side length needed for `count` glyphs at the current
    /// cell metrics.  Returns a power-of-two clamped to MIN_SIZE..MAX_SIZE.
    fn size_for_glyphs(&self, count: usize) -> u32 {
        if count == 0 {
            return Self::MIN_SIZE;
        }
        let cw = (self.cell_width.max(1) + Self::PADDING * 3) as usize;
        let ch = (self.cell_height.max(1) + Self::PADDING * 3) as usize;
        let cols = (Self::MAX_SIZE as usize) / cw;
        if cols == 0 {
            return Self::MAX_SIZE;
        }
        let rows_needed = (count + cols - 1) / cols;
        let height_needed = rows_needed * ch;
        let mut size = Self::MIN_SIZE;
        while (size as usize) < height_needed && size < Self::MAX_SIZE {
            size *= 2;
        }
        // Also ensure width fits at least one glyph.
        while (size as usize) < cw && size < Self::MAX_SIZE {
            size *= 2;
        }
        size.clamp(Self::MIN_SIZE, Self::MAX_SIZE)
    }

    /// Ensure the atlas is large enough for `needed_glyphs` unique glyphs.
    /// Grows the backing canvas (power-of-two) if necessary, which
    /// invalidates all cached slots.
    fn ensure_capacity(&mut self, needed_glyphs: usize) -> bool {
        let required = self.size_for_glyphs(needed_glyphs);
        if required > self.atlas_size() {
            if !self.ensure_canvas_sized(required) {
                return false;
            }
        }
        true
    }

    fn allocate_slot(&mut self, render_width: u32, render_height: u32) -> Option<GlyphSlot> {
        let alloc_width = render_width + Self::PADDING * 2;
        let alloc_height = render_height + Self::PADDING * 2;
        let atlas_size = self.atlas_size();
        if alloc_width >= atlas_size || alloc_height >= atlas_size {
            return None;
        }
        if self.next_x + alloc_width >= atlas_size {
            self.next_x = Self::PADDING;
            self.next_y = self
                .next_y
                .saturating_add(self.row_height.max(Self::PADDING));
            self.row_height = 0;
        }
        if self.next_y + alloc_height >= atlas_size {
            return None;
        }
        let slot = GlyphSlot {
            src_x: (self.next_x + Self::PADDING) as f64,
            src_y: (self.next_y + Self::PADDING) as f64,
            width: render_width as f64,
            height: render_height as f64,
        };
        self.next_x = self.next_x.saturating_add(alloc_width + Self::PADDING);
        self.row_height = self.row_height.max(alloc_height + Self::PADDING);
        Some(slot)
    }

    fn ensure_glyph(
        &mut self,
        key: GlyphKey,
        normal_font: &str,
        cell_width: f64,
        cell_height: f64,
    ) -> Option<GlyphSlot> {
        self.ensure_metrics(cell_width, cell_height);
        if let Some(slot) = self.slots.get(&key) {
            return Some(*slot);
        }

        let render_width = if key.wide {
            self.wide_cell_width.max(1)
        } else {
            self.cell_width.max(1)
        };
        let render_height = self.cell_height.max(1);
        let slot = self.allocate_slot(render_width, render_height)?;
        let ctx = self.ctx.as_ref()?;
        let code_point = key.code_point()?;
        let font = if key.bold && key.italic {
            format!("bold italic {normal_font}")
        } else if key.bold {
            format!("bold {normal_font}")
        } else if key.italic {
            format!("italic {normal_font}")
        } else {
            normal_font.to_owned()
        };
        ctx.clear_rect(
            slot.src_x - Self::PADDING as f64,
            slot.src_y - Self::PADDING as f64,
            render_width as f64 + (Self::PADDING * 2) as f64,
            render_height as f64 + (Self::PADDING * 2) as f64,
        );
        let (draw_font, draw_x, draw_y) = if key.wide {
            let scale = 0.85;
            let scaled_h = cell_height * scale;
            let font_size = scaled_h.round().max(1.0) as u32;
            let scaled_font = format!("{}px {}", font_size, &font[font.find("px ").map(|i| i + 3).unwrap_or(0)..]);
            let pad_x = (render_width as f64 - render_width as f64 * scale) / 2.0;
            let pad_y = (cell_height - scaled_h) / 2.0;
            (scaled_font, slot.src_x + pad_x, slot.src_y + pad_y + scaled_h)
        } else {
            (font, slot.src_x, slot.src_y + cell_height)
        };
        ctx.set_font(&draw_font);
        // Render in white — the GL shader tints per-vertex.
        ctx.set_fill_style_str("#fff");
        ctx.save();
        ctx.begin_path();
        ctx.rect(slot.src_x, slot.src_y, slot.width, slot.height);
        ctx.clip();
        blitFillTextCodePoint(ctx, code_point, draw_x, draw_y);
        if key.underline {
            ctx.set_stroke_style_str("#fff");
            ctx.begin_path();
            ctx.move_to(slot.src_x, slot.src_y + cell_height - 1.0);
            ctx.line_to(slot.src_x + slot.width, slot.src_y + cell_height - 1.0);
            ctx.stroke();
        }
        ctx.restore();
        self.slots.insert(key, slot);
        self.version = self.version.wrapping_add(1);
        Some(slot)
    }
}

#[derive(Clone, Copy)]
struct RenderDamage {
    full: bool,
    min_row: i32,
    max_row: i32,
    min_col: i32,
    max_col: i32,
    scroll_delta_rows: i32,
}

impl Default for RenderDamage {
    fn default() -> Self {
        Self {
            full: false,
            min_row: -1,
            max_row: -1,
            min_col: -1,
            max_col: -1,
            scroll_delta_rows: 0,
        }
    }
}

impl RenderDamage {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn note_rect(&mut self, row: usize, col: usize, row_span: usize, col_span: usize) {
        if self.full {
            return;
        }
        let row = row as i32;
        let col = col as i32;
        let row_end = row + row_span as i32;
        let col_end = col + col_span as i32;
        if self.min_row < 0 {
            self.min_row = row;
            self.max_row = row_end;
            self.min_col = col;
            self.max_col = col_end;
        } else {
            self.min_row = self.min_row.min(row);
            self.max_row = self.max_row.max(row_end);
            self.min_col = self.min_col.min(col);
            self.max_col = self.max_col.max(col_end);
        }
    }
}

struct ScrollCopy {
    delta_rows: i32,
    matched_rows: Vec<bool>,
}

#[wasm_bindgen]
pub struct Terminal {
    cell_width: f64,
    cell_height: f64,
    font_family: String,
    inner: TerminalState,
    painted_cells: Vec<u8>,
    painted_rows: u16,
    painted_cols: u16,
    painted_cursor_row: u16,
    painted_cursor_col: u16,
    glyph_atlas: GlyphAtlas,
    last_render: RenderDamage,
    bg_ops: Vec<u32>,
    glyph_ops: Vec<u32>,
    /// Overflow text ops: (row, col, col_span, string) for cells whose
    /// content exceeds 4 bytes and can't use the glyph atlas.
    overflow_text_ops: Vec<(u32, u32, u32, String)>,
}

#[wasm_bindgen]
impl Terminal {
    #[wasm_bindgen(constructor)]
    pub fn new(rows: u16, cols: u16, cell_width: f64, cell_height: f64) -> Self {
        Terminal {
            cell_width,
            cell_height,
            font_family: DEFAULT_FONT_FAMILY.to_owned(),
            inner: TerminalState::new(rows, cols),
            painted_cells: Vec::new(),
            painted_rows: 0,
            painted_cols: 0,
            painted_cursor_row: 0,
            painted_cursor_col: 0,
            glyph_atlas: GlyphAtlas::default(),
            last_render: RenderDamage::default(),
            bg_ops: Vec::new(),
            glyph_ops: Vec::new(),
            overflow_text_ops: Vec::new(),
        }
    }

    pub fn set_cell_size(&mut self, cell_width: f64, cell_height: f64) {
        self.cell_width = cell_width;
        self.cell_height = cell_height;
        self.painted_rows = 0;
        self.painted_cols = 0;
        self.glyph_atlas.invalidate();
        self.inner.mark_all_dirty();
    }

    pub fn invalidate_render_cache(&mut self) {
        self.painted_rows = 0;
        self.painted_cols = 0;
        self.glyph_atlas.invalidate();
        self.inner.mark_all_dirty();
    }

    pub fn set_font_family(&mut self, font_family: &str) {
        let font_family = font_family.trim();
        let next = if font_family.is_empty() {
            DEFAULT_FONT_FAMILY
        } else {
            font_family
        };
        if self.font_family == next {
            return;
        }
        self.font_family.clear();
        self.font_family.push_str(next);
        self.invalidate_render_cache();
    }

    pub fn mouse_mode(&self) -> u8 {
        ((self.inner.mode() >> 4) & 7) as u8
    }
    pub fn mouse_encoding(&self) -> u8 {
        ((self.inner.mode() >> 7) & 3) as u8
    }
    pub fn app_cursor(&self) -> bool {
        self.inner.mode() & 2 != 0
    }
    pub fn bracketed_paste(&self) -> bool {
        self.inner.mode() & 8 != 0
    }
    pub fn echo(&self) -> bool {
        self.inner.mode() & MODE_ECHO != 0
    }
    pub fn icanon(&self) -> bool {
        self.inner.mode() & MODE_ICANON != 0
    }
    pub fn title(&self) -> String {
        self.inner.title().to_owned()
    }
    #[wasm_bindgen(getter)]
    pub fn cursor_row(&self) -> u16 {
        self.inner.cursor_row()
    }
    #[wasm_bindgen(getter)]
    pub fn cursor_col(&self) -> u16 {
        self.inner.cursor_col()
    }
    #[wasm_bindgen(getter)]
    pub fn rows(&self) -> u16 {
        self.inner.rows()
    }
    #[wasm_bindgen(getter)]
    pub fn cols(&self) -> u16 {
        self.inner.cols()
    }
    pub fn cursor_visible(&self) -> bool {
        self.inner.mode() & 1 != 0
    }
    pub fn last_render_full(&self) -> bool {
        self.last_render.full
    }
    pub fn last_render_scroll_rows(&self) -> i32 {
        self.last_render.scroll_delta_rows
    }
    pub fn last_render_damage_row_start(&self) -> i32 {
        self.last_render.min_row
    }
    pub fn last_render_damage_row_end(&self) -> i32 {
        self.last_render.max_row
    }
    pub fn last_render_damage_col_start(&self) -> i32 {
        self.last_render.min_col
    }
    pub fn last_render_damage_col_end(&self) -> i32 {
        self.last_render.max_col
    }

    pub fn feed_compressed(&mut self, data: &[u8]) {
        let _ = self.inner.feed_compressed(data);
    }

    pub fn feed_compressed_batch(&mut self, batch: &[u8]) {
        let _ = self.inner.feed_compressed_batch(batch);
    }

    pub fn prepare_render_ops(&mut self) {
        self.prepare_render_ops_inner();
    }

    pub fn prepare_full_render_ops(&mut self) {
        self.inner.mark_all_dirty();
        self.prepare_render_ops_inner();
    }

    pub fn background_ops(&self) -> Vec<u32> {
        self.bg_ops.clone()
    }

    pub fn glyph_ops(&self) -> Vec<u32> {
        self.glyph_ops.clone()
    }

    pub fn overflow_text_count(&self) -> usize {
        self.overflow_text_ops.len()
    }

    /// Returns overflow text op at index: (row, col, col_span, text).
    pub fn overflow_text_op(&self, index: usize) -> JsValue {
        if let Some((row, col, col_span, text)) = self.overflow_text_ops.get(index) {
            let arr = js_sys::Array::new_with_length(4);
            arr.set(0, JsValue::from(*row));
            arr.set(1, JsValue::from(*col));
            arr.set(2, JsValue::from(*col_span));
            arr.set(3, JsValue::from(text.as_str()));
            arr.into()
        } else {
            JsValue::NULL
        }
    }

    pub fn glyph_atlas_canvas(&self) -> Option<HtmlCanvasElement> {
        self.glyph_atlas.canvas.clone()
    }

    pub fn glyph_atlas_version(&self) -> u32 {
        self.glyph_atlas.version
    }

    pub fn get_html(&self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> String {
        let mut html = String::from(
            "<pre style=\"font-family:ui-monospace,monospace;background:#000;color:#ccc;padding:4px\">",
        );
        for row in start_row..=end_row.min(self.inner.rows().saturating_sub(1)) {
            let c0 = if row == start_row { start_col } else { 0 };
            let c1 = if row == end_row {
                end_col
            } else {
                self.inner.cols().saturating_sub(1)
            };
            let mut line = String::new();
            let mut col = c0;
            while col <= c1.min(self.inner.cols().saturating_sub(1)) {
                let idx = (row as usize * self.inner.cols() as usize + col as usize) * CELL_SIZE;
                let cell = &self.inner.cells()[idx..idx + CELL_SIZE];
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
                let flat = row as usize * self.inner.cols() as usize + col as usize;
                let ch = if content_len == 7 {
                    self.inner.frame().overflow().get(&flat)
                        .map(|s| s.as_str())
                        .unwrap_or(" ")
                        .to_string()
                } else if content_len > 0 {
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
            html.push_str(line.trim_end());
            if row < end_row.min(self.inner.rows().saturating_sub(1)) {
                html.push('\n');
            }
        }
        html.push_str("</pre>");
        html
    }

    pub fn get_text(&self, start_row: u16, start_col: u16, end_row: u16, end_col: u16) -> String {
        let mut result = String::new();
        for row in start_row..=end_row.min(self.inner.rows().saturating_sub(1)) {
            let c0 = if row == start_row { start_col } else { 0 };
            let c1 = if row == end_row {
                end_col
            } else {
                self.inner.cols().saturating_sub(1)
            };
            let mut line = String::new();
            let mut col = c0;
            while col <= c1.min(self.inner.cols().saturating_sub(1)) {
                let idx = (row as usize * self.inner.cols() as usize + col as usize) * CELL_SIZE;
                let f1 = self.inner.cells()[idx + 1];
                if f1 & 4 != 0 {
                    col += 1;
                    continue;
                }
                let content_len = ((f1 >> 3) & 7) as usize;
                if content_len > 0 {
                    if let Ok(s) =
                        std::str::from_utf8(&self.inner.cells()[idx + 8..idx + 8 + content_len])
                    {
                        line.push_str(s);
                    }
                } else {
                    line.push(' ');
                }
                col += 1;
            }
            result.push_str(line.trim_end());
            if row < end_row.min(self.inner.rows().saturating_sub(1)) {
                result.push('\n');
            }
        }
        result
    }

}

impl Terminal {
    fn cooked_mode(&self) -> bool {
        let mode = self.inner.mode();
        mode & MODE_ECHO != 0 && mode & MODE_ICANON != 0 && mode & MODE_ALT_SCREEN == 0
    }

    fn push_bg_op(&mut self, row: usize, col: usize, col_span: usize, packed: u32) {
        if self.bg_ops.len() >= BG_OP_STRIDE {
            let last = self.bg_ops.len() - BG_OP_STRIDE;
            let last_row = self.bg_ops[last] as usize;
            let last_col = self.bg_ops[last + 1] as usize;
            let last_span = self.bg_ops[last + 2] as usize;
            let last_packed = self.bg_ops[last + 3];
            if last_row == row && last_packed == packed && last_col + last_span == col {
                self.bg_ops[last + 2] += col_span as u32;
                return;
            }
        }
        self.bg_ops
            .extend_from_slice(&[row as u32, col as u32, col_span as u32, packed]);
    }

    fn push_glyph_op(&mut self, slot: GlyphSlot, row: usize, col: usize, col_span: usize, fg_packed: u32) {
        self.glyph_ops.extend_from_slice(&[
            slot.src_x as u32,
            slot.src_y as u32,
            slot.width as u32,
            slot.height as u32,
            row as u32,
            col as u32,
            col_span as u32,
            fg_packed,
        ]);
    }

    fn prepare_render_ops_inner(&mut self) {
        self.prepare_render_ops_attempt(false, 0);
    }

    fn prepare_render_ops_attempt(&mut self, force_all: bool, _retries: u32) {
        let cw = self.cell_width;
        let ch = self.cell_height;
        let rows = self.inner.rows() as usize;
        let cols = self.inner.cols() as usize;
        let total = rows * cols;
        let normal_font = format!("{}px {}", ch.round().max(1.0) as u32, self.font_family);
        let black = CellColor::DEFAULT_BG.pack();

        self.bg_ops.clear();
        self.glyph_ops.clear();
        self.overflow_text_ops.clear();
        self.last_render.reset();

        let do_all = force_all
            || self.inner.all_dirty()
            || self.painted_rows != self.inner.rows()
            || self.painted_cols != self.inner.cols()
            || self.painted_cells.len() != self.inner.cells().len();
        if do_all {
            self.inner.clear_all_dirty();
            self.last_render.full = true;
        } else if ENABLE_SCROLL_COPY && self.cooked_mode() {
            if let Some(scroll_copy) = self.detect_scroll_copy(cols, rows) {
                self.last_render.scroll_delta_rows = scroll_copy.delta_rows;
                for (row, matched) in scroll_copy.matched_rows.iter().enumerate() {
                    let row_start = row * cols;
                    if *matched {
                        self.inner.dirty_flags_mut()[row_start..row_start + cols].fill(false);
                    } else {
                        self.inner.dirty_flags_mut()[row_start..row_start + cols].fill(true);
                    }
                }
            }
        }

        if !do_all {
            let old_idx =
                self.painted_cursor_row as usize * cols + self.painted_cursor_col as usize;
            if old_idx < total {
                self.inner.dirty_flags_mut()[old_idx] = true;
            }
            let new_idx =
                self.inner.cursor_row() as usize * cols + self.inner.cursor_col() as usize;
            if new_idx < total {
                self.inner.dirty_flags_mut()[new_idx] = true;
            }
        }

        // When a continuation cell is dirty, also mark its primary wide cell
        // dirty so the glyph is repainted across both cells.  Without this,
        // the continuation's bg fill erases the right half of the emoji.
        if !do_all {
            for i in 1..total {
                if !self.inner.dirty_flags()[i] {
                    continue;
                }
                let f1 = self.inner.cells()[i * CELL_SIZE + 1];
                if f1 & 4 != 0 {
                    // This is a wide continuation — dirty the primary cell.
                    self.inner.dirty_flags_mut()[i - 1] = true;
                }
            }
        }

        // ── Pre-scan: collect unique glyph keys and size the atlas ───────
        self.glyph_atlas.ensure_metrics(cw, ch);
        let cells = self.inner.cells();
        let dirty = self.inner.dirty_flags();
        let mut needed_keys: HashMap<GlyphKey, ()> = HashMap::new();
        for i in 0..total {
            if !do_all && !dirty[i] {
                continue;
            }
            let idx = i * CELL_SIZE;
            let f0 = cells[idx];
            let f1 = cells[idx + 1];
            if f1 & 4 != 0 {
                continue; // wide continuation
            }
            let content_len = ((f1 >> 3) & 7) as usize;
            if content_len == 0 || content_len == 7 {
                continue; // empty or overflow
            }
            let content_bytes = &cells[idx + 8..idx + 8 + content_len];
            if content_bytes == b" " {
                continue;
            }
            let bold = f0 & (1 << 4) != 0;
            let italic = f0 & (1 << 6) != 0;
            let underline = f0 & (1 << 7) != 0;
            let wide = f1 & 2 != 0;
            if let Some(key) = GlyphKey::new(content_bytes, bold, italic, underline, wide) {
                if !self.glyph_atlas.slots.contains_key(&key) {
                    needed_keys.insert(key, ());
                }
            }
        }
        let total_unique = self.glyph_atlas.slots.len() + needed_keys.len();
        self.glyph_atlas.ensure_capacity(total_unique);

        // ── Main pass: generate bg + glyph ops ──────────────────────────
        let mut i = 0usize;
        while i < total {
            if !do_all && !self.inner.dirty_flags()[i] {
                i += 1;
                continue;
            }

            let row = i / cols;
            let col = i % cols;
            let idx = i * CELL_SIZE;
            self.inner.dirty_flags_mut()[i] = false;

            let mut cell = [0u8; CELL_SIZE];
            cell.copy_from_slice(&self.inner.cells()[idx..idx + CELL_SIZE]);
            let f0 = cell[0];
            let f1 = cell[1];
            let fg_type = f0 & 3;
            let bg_type = (f0 >> 2) & 3;
            let bold = f0 & (1 << 4) != 0;
            let dim = f0 & (1 << 5) != 0;
            let italic = f0 & (1 << 6) != 0;
            let underline = f0 & (1 << 7) != 0;
            let inverse = f1 & 1 != 0;
            let wide = f1 & 2 != 0;
            let content_len = ((f1 >> 3) & 7) as usize;
            let (fg, bg) = if inverse {
                (
                    CellColor::resolve(bg_type, cell[5], cell[6], cell[7], false, dim),
                    CellColor::resolve(fg_type, cell[2], cell[3], cell[4], true, false),
                )
            } else {
                (
                    CellColor::resolve(fg_type, cell[2], cell[3], cell[4], true, dim),
                    CellColor::resolve(bg_type, cell[5], cell[6], cell[7], false, false),
                )
            };
            let cell_cols = if wide { 2 } else { 1 };

            if f1 & 4 != 0 {
                self.last_render.note_rect(row, col, 1, 1);
                i += 1;
                continue;
            }

            let bg_black = bg == CellColor::DEFAULT_BG || bg == CellColor::DIM_BG;
            self.last_render.note_rect(row, col, 1, cell_cols);

            if !do_all && bg_black {
                self.push_bg_op(row, col, cell_cols, black);
            }
            if !bg_black {
                self.push_bg_op(row, col, cell_cols, bg.pack());
            }

            if content_len == 7 {
                if let Some(s) = self.inner.frame().overflow().get(&i) {
                    self.overflow_text_ops.push((
                        row as u32,
                        col as u32,
                        cell_cols as u32,
                        s.clone(),
                    ));
                }
            } else if content_len > 0 {
                let content_bytes = &cell[8..8 + content_len];
                if content_bytes != b" " {
                    if let Some(key) =
                        GlyphKey::new(content_bytes, bold, italic, underline, wide)
                    {
                        if let Some(slot) =
                            self.glyph_atlas.ensure_glyph(key, &normal_font, cw, ch)
                        {
                            self.push_glyph_op(slot, row, col, cell_cols, fg.pack());
                        }
                    }
                }
            }
            i += 1;
        }

        self.sync_painted_frame();
    }

    fn sync_painted_frame(&mut self) {
        self.painted_rows = self.inner.rows();
        self.painted_cols = self.inner.cols();
        self.painted_cursor_row = self.inner.cursor_row();
        self.painted_cursor_col = self.inner.cursor_col();
        self.painted_cells.clear();
        self.painted_cells.extend_from_slice(self.inner.cells());
    }

    fn detect_scroll_copy(&self, cols: usize, rows: usize) -> Option<ScrollCopy> {
        if rows < 3
            || cols == 0
            || self.painted_rows != self.inner.rows()
            || self.painted_cols != self.inner.cols()
            || self.painted_cells.len() != self.inner.cells().len()
        {
            return None;
        }

        let dirty_rows: Vec<bool> = (0..rows)
            .map(|row| {
                let row_start = row * cols;
                self.inner.dirty_flags()[row_start..row_start + cols]
                    .iter()
                    .any(|dirty| *dirty)
            })
            .collect();
        let dirty_row_count = dirty_rows.iter().filter(|dirty| **dirty).count();
        if dirty_row_count * 2 < rows {
            return None;
        }

        let row_bytes = cols * CELL_SIZE;
        let current = self.inner.cells();
        let previous = &self.painted_cells;
        let max_delta = rows.saturating_sub(1).min(6);
        let mut best: Option<(usize, i32, Vec<bool>)> = None;

        for delta in 1..=max_delta {
            let overlap = rows - delta;
            if overlap < 3 {
                continue;
            }
            for signed_delta in [-(delta as i32), delta as i32] {
                let mut matched_rows = vec![false; rows];
                let mut matched = 0usize;
                let mut matched_dirty = 0usize;
                for row in 0..rows {
                    let src_row = row as i32 - signed_delta;
                    if src_row < 0 || src_row >= rows as i32 {
                        continue;
                    }
                    let cur_start = row * row_bytes;
                    let prev_start = src_row as usize * row_bytes;
                    if current[cur_start..cur_start + row_bytes]
                        == previous[prev_start..prev_start + row_bytes]
                    {
                        matched_rows[row] = true;
                        matched += 1;
                        if dirty_rows[row] {
                            matched_dirty += 1;
                        }
                    }
                }
                if matched * 5 < overlap * 4 || matched_dirty * 5 < dirty_row_count * 4 {
                    continue;
                }
                let replace = match &best {
                    None => true,
                    Some((best_matched, best_delta, _)) => {
                        matched > *best_matched
                            || (matched == *best_matched
                                && signed_delta.unsigned_abs() < best_delta.unsigned_abs())
                    }
                };
                if replace {
                    best = Some((matched, signed_delta, matched_rows));
                }
            }
        }

        best.map(|(_, delta_rows, matched_rows)| ScrollCopy {
            delta_rows,
            matched_rows,
        })
    }

}
