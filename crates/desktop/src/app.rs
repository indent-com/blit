use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::connection::{Command, ConnectionManager, ConnectionStatus, ServerEvent, SessionKey};
use crate::input::{self, AppAction};
use crate::overlay::{self, OverlayKind, SwitcherOverlay};
use crate::palette::{find_palette, Palette};
use crate::remotes::{RemoteConfig, UserConfig};
use crate::renderer::Renderer;
use crate::terminal::{self, Terminal};

pub struct App {
    window: Option<Arc<Window>>,
    surface: Option<wgpu::Surface<'static>>,
    device: Option<wgpu::Device>,
    queue: Option<wgpu::Queue>,
    config: Option<wgpu::SurfaceConfiguration>,
    renderer: Option<Renderer>,
    atlas: Option<crate::atlas::GlyphAtlas>,
    sessions: HashMap<SessionKey, Terminal>,
    titles: HashMap<SessionKey, String>,
    exited: HashSet<SessionKey>,
    connection_mgr: ConnectionManager,
    overlay: Option<OverlayKind>,
    palette: &'static Palette,
    font_family: String,
    font_size: f32,
    focused: Option<SessionKey>,
    lru: Vec<SessionKey>,
    modifiers: ModifiersState,
    window_size: PhysicalSize<u32>,
    remotes: Vec<RemoteConfig>,
    hub: String,
    blink_visible: bool,
    last_blink: Instant,
    needs_redraw: bool,
    cursor_position: (f64, f64),
    mouse_buttons: u8,
}

impl App {
    pub fn new(remotes: Vec<RemoteConfig>, hub: String, proxy: EventLoopProxy<()>, user_config: UserConfig) -> Self {
        Self {
            window: None,
            surface: None,
            device: None,
            queue: None,
            config: None,
            renderer: None,
            atlas: None,
            sessions: HashMap::new(),
            titles: HashMap::new(),
            exited: HashSet::new(),
            connection_mgr: ConnectionManager::new(proxy),
            overlay: None,
            palette: find_palette(&user_config.palette),
            font_family: user_config.font_family,
            font_size: user_config.font_size,
            focused: None,
            lru: Vec::new(),
            modifiers: ModifiersState::empty(),
            window_size: PhysicalSize::new(800, 600),
            remotes,
            hub,
            blink_visible: true,
            last_blink: Instant::now(),
            needs_redraw: true,
            cursor_position: (0.0, 0.0),
            mouse_buttons: 0,
        }
    }

