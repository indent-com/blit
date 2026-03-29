use rustc_hash::FxHashMap;

use blit_remote::{TerminalState, CELL_SIZE};
use wasm_bindgen::{prelude::*, JsCast};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

/// Expose WASM linear memory for zero-copy typed array views.
#[wasm_bindgen]
pub fn wasm_memory() -> JsValue {
    wasm_bindgen::memory()
}

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

const DEFAULT_FONT_FAMILY: &str = r#"ui-monospace, monospace"#;

/// Quote font families in a CSS font-family list so that names with spaces
/// or non-generic names work correctly in canvas `ctx.font` assignments.
fn css_quote_font_family(family: &str) -> String {
    const GENERIC: &[&str] = &[
        "serif",
        "sans-serif",
        "monospace",
        "cursive",
        "fantasy",
        "system-ui",
        "ui-serif",
        "ui-sans-serif",
        "ui-monospace",
        "ui-rounded",
        "math",
        "emoji",
        "fangsong",
    ];
    family
        .split(',')
        .map(|f| {
            let f = f.trim();
            if GENERIC.iter().any(|g| g.eq_ignore_ascii_case(f))
                || f.starts_with('"')
                || f.starts_with('\'')
            {
                f.to_owned()
            } else {
                format!("'{f}'")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}
const BG_OP_STRIDE: usize = 4;
const MODE_ECHO: u16 = 1 << 9;
const MODE_ICANON: u16 = 1 << 10;

const DEFAULT_ANSI_COLORS: [[u8; 3]; 16] = [
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

#[derive(Clone)]
struct Palette {
    ansi_16: [[u8; 3]; 16],
    default_fg: [u8; 3],
    default_bg: [u8; 3],
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            ansi_16: DEFAULT_ANSI_COLORS,
            default_fg: [204, 204, 204],
            default_bg: [0, 0, 0],
        }
    }
}

impl Palette {
    fn idx_to_rgb(&self, idx: u8) -> (u8, u8, u8) {
        if idx < 16 {
            let c = self.ansi_16[idx as usize];
            return (c[0], c[1], c[2]);
        }
        idx_to_rgb_high(idx)
    }

    fn resolve(&self, color_type: u8, r: u8, g: u8, b: u8, is_fg: bool, dim: bool) -> CellColor {
        match color_type {
            0 => {
                let [cr, cg, cb] = if is_fg {
                    self.default_fg
                } else {
                    self.default_bg
                };
                let (r, g, b) = if dim && is_fg {
                    (
                        (cr as u16 * 6 / 10) as u8,
                        (cg as u16 * 6 / 10) as u8,
                        (cb as u16 * 6 / 10) as u8,
                    )
                } else {
                    (cr, cg, cb)
                };
                CellColor {
                    r,
                    g,
                    b,
                    is_default: true,
                }
            }
            1 => {
                let (cr, cg, cb) = self.idx_to_rgb(r);
                let (r, g, b) = if dim {
                    (cr / 2, cg / 2, cb / 2)
                } else {
                    (cr, cg, cb)
                };
                CellColor {
                    r,
                    g,
                    b,
                    is_default: false,
                }
            }
            2 => {
                let (r, g, b) = if dim {
                    (r / 2, g / 2, b / 2)
                } else {
                    (r, g, b)
                };
                CellColor {
                    r,
                    g,
                    b,
                    is_default: false,
                }
            }
            _ => {
                let [cr, cg, cb] = if is_fg {
                    self.default_fg
                } else {
                    self.default_bg
                };
                CellColor {
                    r: cr,
                    g: cg,
                    b: cb,
                    is_default: true,
                }
            }
        }
    }

    fn color_css(&self, color_type: u8, r: u8, g: u8, b: u8, is_fg: bool, dim: bool) -> String {
        let (cr, cg, cb) = match color_type {
            0 => {
                let [dr, dg, db] = if is_fg {
                    self.default_fg
                } else {
                    self.default_bg
                };
                if dim && is_fg {
                    return format!(
                        "rgb({},{},{})",
                        (dr as u16 * 6 / 10) as u8,
                        (dg as u16 * 6 / 10) as u8,
                        (db as u16 * 6 / 10) as u8
                    );
                }
                return format!("#{:02x}{:02x}{:02x}", dr, dg, db);
            }
            1 => self.idx_to_rgb(r),
            2 => (r, g, b),
            _ => {
                let [dr, dg, db] = if is_fg {
                    self.default_fg
                } else {
                    self.default_bg
                };
                return format!("#{:02x}{:02x}{:02x}", dr, dg, db);
            }
        };
        let (cr, cg, cb) = if dim {
            (cr / 2, cg / 2, cb / 2)
        } else {
            (cr, cg, cb)
        };
        format!("#{:02x}{:02x}{:02x}", cr, cg, cb)
    }
}

fn idx_to_rgb_high(idx: u8) -> (u8, u8, u8) {
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
    /// Pack into u32 for fill-style change detection (no String needed).
    fn pack(&self) -> u32 {
        ((self.is_default as u32) << 24)
            | ((self.r as u32) << 16)
            | ((self.g as u32) << 8)
            | self.b as u32
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
    fn new(bytes: &[u8], bold: bool, italic: bool, underline: bool, wide: bool) -> Option<Self> {
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
    slots: FxHashMap<GlyphKey, GlyphSlot>,
    version: u32,
    next_x: u32,
    next_y: u32,
    row_height: u32,
    cell_width: u32,
    cell_height: u32,
    wide_cell_width: u32,
    /// Cached canvas size — avoids DOM property access per glyph.
    cached_size: u32,
}

impl GlyphAtlas {
    const MIN_SIZE: u32 = 2048;
    const MAX_SIZE: u32 = 8192;
    const PADDING: u32 = 1;
    const VERT_PAD: u32 = 2;

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
            if self.cached_size > 0 {
                ctx.clear_rect(0.0, 0.0, self.cached_size as f64, self.cached_size as f64);
            }
            ctx.set_text_baseline("bottom");
        }
    }

    fn atlas_size(&self) -> u32 {
        if self.cached_size > 0 {
            self.cached_size
        } else {
            Self::MIN_SIZE
        }
    }

    fn ensure_canvas_sized(&mut self, size: u32) -> bool {
        let size = size.clamp(Self::MIN_SIZE, Self::MAX_SIZE);
        if self.cached_size == size && self.canvas.is_some() && self.ctx.is_some() {
            return true;
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
        self.cached_size = size;
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
        let rows_needed = count.div_ceil(cols);
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
        if required > self.atlas_size() && !self.ensure_canvas_sized(required) {
            return false;
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
        let render_height = self.cell_height.max(1) + Self::VERT_PAD;
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
            let scaled_font = format!(
                "{}px {}",
                font_size,
                &font[font.find("px ").map(|i| i + 3).unwrap_or(0)..]
            );
            let pad_y = (cell_height - scaled_h) / 2.0;
            (
                scaled_font,
                slot.src_x,
                slot.src_y + pad_y + scaled_h + Self::VERT_PAD as f64,
            )
        } else {
            (
                font,
                slot.src_x,
                slot.src_y + cell_height + Self::VERT_PAD as f64,
            )
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
            ctx.move_to(
                slot.src_x,
                slot.src_y + cell_height + Self::VERT_PAD as f64 - 1.0,
            );
            ctx.line_to(
                slot.src_x + slot.width,
                slot.src_y + cell_height + Self::VERT_PAD as f64 - 1.0,
            );
            ctx.stroke();
        }
        ctx.restore();
        self.slots.insert(key, slot);
        self.version = self.version.wrapping_add(1);
        Some(slot)
    }
}

#[wasm_bindgen]
pub struct Terminal {
    cell_width: f64,
    cell_height: f64,
    font_size: f64,
    font_family: String,
    palette: Palette,
    inner: TerminalState,
    glyph_atlas: GlyphAtlas,
    bg_ops: Vec<u32>,
    overflow_text_ops: Vec<(u32, u32, u32, String)>,
    /// Ready-to-upload background vertex data (6 verts × 6 floats per rect).
    bg_verts: Vec<f32>,
    /// Ready-to-upload glyph vertex data (6 verts × 8 floats per glyph).
    glyph_verts: Vec<f32>,
    /// Pixel offsets added to all vertex coordinates (for centering content).
    render_x_offset: f32,
    render_y_offset: f32,
}

#[wasm_bindgen]
impl Terminal {
    #[wasm_bindgen(constructor)]
    pub fn new(rows: u16, cols: u16, cell_width: f64, cell_height: f64) -> Self {
        Terminal {
            cell_width,
            cell_height,
            font_size: cell_height,
            font_family: DEFAULT_FONT_FAMILY.to_owned(),
            palette: Palette::default(),
            inner: TerminalState::new(rows, cols),
            glyph_atlas: GlyphAtlas::default(),
            bg_ops: Vec::new(),
            bg_verts: Vec::new(),
            glyph_verts: Vec::new(),
            overflow_text_ops: Vec::new(),
            render_x_offset: 0.0,
            render_y_offset: 0.0,
        }
    }

    pub fn set_cell_size(&mut self, cell_width: f64, cell_height: f64) {
        self.cell_width = cell_width;
        self.cell_height = cell_height;
        self.glyph_atlas.invalidate();
    }

    pub fn set_render_offset(&mut self, x: f64, y: f64) {
        self.render_x_offset = x as f32;
        self.render_y_offset = y as f32;
    }

    pub fn set_font_size(&mut self, font_size: f64) {
        let next = font_size.max(1.0);
        if (self.font_size - next).abs() < f64::EPSILON {
            return;
        }
        self.font_size = next;
        self.glyph_atlas.invalidate();
    }

    pub fn invalidate_render_cache(&mut self) {
        self.glyph_atlas.invalidate();
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
        self.glyph_atlas.invalidate();
    }

    pub fn set_default_colors(
        &mut self,
        fg_r: u8,
        fg_g: u8,
        fg_b: u8,
        bg_r: u8,
        bg_g: u8,
        bg_b: u8,
    ) {
        self.palette.default_fg = [fg_r, fg_g, fg_b];
        self.palette.default_bg = [bg_r, bg_g, bg_b];
    }

    pub fn set_ansi_color(&mut self, idx: u8, r: u8, g: u8, b: u8) {
        if idx < 16 {
            self.palette.ansi_16[idx as usize] = [r, g, b];
        }
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
    pub fn scrollback_lines(&self) -> u32 {
        self.inner.frame().scrollback_lines()
    }
    pub fn cursor_visible(&self) -> bool {
        self.inner.mode() & 1 != 0
    }
    /// DECSCUSR cursor style: 0=default, 1=blinking block, 2=steady block,
    /// 3=blinking underline, 4=steady underline, 5=blinking bar, 6=steady bar.
    pub fn cursor_style(&self) -> u16 {
        (self.inner.mode() >> 12) & 7
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

    /// Pointer to ready-to-upload background vertex data (f32).
    pub fn bg_verts_ptr(&self) -> *const f32 {
        self.bg_verts.as_ptr()
    }

    /// Length of background vertex data (in f32 elements).
    pub fn bg_verts_len(&self) -> usize {
        self.bg_verts.len()
    }

    /// Pointer to ready-to-upload glyph vertex data (f32).
    pub fn glyph_verts_ptr(&self) -> *const f32 {
        self.glyph_verts.as_ptr()
    }

    /// Length of glyph vertex data (in f32 elements).
    pub fn glyph_verts_len(&self) -> usize {
        self.glyph_verts.len()
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
        let [dfr, dfg, dfb] = self.palette.default_fg;
        let [dbr, dbg, dbb] = self.palette.default_bg;
        let default_fg_css = format!("#{:02x}{:02x}{:02x}", dfr, dfg, dfb);
        let default_bg_css = format!("#{:02x}{:02x}{:02x}", dbr, dbg, dbb);
        let mut html = format!(
            "<pre style=\"font-family:ui-monospace,monospace;background:{};color:{};padding:4px\">",
            default_bg_css, default_fg_css,
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
                        self.palette
                            .color_css(bg_type, cell[5], cell[6], cell[7], false, dim),
                        self.palette
                            .color_css(fg_type, cell[2], cell[3], cell[4], true, false),
                    )
                } else {
                    (
                        self.palette
                            .color_css(fg_type, cell[2], cell[3], cell[4], true, dim),
                        self.palette
                            .color_css(bg_type, cell[5], cell[6], cell[7], false, false),
                    )
                };
                let flat = row as usize * self.inner.cols() as usize + col as usize;
                let ch = if content_len == 7 {
                    self.inner
                        .frame()
                        .overflow()
                        .get(&flat)
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
                let has_style =
                    fg != default_fg_css || bg != default_bg_css || bold || italic || underline;
                if has_style {
                    let mut style = String::new();
                    if fg != default_fg_css {
                        style.push_str("color:");
                        style.push_str(&fg);
                        style.push(';');
                    }
                    if bg != default_bg_css {
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

    /// Returns true if the given row wraps to the next (is part of a longer logical line).
    pub fn is_wrapped(&self, row: u16) -> bool {
        self.inner.is_wrapped(row)
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

    fn push_glyph_vert(
        &mut self,
        slot: GlyphSlot,
        row: usize,
        col: usize,
        col_span: usize,
        fg_packed: u32,
    ) {
        let pw = self.cell_width as f32;
        let ph = self.cell_height as f32;
        let aw = self.glyph_atlas.atlas_size().max(1) as f32;
        let sx = slot.src_x as f32;
        let sy = slot.src_y as f32;
        let sw = slot.width as f32;
        let sh = slot.height as f32;
        let xo = self.render_x_offset;
        let yo = self.render_y_offset;
        let dx1 = col as f32 * pw + xo;
        let dy1 = row as f32 * ph - (sh - ph) + yo;
        let dx2 = dx1 + col_span as f32 * pw;
        let dy2 = dy1 + sh;
        let u1 = sx / aw;
        let v1 = sy / aw;
        let u2 = (sx + sw) / aw;
        let v2 = (sy + sh) / aw;
        let r = ((fg_packed >> 16) & 0xff) as f32 / 255.0;
        let g = ((fg_packed >> 8) & 0xff) as f32 / 255.0;
        let b = (fg_packed & 0xff) as f32 / 255.0;
        self.glyph_verts.extend_from_slice(&[
            dx1, dy1, u1, v1, r, g, b, 1.0, dx2, dy1, u2, v1, r, g, b, 1.0, dx1, dy2, u1, v2, r, g,
            b, 1.0, dx1, dy2, u1, v2, r, g, b, 1.0, dx2, dy1, u2, v1, r, g, b, 1.0, dx2, dy2, u2,
            v2, r, g, b, 1.0,
        ]);
    }

    fn prepare_render_ops_inner(&mut self) {
        let cw = self.cell_width;
        let ch = self.cell_height;
        let rows = self.inner.rows() as usize;
        let cols = self.inner.cols() as usize;
        let total = rows * cols;
        let normal_font = format!(
            "{}px {}",
            self.font_size.round().max(1.0) as u32,
            css_quote_font_family(&self.font_family)
        );

        self.bg_ops.clear();
        self.glyph_verts.clear();
        self.overflow_text_ops.clear();

        self.glyph_atlas.ensure_metrics(cw, ch);
        // Pre-grow atlas based on previous capacity — avoids mid-frame
        // invalidation for steady-state rendering (same glyph set).
        self.glyph_atlas
            .ensure_capacity(self.glyph_atlas.slots.len());

        for i in 0..total {
            let idx = i * CELL_SIZE;
            let mut cell = [0u8; CELL_SIZE];
            cell.copy_from_slice(&self.inner.cells()[idx..idx + CELL_SIZE]);
            let f0 = cell[0];
            let f1 = cell[1];

            if f1 & 4 != 0 {
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
            let row = i / cols;
            let col = i % cols;

            let (fg, bg) = if inverse {
                (
                    self.palette
                        .resolve(bg_type, cell[5], cell[6], cell[7], false, dim),
                    {
                        let mut c = self
                            .palette
                            .resolve(fg_type, cell[2], cell[3], cell[4], true, false);
                        c.is_default = false;
                        c
                    },
                )
            } else {
                (
                    self.palette
                        .resolve(fg_type, cell[2], cell[3], cell[4], true, dim),
                    self.palette
                        .resolve(bg_type, cell[5], cell[6], cell[7], false, false),
                )
            };
            let cell_cols = if wide { 2 } else { 1 };

            if !bg.is_default {
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
                    if let Some(key) = GlyphKey::new(content_bytes, bold, italic, underline, wide) {
                        if let Some(slot) = self.glyph_atlas.ensure_glyph(key, &normal_font, cw, ch)
                        {
                            self.push_glyph_vert(slot, row, col, cell_cols, fg.pack());
                        }
                    }
                }
            }
        }

        // Build ready-to-upload background vertex buffer from coalesced ops.
        self.build_bg_verts();
    }

    fn build_bg_verts(&mut self) {
        let pw = self.cell_width as f32;
        let ph = self.cell_height as f32;
        let xo = self.render_x_offset;
        let yo = self.render_y_offset;
        self.bg_verts.clear();
        self.bg_verts.reserve(self.bg_ops.len() / BG_OP_STRIDE * 36);
        let ops = &self.bg_ops;
        let mut i = 0;
        while i < ops.len() {
            let x1 = ops[i + 1] as f32 * pw + xo;
            let y1 = ops[i] as f32 * ph + yo;
            let x2 = x1 + ops[i + 2] as f32 * pw;
            let y2 = y1 + ph;
            let packed = ops[i + 3];
            let r = ((packed >> 16) & 0xff) as f32 / 255.0;
            let g = ((packed >> 8) & 0xff) as f32 / 255.0;
            let b = (packed & 0xff) as f32 / 255.0;
            self.bg_verts.extend_from_slice(&[
                x1, y1, r, g, b, 1.0, x2, y1, r, g, b, 1.0, x1, y2, r, g, b, 1.0, x1, y2, r, g, b,
                1.0, x2, y1, r, g, b, 1.0, x2, y2, r, g, b, 1.0,
            ]);
            i += BG_OP_STRIDE;
        }
    }
}
