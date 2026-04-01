use crate::atlas::{GlyphAtlas, GlyphKey};
use crate::palette::{resolve_color, Palette};

pub struct Terminal {
    pub state: blit_remote::TerminalState,
    pub scroll_offset: u32,
    pub selection: Option<Selection>,
    pub dirty: bool,
    pub bg_verts: Vec<f32>,
    pub glyph_verts: Vec<f32>,
}

pub struct Selection {
    pub start_row: u16,
    pub start_col: u16,
    pub end_row: u16,
    pub end_col: u16,
    pub granularity: u8,
}

impl Terminal {
    pub fn new() -> Self {
        Self {
            state: blit_remote::TerminalState::new(24, 80),
            scroll_offset: 0,
            selection: None,
            dirty: true,
            bg_verts: Vec::new(),
            glyph_verts: Vec::new(),
        }
    }

    pub fn feed_frame(&mut self, payload: &[u8]) {
        self.state.feed_compressed(payload);
        self.dirty = true;
    }

    pub fn rows(&self) -> u16 {
        self.state.rows()
    }

    pub fn cols(&self) -> u16 {
        self.state.cols()
    }

    pub fn cursor_row(&self) -> u16 {
        self.state.cursor_row()
    }

    pub fn cursor_col(&self) -> u16 {
        self.state.cursor_col()
    }

    pub fn cursor_visible(&self) -> bool {
        self.state.mode() & 1 != 0
    }

    pub fn cursor_style(&self) -> u8 {
        ((self.state.mode() >> 12) & 0x7) as u8
    }

    pub fn app_cursor(&self) -> bool {
        self.state.mode() & 2 != 0
    }

    pub fn mouse_mode(&self) -> u8 {
        ((self.state.mode() >> 4) & 7) as u8
    }

    pub fn scrollback_lines(&self) -> u32 {
        self.state.frame().scrollback_lines()
    }

    pub fn get_text(&self, sr: u16, sc: u16, er: u16, ec: u16) -> String {
        self.state.get_text(sr, sc, er, ec)
    }

    pub fn prepare_vertices(
        &mut self,
        palette: &Palette,
        atlas: &mut GlyphAtlas,
        cell_w: f32,
        cell_h: f32,
        offset_x: f32,
        offset_y: f32,
    ) {
        self.bg_verts.clear();
        self.glyph_verts.clear();
        self.dirty = false;

        let rows = self.state.rows() as usize;
        let cols = self.state.cols() as usize;
        let cells = self.state.cells();
        let cell_size = blit_remote::CELL_SIZE;

        let mut bg_ops: Vec<(f32, f32, f32, u32, [u8; 3])> = Vec::new();

        for row in 0..rows {
            for col in 0..cols {
                let idx = (row * cols + col) * cell_size;
                if idx + cell_size > cells.len() {
                    continue;
                }
                let f0 = cells[idx];
                let f1 = cells[idx + 1];

                let wide_cont = f1 & 4 != 0;
                if wide_cont {
                    continue;
                }

                let fg_type = f0 & 3;
                let bg_type = (f0 >> 2) & 3;
                let bold = f0 & 0x10 != 0;
                let dim = f0 & 0x20 != 0;
                let italic = f0 & 0x40 != 0;
                let underline = f0 & 0x80 != 0;
                let inverse = f1 & 1 != 0;
                let wide = f1 & 2 != 0;
                let content_len = (f1 >> 3) & 7;

                let fg_r = cells[idx + 2];
                let fg_g = cells[idx + 3];
                let fg_b = cells[idx + 4];
                let bg_r = cells[idx + 5];
                let bg_g = cells[idx + 6];
                let bg_b = cells[idx + 7];

                let mut fg = resolve_color(palette, fg_type, fg_r, fg_g, fg_b, true, dim);
                let mut bg = resolve_color(palette, bg_type, bg_r, bg_g, bg_b, false, false);
                let mut _fg_is_default = fg_type == 0;
                let mut bg_is_default = bg_type == 0;

                if inverse {
                    std::mem::swap(&mut fg, &mut bg);
                    std::mem::swap(&mut _fg_is_default, &mut bg_is_default);
                    bg_is_default = false;
                }

                let col_span = if wide { 2u32 } else { 1u32 };

                if !bg_is_default {
                    let packed = ((bg[0] as u32) << 16) | ((bg[1] as u32) << 8) | (bg[2] as u32);
                    let x = col as f32 * cell_w + offset_x;
                    let y = row as f32 * cell_h + offset_y;
                    if let Some(last) = bg_ops.last_mut() {
                        if (last.1 - y).abs() < 0.1 && (last.0 + last.2 - x).abs() < 0.1 && last.3 == packed {
                            last.2 += cell_w * col_span as f32;
                        } else {
                            bg_ops.push((x, y, cell_w * col_span as f32, packed, bg));
                        }
                    } else {
                        bg_ops.push((x, y, cell_w * col_span as f32, packed, bg));
                    }
                }

                if content_len > 0 && content_len <= 4 {
                    let text_bytes = &cells[idx + 8..idx + 8 + content_len as usize];
                    if text_bytes == b" " {
                        continue;
                    }
                    let glyph_key = GlyphKey {
                        bytes: {
                            let mut b = [0u8; 4];
                            b[..text_bytes.len()].copy_from_slice(text_bytes);
                            b
                        },
                        len: content_len,
                        bold,
                        italic,
                        underline,
                        wide,
                    };
                    let slot = atlas.ensure_glyph(glyph_key);
                    let x1 = col as f32 * cell_w + offset_x;
                    let y1 = row as f32 * cell_h + offset_y;
                    let x2 = x1 + slot.width as f32;
                    let y2 = y1 + slot.height as f32;
                    let u1 = slot.x as f32 / atlas.atlas_size as f32;
                    let v1 = slot.y as f32 / atlas.atlas_size as f32;
                    let u2 = (slot.x + slot.width) as f32 / atlas.atlas_size as f32;
                    let v2 = (slot.y + slot.height) as f32 / atlas.atlas_size as f32;
                    let fr = fg[0] as f32 / 255.0;
                    let fg_g_f = fg[1] as f32 / 255.0;
                    let fb = fg[2] as f32 / 255.0;

                    push_glyph_quad(&mut self.glyph_verts, x1, y1, x2, y2, u1, v1, u2, v2, fr, fg_g_f, fb);
                }
            }
        }

        for (x, y, w, _, color) in &bg_ops {
            let r = color[0] as f32 / 255.0;
            let g = color[1] as f32 / 255.0;
            let b = color[2] as f32 / 255.0;
            push_rect_quad(&mut self.bg_verts, *x, *y, *x + *w, *y + cell_h, r, g, b, 1.0);
        }
    }
}