    fn init_gpu(&mut self, window: Arc<Window>) {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });
        let surface = instance.create_surface(window.clone()).unwrap();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("no suitable GPU adapter");

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("blit-desktop"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: Default::default(),
                trace: wgpu::Trace::Off,
            },
        ))
        .expect("failed to create device");

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().find(|f| f.is_srgb()).copied().unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let renderer = Renderer::new(&device, format);
        let atlas = crate::atlas::GlyphAtlas::new(&self.font_family, self.font_size);

        self.surface = Some(surface);
        self.device = Some(device);
        self.queue = Some(queue);
        self.config = Some(config);
        self.renderer = Some(renderer);
        self.atlas = Some(atlas);
        self.window_size = size;
        self.window = Some(window);
    }

    fn connect_remotes(&mut self) {
        for remote in &self.remotes {
            if remote.autoconnect {
                self.connection_mgr.connect(remote.clone(), &self.hub);
            }
        }
    }

    fn process_server_events(&mut self) {
        while let Ok((remote, event)) = self.connection_mgr.event_rx.try_recv() {
            match event {
                ServerEvent::StatusChanged(status) => {
                    self.connection_mgr.update_status(&remote, status);
                    self.needs_redraw = true;
                }
                ServerEvent::ReconnectNeeded => {
                    if let Some(handle) = self.connection_mgr.connections.get(&remote) {
                        let r = handle.remote.clone();
                        let hub = self.hub.clone();
                        self.connection_mgr.connect(r, &hub);
                    }
                }
                ServerEvent::SessionList(sessions) => {
                    for s in sessions {
                        let key = SessionKey { remote: remote.clone(), pty_id: s.pty_id };
                        if !self.sessions.contains_key(&key) {
                            self.sessions.insert(key.clone(), Terminal::new());
                            self.connection_mgr.send(&remote, Command::Subscribe(s.pty_id));
                        }
                        self.titles.insert(key.clone(), s.tag.clone());
                        if self.focused.is_none() {
                            self.focused = Some(key.clone());
                            self.connection_mgr.send(&remote, Command::Focus(s.pty_id));
                            let (rows, cols) = self.terminal_size();
                            self.connection_mgr.send(&remote, Command::Resize { pty_id: s.pty_id, rows, cols });
                        }
                    }
                    self.needs_redraw = true;
                }
                ServerEvent::SessionCreated { pty_id, tag } | ServerEvent::SessionCreatedN { pty_id, tag, .. } => {
                    let key = SessionKey { remote: remote.clone(), pty_id };
                    self.sessions.insert(key.clone(), Terminal::new());
                    self.titles.insert(key.clone(), tag);
                    self.connection_mgr.send(&remote, Command::Subscribe(pty_id));
                    self.focused = Some(key.clone());
                    self.connection_mgr.send(&remote, Command::Focus(pty_id));
                    let (rows, cols) = self.terminal_size();
                    self.connection_mgr.send(&remote, Command::Resize { pty_id, rows, cols });
                    self.push_lru(key);
                    self.needs_redraw = true;
                }
                ServerEvent::SessionClosed(pty_id) => {
                    let key = SessionKey { remote: remote.clone(), pty_id };
                    self.sessions.remove(&key);
                    self.titles.remove(&key);
                    self.exited.remove(&key);
                    self.lru.retain(|k| k != &key);
                    if self.focused.as_ref() == Some(&key) {
                        self.focused = self.lru.first().cloned();
                    }
                    self.needs_redraw = true;
                }
                ServerEvent::SessionExited { pty_id, .. } => {
                    let key = SessionKey { remote: remote.clone(), pty_id };
                    self.exited.insert(key);
                    self.needs_redraw = true;
                }
                ServerEvent::FrameUpdate { pty_id, payload } => {
                    let key = SessionKey { remote: remote.clone(), pty_id };
                    if let Some(term) = self.sessions.get_mut(&key) {
                        term.feed_frame(&payload);
                    }
                    self.needs_redraw = true;
                }
                ServerEvent::TitleChanged { pty_id, title } => {
                    let key = SessionKey { remote: remote.clone(), pty_id };
                    self.titles.insert(key, title);
                    self.needs_redraw = true;
                }
                ServerEvent::Hello { .. } | ServerEvent::SearchResults { .. } | ServerEvent::Ready => {}
            }
        }
    }

    fn push_lru(&mut self, key: SessionKey) {
        self.lru.retain(|k| k != &key);
        self.lru.insert(0, key);
    }

    fn handle_app_action(&mut self, action: AppAction) {
        match action {
            AppAction::ToggleSwitcher => {
                if matches!(self.overlay, Some(OverlayKind::Switcher(_))) {
                    self.overlay = None;
                } else {
                    let mut sw = SwitcherOverlay::new();
                    let sessions = self.session_list();
                    let connected: Vec<String> = self.connection_mgr.connections.keys().cloned().collect();
                    sw.rebuild_items(&sessions, &connected, &[]);
                    self.overlay = Some(OverlayKind::Switcher(sw));
                }
            }
            AppAction::ToggleHelp => {
                if matches!(self.overlay, Some(OverlayKind::Help)) {
                    self.overlay = None;
                } else {
                    self.overlay = Some(OverlayKind::Help);
                }
            }
            AppAction::NewTerminal => {
                if let Some(ref focused) = self.focused {
                    let remote = focused.remote.clone();
                    let (rows, cols) = self.terminal_size();
                    self.connection_mgr.send(&remote, Command::CreateSession {
                        tag: "shell".into(),
                        command: None,
                        rows,
                        cols,
                    });
                } else if let Some(first) = self.connection_mgr.connections.keys().next().cloned() {
                    let (rows, cols) = self.terminal_size();
                    self.connection_mgr.send(&first, Command::CreateSession {
                        tag: "shell".into(),
                        command: None,
                        rows,
                        cols,
                    });
                }
            }
            AppAction::CloseSession => {
                if let Some(ref key) = self.focused.clone() {
                    self.connection_mgr.send(&key.remote, Command::CloseSession(key.pty_id));
                }
            }
            AppAction::CycleSessionNext | AppAction::CycleSessionPrev => {
                let keys: Vec<SessionKey> = self.sessions.keys().cloned().collect();
                if keys.is_empty() {
                    return;
                }
                let current_idx = self.focused.as_ref().and_then(|f| keys.iter().position(|k| k == f)).unwrap_or(0);
                let next = if matches!(action, AppAction::CycleSessionNext) {
                    (current_idx + 1) % keys.len()
                } else {
                    (current_idx + keys.len() - 1) % keys.len()
                };
                let key = keys[next].clone();
                self.connection_mgr.send(&key.remote, Command::Focus(key.pty_id));
                self.focused = Some(key.clone());
                self.push_lru(key);
            }
            AppAction::ScrollPageUp => {
                if let Some(ref key) = self.focused {
                    if let Some(term) = self.sessions.get_mut(key) {
                        let rows = term.rows();
                        term.scroll_offset = (term.scroll_offset + rows as u32).min(term.scrollback_lines());
                        let offset = term.scroll_offset;
                        self.connection_mgr.send(&key.remote, Command::Scroll { pty_id: key.pty_id, offset });
                        self.needs_redraw = true;
                    }
                }
            }
            AppAction::ScrollPageDown => {
                if let Some(ref key) = self.focused {
                    if let Some(term) = self.sessions.get_mut(key) {
                        let rows = term.rows() as u32;
                        term.scroll_offset = term.scroll_offset.saturating_sub(rows);
                        let offset = term.scroll_offset;
                        self.connection_mgr.send(&key.remote, Command::Scroll { pty_id: key.pty_id, offset });
                        self.needs_redraw = true;
                    }
                }
            }
            AppAction::ScrollToTop => {
                if let Some(ref key) = self.focused {
                    if let Some(term) = self.sessions.get_mut(key) {
                        term.scroll_offset = term.scrollback_lines();
                        let offset = term.scroll_offset;
                        self.connection_mgr.send(&key.remote, Command::Scroll { pty_id: key.pty_id, offset });
                        self.needs_redraw = true;
                    }
                }
            }
            AppAction::ScrollToBottom => {
                if let Some(ref key) = self.focused {
                    if let Some(term) = self.sessions.get_mut(key) {
                        term.scroll_offset = 0;
                        self.connection_mgr.send(&key.remote, Command::Scroll { pty_id: key.pty_id, offset: 0 });
                        self.needs_redraw = true;
                    }
                }
            }
            AppAction::CloseOverlay => {
                self.overlay = None;
            }
            AppAction::CyclePaneNext | AppAction::CyclePanePrev => {}
        }
        self.needs_redraw = true;
    }

    fn status_bar_height(&self) -> f32 {
        self.atlas.as_ref().map(|a| a.cell_height).unwrap_or(20.0)
    }

    fn terminal_size(&self) -> (u16, u16) {
        if let Some(ref atlas) = self.atlas {
            let cols = (self.window_size.width as f32 / atlas.cell_width) as u16;
            let rows = ((self.window_size.height as f32 - self.status_bar_height()) / atlas.cell_height) as u16;
            (rows.max(1), cols.max(1))
        } else {
            (24, 80)
        }
    }

    fn session_list(&self) -> Vec<(SessionKey, String, String)> {
        self.sessions.keys().map(|k| {
            let title = self.titles.get(k).cloned().unwrap_or_else(|| k.pty_id.to_string());
            (k.clone(), title, k.remote.clone())
        }).collect()
    }

    fn any_connected(&self) -> (bool, bool) {
        let mut connected = false;
        let mut connecting = false;
        for handle in self.connection_mgr.connections.values() {
            match &handle.status {
                ConnectionStatus::Connected => connected = true,
                ConnectionStatus::Connecting => connecting = true,
                ConnectionStatus::Disconnected(_) => {}
            }
        }
        (connected, connecting)
    }

    fn handle_overlay_key(&mut self, key: &winit::keyboard::Key) {
        use winit::keyboard::{Key, NamedKey};

        enum OverlayResult {
            None,
            FocusSession(SessionKey),
            NewTerminal,
            Close,
        }

        let sessions = self.session_list();
        let connected: Vec<String> = self.connection_mgr.connections.keys().cloned().collect();

        let result = if let Some(OverlayKind::Switcher(sw)) = self.overlay.as_mut() {
            match key {
                Key::Named(NamedKey::ArrowDown) => {
                    if !sw.items.is_empty() {
                        sw.selected = (sw.selected + 1) % sw.items.len();
                    }
                    OverlayResult::None
                }
                Key::Named(NamedKey::ArrowUp) => {
                    if !sw.items.is_empty() {
                        sw.selected = (sw.selected + sw.items.len() - 1) % sw.items.len();
                    }
                    OverlayResult::None
                }
                Key::Named(NamedKey::Enter) => {
                    match sw.items.get(sw.selected) {
                        Some(overlay::SwitcherItem::Session { key, .. }) => {
                            OverlayResult::FocusSession(key.clone())
                        }
                        Some(overlay::SwitcherItem::Action { action, .. }) => {
                            match action {
                                overlay::SwitcherAction::NewTerminal => OverlayResult::NewTerminal,
                                _ => OverlayResult::Close,
                            }
                        }
                        _ => OverlayResult::None,
                    }
                }
                Key::Named(NamedKey::Backspace) => {
                    sw.input.pop();
                    sw.rebuild_items(&sessions, &connected, &[]);
                    sw.selected = sw.selected.min(sw.items.len().saturating_sub(1));
                    OverlayResult::None
                }
                Key::Character(ch) => {
                    sw.input.push_str(ch.as_str());
                    sw.rebuild_items(&sessions, &connected, &[]);
                    sw.selected = 0;
                    OverlayResult::None
                }
                _ => OverlayResult::None,
            }
        } else {
            OverlayResult::None
        };

        match result {
            OverlayResult::FocusSession(k) => {
                self.connection_mgr.send(&k.remote, Command::Focus(k.pty_id));
                self.focused = Some(k.clone());
                self.push_lru(k);
                self.overlay = None;
            }
            OverlayResult::NewTerminal => {
                self.overlay = None;
                self.handle_app_action(AppAction::NewTerminal);
            }
            OverlayResult::Close => {
                self.overlay = None;
            }
            OverlayResult::None => {}
        }
    }

    fn pixel_to_cell(&self, x: f64, y: f64) -> Option<(u16, u16)> {
        let atlas = self.atlas.as_ref()?;
        let col = (x as f32 / atlas.cell_width) as i32;
        let row = (y as f32 / atlas.cell_height) as i32;
        let (rows, cols) = self.terminal_size();
        if col < 0 || row < 0 {
            return None;
        }
        Some((row.min(rows as i32 - 1) as u16, col.min(cols as i32 - 1) as u16))
    }

    fn send_mouse(&mut self, kind: u8, button: u8, col: u16, row: u16) {
        if let Some(ref key) = self.focused.clone() {
            self.connection_mgr.send(&key.remote, Command::Mouse {
                pty_id: key.pty_id,
                kind,
                button,
                col,
                row,
            });
        }
    }

    fn focused_mouse_mode(&self) -> u8 {
        self.focused.as_ref()
            .and_then(|k| self.sessions.get(k))
            .map(|t| t.mouse_mode())
            .unwrap_or(0)
    }

    fn render(&mut self) {
        if self.device.is_none() || self.queue.is_none() || self.surface.is_none()
            || self.renderer.is_none() || self.atlas.is_none()
        {
            return;
        }

        let now = Instant::now();
        if now.duration_since(self.last_blink).as_millis() > 530 {
            self.blink_visible = !self.blink_visible;
            self.last_blink = now;
        }

        let (rows, cols) = self.terminal_size();
        let (connected, connecting) = self.any_connected();
        let focused_name: Option<String> = self.focused.as_ref().and_then(|k| self.titles.get(k)).cloned();

        let device = self.device.as_ref().unwrap();
        let queue = self.queue.as_ref().unwrap();
        let surface = self.surface.as_ref().unwrap();
        let renderer = self.renderer.as_mut().unwrap();
        let atlas = self.atlas.as_mut().unwrap();

        renderer.update_resolution(queue, self.window_size.width as f32, self.window_size.height as f32);

        let cell_w = atlas.cell_width;
        let cell_h = atlas.cell_height;

        let mut all_bg = Vec::new();
        let mut all_glyph = Vec::new();
        let mut all_cursor = Vec::new();

        if let Some(ref key) = self.focused {
            if let Some(term) = self.sessions.get_mut(key) {
                term.prepare_vertices(self.palette, atlas, cell_w, cell_h, 0.0, 0.0);
                all_bg.extend_from_slice(&term.bg_verts);
                all_glyph.extend_from_slice(&term.glyph_verts);
                if term.cursor_visible() {
                    let cv = terminal::cursor_verts(
                        term.cursor_row(), term.cursor_col(), term.cursor_style(),
                        cell_w, cell_h, 0.0, 0.0, true, self.blink_visible,
                    );
                    all_cursor.extend_from_slice(&cv);
                }
            }
        }
        crate::statusbar::render_status_bar(
            &mut all_bg,
            &mut all_glyph,
            atlas,
            self.palette,
            self.window_size.width as f32,
            self.window_size.height as f32,
            self.sessions.len(),
            self.exited.len(),
            focused_name.as_deref(),
            cols,
            rows,
            connected,
            connecting,
        );

        let bg_color = [
            self.palette.bg[0] as f32 / 255.0,
            self.palette.bg[1] as f32 / 255.0,
            self.palette.bg[2] as f32 / 255.0,
        ];

        let mut overlay_bg = Vec::new();
        let mut overlay_glyph = Vec::new();
        if let Some(ref ov) = self.overlay {
            let w = self.window_size.width as f32;
            let h = self.window_size.height as f32;
            overlay_bg = overlay::render_overlay_bg(ov, w, h, self.palette);
            overlay::render_overlay_glyphs(ov, &mut overlay_glyph, &mut overlay_bg, atlas, w, h, self.palette);
        }

        renderer.update_atlas(device, queue, atlas);

        let frame = match surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => return,
        };
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("render") });

        renderer.render(
            &mut encoder,
            &view,
            bg_color,
            &all_bg,
            &all_glyph,
            &all_cursor,
            &[],
            &overlay_bg,
            &overlay_glyph,
            device,
        );

        queue.submit(std::iter::once(encoder.finish()));
        frame.present();
    }
}

