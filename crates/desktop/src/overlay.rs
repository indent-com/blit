use crate::atlas::GlyphAtlas;
use crate::connection::SessionKey;
use crate::palette::Palette;
use crate::statusbar::render_text;
use crate::terminal;

pub enum OverlayKind {
    Help,
    Palette(PaletteOverlay),
    Font(FontOverlay),
    Switcher(SwitcherOverlay),
    Disconnected(DisconnectedOverlay),
}

pub struct PaletteOverlay {
    pub search: String,
    pub selected: usize,
    pub dark_filter: Option<bool>,
}

pub struct FontOverlay {
    pub search: String,
    pub selected: usize,
    pub font_size: f32,
    pub families: Vec<String>,
}

pub struct SwitcherOverlay {
    pub input: String,
    pub selected: usize,
    pub items: Vec<SwitcherItem>,
}

pub struct DisconnectedOverlay {
    pub remotes: Vec<(String, String, u32)>,
}

pub enum SwitcherItem {
    Session { key: SessionKey, title: String, remote: String },
    Action { label: String, action: SwitcherAction },
}

pub enum SwitcherAction {
    NewTerminal,
    ChangePalette,
    ChangeFont,
    ChangeLayout,
    ClearLayout,
    ConnectRemote(String),
    DisconnectRemote(String),
}

impl PaletteOverlay {
    pub fn new() -> Self {
        Self {
            search: String::new(),
            selected: 0,
            dark_filter: None,
        }
    }

    pub fn filtered_palettes(&self) -> Vec<&'static Palette> {
        crate::palette::PALETTES
            .iter()
            .filter(|p| {
                if let Some(dark) = self.dark_filter {
                    if p.dark != dark {
                        return false;
                    }
                }
                if !self.search.is_empty() {
                    return p.name.to_lowercase().contains(&self.search.to_lowercase());
                }
                true
            })
            .collect()
    }
}

impl FontOverlay {
    pub fn new(current_size: f32) -> Self {
        let families = blit_fonts::list_monospace_font_families();
        Self {
            search: String::new(),
            selected: 0,
            font_size: current_size,
            families,
        }
    }

    pub fn filtered_families(&self) -> Vec<&str> {
        self.families
            .iter()
            .filter(|f| {
                if self.search.is_empty() {
                    return true;
                }
                f.to_lowercase().contains(&self.search.to_lowercase())
            })
            .map(|s| s.as_str())
            .collect()
    }
}

impl SwitcherOverlay {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            selected: 0,
            items: Vec::new(),
        }
    }

    pub fn rebuild_items(
        &mut self,
        sessions: &[(SessionKey, String, String)],
        connected_remotes: &[String],
        disconnected_remotes: &[String],
    ) {
        self.items.clear();
        let is_command = self.input.starts_with('>');
        if is_command {
            self.items.push(SwitcherItem::Action {
                label: "New terminal".into(),
                action: SwitcherAction::NewTerminal,
            });
            self.items.push(SwitcherItem::Action {
                label: "Change palette".into(),
                action: SwitcherAction::ChangePalette,
            });
            self.items.push(SwitcherItem::Action {
                label: "Change font".into(),
                action: SwitcherAction::ChangeFont,
            });
            self.items.push(SwitcherItem::Action {
                label: "Change layout".into(),
                action: SwitcherAction::ChangeLayout,
            });
            self.items.push(SwitcherItem::Action {
                label: "Clear layout".into(),
                action: SwitcherAction::ClearLayout,
            });
            for r in disconnected_remotes {
                self.items.push(SwitcherItem::Action {
                    label: format!("Connect: {r}"),
                    action: SwitcherAction::ConnectRemote(r.clone()),
                });
            }
            for r in connected_remotes {
                self.items.push(SwitcherItem::Action {
                    label: format!("Disconnect: {r}"),
                    action: SwitcherAction::DisconnectRemote(r.clone()),
                });
            }
            return;
        }

        let query = self.input.to_lowercase();
        for (key, title, remote) in sessions {
            let display = format!("{remote}: {title}");
            if !query.is_empty() && !display.to_lowercase().contains(&query) {
                continue;
            }
            self.items.push(SwitcherItem::Session {
                key: key.clone(),
                title: display,
                remote: remote.clone(),
            });
        }
    }
}

impl DisconnectedOverlay {
    pub fn new() -> Self {
        Self { remotes: Vec::new() }
    }
}