pub fn push_rect_quad_pub(verts: &mut Vec<f32>, x1: f32, y1: f32, x2: f32, y2: f32, r: f32, g: f32, b: f32, a: f32) {
    push_rect_quad(verts, x1, y1, x2, y2, r, g, b, a);
}

fn push_rect_quad(verts: &mut Vec<f32>, x1: f32, y1: f32, x2: f32, y2: f32, r: f32, g: f32, b: f32, a: f32) {
    let v = [
        x1, y1, r, g, b, a,
        x2, y1, r, g, b, a,
        x1, y2, r, g, b, a,
        x1, y2, r, g, b, a,
        x2, y1, r, g, b, a,
        x2, y2, r, g, b, a,
    ];
    verts.extend_from_slice(&v);
}

pub fn push_glyph_quad(
    verts: &mut Vec<f32>,
    x1: f32, y1: f32, x2: f32, y2: f32,
    u1: f32, v1: f32, u2: f32, v2: f32,
    r: f32, g: f32, b: f32,
) {
    let v = [
        x1, y1, u1, v1, r, g, b, 1.0,
        x2, y1, u2, v1, r, g, b, 1.0,
        x1, y2, u1, v2, r, g, b, 1.0,
        x1, y2, u1, v2, r, g, b, 1.0,
        x2, y1, u2, v1, r, g, b, 1.0,
        x2, y2, u2, v2, r, g, b, 1.0,
    ];
    verts.extend_from_slice(&v);
}

pub fn cursor_verts(
    row: u16,
    col: u16,
    style: u8,
    cell_w: f32,
    cell_h: f32,
    offset_x: f32,
    offset_y: f32,
    focused: bool,
    blink_visible: bool,
) -> Vec<f32> {
    let x = col as f32 * cell_w + offset_x;
    let y = row as f32 * cell_h + offset_y;
    let mut verts = Vec::new();

    if !focused {
        let t = 1.0f32;
        let (r, g, b, a) = (0.6, 0.6, 0.6, 0.6);
        push_rect_quad(&mut verts, x, y, x + cell_w, y + t, r, g, b, a);
        push_rect_quad(&mut verts, x, y + cell_h - t, x + cell_w, y + cell_h, r, g, b, a);
        push_rect_quad(&mut verts, x, y, x + t, y + cell_h, r, g, b, a);
        push_rect_quad(&mut verts, x + cell_w - t, y, x + cell_w, y + cell_h, r, g, b, a);
        return verts;
    }

    let should_blink = matches!(style, 0 | 1 | 3 | 5);
    if should_blink && !blink_visible {
        return verts;
    }

    match style {
        0 | 1 | 2 => {
            push_rect_quad(&mut verts, x, y, x + cell_w, y + cell_h, 0.8, 0.8, 0.8, 0.5);
        }
        3 | 4 => {
            let h = (cell_h * 0.12).max(1.0);
            push_rect_quad(&mut verts, x, y + cell_h - h, x + cell_w, y + cell_h, 0.8, 0.8, 0.8, 0.7);
        }
        5 | 6 => {
            let w = (cell_w * 0.12).max(1.0);
            push_rect_quad(&mut verts, x, y, x + w, y + cell_h, 0.8, 0.8, 0.8, 0.7);
        }
        _ => {
            push_rect_quad(&mut verts, x, y, x + cell_w, y + cell_h, 0.8, 0.8, 0.8, 0.5);
        }
    }
    verts
}