impl ApplicationHandler<()> for App {
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        self.process_server_events();
        if self.needs_redraw {
            if let Some(ref w) = self.window {
                w.request_redraw();
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("blit")
            .with_inner_size(PhysicalSize::new(1024u32, 768u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("failed to create window"));
        self.init_gpu(window);
        self.connect_remotes();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::Resized(new_size) => {
                self.window_size = new_size;
                if let (Some(surface), Some(device), Some(config)) = (self.surface.as_ref(), self.device.as_ref(), self.config.as_mut()) {
                    config.width = new_size.width.max(1);
                    config.height = new_size.height.max(1);
                    surface.configure(device, config);
                }
                if let Some(ref focused) = self.focused {
                    let (rows, cols) = self.terminal_size();
                    self.connection_mgr.send(&focused.remote, Command::Resize {
                        pty_id: focused.pty_id,
                        rows,
                        cols,
                    });
                }
                self.needs_redraw = true;
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let Some(action) = input::check_app_keybinding(
                        &event.logical_key,
                        &event.physical_key,
                        &self.modifiers,
                        event.state,
                    ) {
                        self.handle_app_action(action);
                        return;
                    }
                }

                if self.overlay.is_some() && event.state == ElementState::Pressed {
                    self.handle_overlay_key(&event.logical_key);
                    self.needs_redraw = true;
                    return;
                }

                if event.state == ElementState::Pressed {
                    if let Some(ref key) = self.focused.clone() {
                        let app_cursor = self.sessions.get(key).map(|t| t.app_cursor()).unwrap_or(false);
                        if let Some(bytes) = input::key_to_bytes(
                            &event.logical_key,
                            &event.physical_key,
                            &self.modifiers,
                            app_cursor,
                        ) {
                            self.connection_mgr.send(&key.remote, Command::Input {
                                pty_id: key.pty_id,
                                data: bytes,
                            });
                            if let Some(term) = self.sessions.get_mut(key) {
                                if term.scroll_offset > 0 {
                                    term.scroll_offset = 0;
                                    self.connection_mgr.send(&key.remote, Command::Scroll {
                                        pty_id: key.pty_id,
                                        offset: 0,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_position = (position.x, position.y);
                let mouse_mode = self.focused_mouse_mode();
                if mouse_mode >= 3 && self.mouse_buttons > 0 {
                    if let Some((row, col)) = self.pixel_to_cell(position.x, position.y) {
                        let drag_button = (self.mouse_buttons - 1) + 32;
                        self.send_mouse(2, drag_button, col, row);
                    }
                } else if mouse_mode >= 4 {
                    if let Some((row, col)) = self.pixel_to_cell(position.x, position.y) {
                        self.send_mouse(2, 35, col, row);
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if self.overlay.is_some() {
                    return;
                }
                let mouse_mode = self.focused_mouse_mode();
                if mouse_mode == 0 {
                    return;
                }
                let btn = match button {
                    winit::event::MouseButton::Left => 0u8,
                    winit::event::MouseButton::Middle => 1,
                    winit::event::MouseButton::Right => 2,
                    _ => return,
                };
                if let Some((row, col)) = self.pixel_to_cell(self.cursor_position.0, self.cursor_position.1) {
                    match state {
                        ElementState::Pressed => {
                            self.mouse_buttons = btn + 1;
                            self.send_mouse(0, btn, col, row);
                        }
                        ElementState::Released => {
                            self.mouse_buttons = 0;
                            if mouse_mode >= 2 {
                                self.send_mouse(1, btn, col, row);
                            }
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.overlay.is_some() {
                    return;
                }
                let mouse_mode = self.focused_mouse_mode();
                let lines = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y,
                    winit::event::MouseScrollDelta::PixelDelta(pos) => {
                        let cell_h = self.atlas.as_ref().map(|a| a.cell_height).unwrap_or(20.0);
                        pos.y as f32 / cell_h
                    }
                };
                if mouse_mode > 0 {
                    if let Some((row, col)) = self.pixel_to_cell(self.cursor_position.0, self.cursor_position.1) {
                        let button = if lines > 0.0 { 64u8 } else { 65u8 };
                        let ticks = lines.abs().ceil() as u32;
                        for _ in 0..ticks.min(5) {
                            self.send_mouse(0, button, col, row);
                        }
                    }
                } else if let Some(ref key) = self.focused.clone() {
                    if let Some(term) = self.sessions.get_mut(key) {
                        let scroll_lines = (lines.abs() * 3.0).ceil() as u32;
                        if lines > 0.0 {
                            term.scroll_offset = (term.scroll_offset + scroll_lines).min(term.scrollback_lines());
                        } else {
                            term.scroll_offset = term.scroll_offset.saturating_sub(scroll_lines);
                        }
                        let offset = term.scroll_offset;
                        self.connection_mgr.send(&key.remote, Command::Scroll { pty_id: key.pty_id, offset });
                        self.needs_redraw = true;
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.process_server_events();
                if self.needs_redraw {
                    self.render();
                    self.needs_redraw = false;
                }
                let now = Instant::now();
                if now.duration_since(self.last_blink).as_millis() > 530 {
                    self.needs_redraw = true;
                }
                if self.needs_redraw {
                    if let Some(ref w) = self.window {
                        w.request_redraw();
                    }
                }
            }
            _ => {}
        }
    }
}