pub fn render_overlay_glyphs(
    overlay: &OverlayKind,
    glyph_verts: &mut Vec<f32>,
    bg_verts: &mut Vec<f32>,
    atlas: &mut GlyphAtlas,
    width: f32,
    height: f32,
    palette: &Palette,
) {
    let theme = palette.theme();
    let fg = [
        theme.fg[0] as f32 / 255.0,
        theme.fg[1] as f32 / 255.0,
        theme.fg[2] as f32 / 255.0,
    ];
    let dim = [
        theme.dim_fg[0] as f32 / 255.0,
        theme.dim_fg[1] as f32 / 255.0,
        theme.dim_fg[2] as f32 / 255.0,
    ];
    let accent = [
        theme.accent[0] as f32 / 255.0,
        theme.accent[1] as f32 / 255.0,
        theme.accent[2] as f32 / 255.0,
    ];

    let cell_w = atlas.cell_width;
    let cell_h = atlas.cell_height;

    let panel_w = (width * 0.6).min(600.0);
    let panel_h = match overlay {
        OverlayKind::Help => (height * 0.7).min(500.0),
        _ => (height * 0.6).min(400.0),
    };
    let px = (width - panel_w) / 2.0;
    let py = (height - panel_h) / 2.0;

    match overlay {
        OverlayKind::Switcher(sw) => {
            let input_y = py + cell_h * 0.5;
            let prompt = if sw.input.is_empty() { "Search sessions..." } else { "" };
            let display = if sw.input.is_empty() {
                prompt.to_string()
            } else {
                sw.input.clone()
            };
            let color = if sw.input.is_empty() { dim } else { fg };
            render_text(glyph_verts, atlas, &display, px + cell_w, input_y, cell_w, cell_h, color);

            terminal::push_rect_quad_pub(
                bg_verts,
                px + cell_w * 0.5,
                input_y + cell_h + 2.0,
                px + panel_w - cell_w * 0.5,
                input_y + cell_h + 3.0,
                dim[0], dim[1], dim[2], 0.3,
            );

            let list_y = input_y + cell_h * 2.0;
            let max_visible = ((panel_h - cell_h * 3.0) / cell_h) as usize;
            for (i, item) in sw.items.iter().enumerate().take(max_visible) {
                let y = list_y + i as f32 * cell_h;
                let is_selected = i == sw.selected;
                if is_selected {
                    terminal::push_rect_quad_pub(
                        bg_verts,
                        px + cell_w * 0.5,
                        y,
                        px + panel_w - cell_w * 0.5,
                        y + cell_h,
                        accent[0], accent[1], accent[2], 0.2,
                    );
                }
                let label = match item {
                    SwitcherItem::Session { title, .. } => title.as_str(),
                    SwitcherItem::Action { label, .. } => label.as_str(),
                };
                let color = if is_selected { fg } else { dim };
                let prefix = match item {
                    SwitcherItem::Session { .. } => "",
                    SwitcherItem::Action { .. } => "> ",
                };
                let text = format!("{prefix}{label}");
                render_text(glyph_verts, atlas, &text, px + cell_w * 1.5, y, cell_w, cell_h, color);
            }
        }
        OverlayKind::Help => {
            let lines = [
                "Keyboard Shortcuts",
                "",
                "Cmd/Ctrl+K          Switcher",
                "Cmd/Ctrl+Shift+Enter  New terminal",
                "Cmd/Ctrl+Shift+W    Close session",
                "Cmd/Ctrl+Shift+}    Next session",
                "Cmd/Ctrl+Shift+{    Prev session",
                "Shift+PageUp/Down   Scroll",
                "Escape              Close overlay",
                "",
                "In switcher, type > for commands",
            ];
            for (i, line) in lines.iter().enumerate() {
                let y = py + cell_h * (i as f32 + 1.0);
                let color = if i == 0 { fg } else { dim };
                render_text(glyph_verts, atlas, line, px + cell_w, y, cell_w, cell_h, color);
            }
        }
        _ => {}
    }
}

pub fn render_overlay_bg(
    overlay: &OverlayKind,
    width: f32,
    height: f32,
    palette: &Palette,
) -> Vec<f32> {
    let mut verts = Vec::new();
    let theme = palette.theme();

    terminal::push_rect_quad_pub(&mut verts, 0.0, 0.0, width, height, 0.0, 0.0, 0.0, 0.6);

    let panel_w = (width * 0.6).min(600.0);
    let panel_h = match overlay {
        OverlayKind::Help => (height * 0.7).min(500.0),
        _ => (height * 0.6).min(400.0),
    };
    let px = (width - panel_w) / 2.0;
    let py = (height - panel_h) / 2.0;
    let r = theme.panel_bg[0] as f32 / 255.0;
    let g = theme.panel_bg[1] as f32 / 255.0;
    let b = theme.panel_bg[2] as f32 / 255.0;
    terminal::push_rect_quad_pub(&mut verts, px, py, px + panel_w, py + panel_h, r, g, b, 0.95);

    verts
}


