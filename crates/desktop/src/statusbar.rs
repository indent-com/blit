use crate::atlas::{GlyphAtlas, GlyphKey};
use crate::palette::Palette;
use crate::terminal::{push_glyph_quad, push_rect_quad_pub};

pub fn render_status_bar(
    bg_verts: &mut Vec<f32>,
    glyph_verts: &mut Vec<f32>,
    atlas: &mut GlyphAtlas,
    palette: &Palette,
    window_width: f32,
    window_height: f32,
    session_count: usize,
    exited_count: usize,
    focused_name: Option<&str>,
    cols: u16,
    rows: u16,
    connected: bool,
    connecting: bool,
) {
    let cell_w = atlas.cell_width;
    let cell_h = atlas.cell_height;
    let bar_y = window_height - cell_h;
    let theme = palette.theme();

    let panel_r = theme.panel_bg[0] as f32 / 255.0;
    let panel_g = theme.panel_bg[1] as f32 / 255.0;
    let panel_b = theme.panel_bg[2] as f32 / 255.0;
    push_rect_quad_pub(bg_verts, 0.0, bar_y, window_width, window_height, panel_r, panel_g, panel_b, 1.0);

    let border_r = theme.dim_fg[0] as f32 / 255.0;
    let border_g = theme.dim_fg[1] as f32 / 255.0;
    let border_b = theme.dim_fg[2] as f32 / 255.0;
    push_rect_quad_pub(bg_verts, 0.0, bar_y, window_width, bar_y + 1.0, border_r, border_g, border_b, 0.3);

    let dim_fg = [
        theme.dim_fg[0] as f32 / 255.0,
        theme.dim_fg[1] as f32 / 255.0,
        theme.dim_fg[2] as f32 / 255.0,
    ];
    let fg = [
        theme.fg[0] as f32 / 255.0,
        theme.fg[1] as f32 / 255.0,
        theme.fg[2] as f32 / 255.0,
    ];

    let mut x = cell_w * 0.5;

    let count_text = if exited_count > 0 {
        format!("{session_count} terminal{} ({exited_count} exited)", if session_count != 1 { "s" } else { "" })
    } else {
        format!("{session_count} terminal{}", if session_count != 1 { "s" } else { "" })
    };
    x = render_text(glyph_verts, atlas, &count_text, x, bar_y, cell_w, cell_h, dim_fg);

    x += cell_w;

    if let Some(name) = focused_name {
        let max_chars = ((window_width * 0.4) / cell_w) as usize;
        let display = if name.chars().count() > max_chars && max_chars > 1 {
            let truncated: String = name.chars().take(max_chars - 1).collect();
            format!("{truncated}…")
        } else {
            name.to_string()
        };
        x = render_text(glyph_verts, atlas, &display, x, bar_y, cell_w, cell_h, fg);
    }
    let _ = x;

    let right_text = format!("{cols}x{rows}");
    let dot_width = cell_w;
    let right_text_width = right_text.len() as f32 * cell_w;
    let right_x = window_width - right_text_width - dot_width - cell_w * 1.5;
    render_text(glyph_verts, atlas, &right_text, right_x, bar_y, cell_w, cell_h, dim_fg);

    let dot_x = window_width - dot_width - cell_w * 0.5;
    let dot_y = bar_y + cell_h * 0.35;
    let dot_size = cell_h * 0.3;
    let dot_color = if connected {
        [theme.success[0] as f32 / 255.0, theme.success[1] as f32 / 255.0, theme.success[2] as f32 / 255.0]
    } else if connecting {
        [theme.warning[0] as f32 / 255.0, theme.warning[1] as f32 / 255.0, theme.warning[2] as f32 / 255.0]
    } else {
        [theme.error[0] as f32 / 255.0, theme.error[1] as f32 / 255.0, theme.error[2] as f32 / 255.0]
    };
    push_rect_quad_pub(bg_verts, dot_x, dot_y, dot_x + dot_size, dot_y + dot_size, dot_color[0], dot_color[1], dot_color[2], 1.0);
}

pub fn render_text(
    glyph_verts: &mut Vec<f32>,
    atlas: &mut GlyphAtlas,
    text: &str,
    start_x: f32,
    y: f32,
    cell_w: f32,
    _cell_h: f32,
    color: [f32; 3],
) -> f32 {
    let mut x = start_x;
    for ch in text.chars() {
        let mut bytes = [0u8; 4];
        let len = ch.encode_utf8(&mut bytes).len();
        let key = GlyphKey {
            bytes,
            len: len as u8,
            bold: false,
            italic: false,
            underline: false,
            wide: false,
        };
        let slot = atlas.ensure_glyph(key);
        let x1 = x;
        let y1 = y;
        let x2 = x1 + slot.width as f32;
        let y2 = y1 + slot.height as f32;
        let atlas_size = atlas.atlas_size as f32;
        let u1 = slot.x as f32 / atlas_size;
        let v1 = slot.y as f32 / atlas_size;
        let u2 = (slot.x + slot.width) as f32 / atlas_size;
        let v2 = (slot.y + slot.height) as f32 / atlas_size;
        push_glyph_quad(glyph_verts, x1, y1, x2, y2, u1, v1, u2, v2, color[0], color[1], color[2]);
        x += cell_w;
    }
    x
}
