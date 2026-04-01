use std::collections::BTreeMap;

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::search::RegexSearch;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::NamedColor;
use blit_remote::FrameState;

const CELL_SIZE: usize = blit_remote::CELL_SIZE;

// ── Search scoring constants ────────────────────────────────────────────

const SEARCH_TITLE_BASE: u32 = 1400;
const SEARCH_TITLE_PREFIX_BONUS: u32 = 240;
const SEARCH_TITLE_MATCH_BONUS: u32 = 120;
const SEARCH_VISIBLE_BASE: u32 = 360;
const SEARCH_VISIBLE_LINE_BONUS: u32 = 32;
const SEARCH_SCROLLBACK_BASE: u32 = 120;
const SEARCH_SCROLLBACK_LINE_BONUS: u32 = 12;
const SEARCH_CONTEXT_BEFORE: usize = 28;
const SEARCH_CONTEXT_AFTER: usize = 52;

pub const SEARCH_MATCH_TITLE: u8 = 1 << 0;
pub const SEARCH_MATCH_VISIBLE: u8 = 1 << 1;
pub const SEARCH_MATCH_SCROLLBACK: u8 = 1 << 2;

// ── Mode tracking ───────────────────────────────────────────────────────
// alacritty_terminal doesn't directly expose mouse mode/encoding as simple
// integers, so we track them ourselves by scanning the raw PTY output, same
// as the old wezterm driver.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum EscapeParseState {
    Ground,
    Escape,
    Csi(CsiState),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CsiState {
    private: bool,
    bang: bool,
    space: bool,
    params: [u16; 8],
    current: Option<u16>,
    len: u8,
}

impl CsiState {
    fn push_current(&mut self) {
        if self.len < 8 {
            self.params[self.len as usize] = self.current.unwrap_or(0);
            self.len += 1;
        }
        self.current = None;
    }
    fn params(&self) -> &[u16] {
        &self.params[..self.len as usize]
    }
}

#[derive(Clone, Debug)]
struct ModeTracker {
    app_cursor: bool,
    app_keypad: bool,
    alt_screen: bool,
    mouse_mode: u16,
    mouse_encoding: u16,
    cursor_style: u16,
    synced_output: bool,
    parse_state: EscapeParseState,
}

impl Default for ModeTracker {
    fn default() -> Self {
        Self {
            app_cursor: false,
            app_keypad: false,
            alt_screen: false,
            mouse_mode: 0,
            mouse_encoding: 0,
            cursor_style: 0,
            synced_output: false,
            parse_state: EscapeParseState::Ground,
        }
    }
}

impl ModeTracker {
    fn process(&mut self, data: &[u8]) {
        for &byte in data {
            match self.parse_state {
                EscapeParseState::Ground => {
                    if byte == 0x1b {
                        self.parse_state = EscapeParseState::Escape;
                    }
                }
                EscapeParseState::Escape => match byte {
                    b'[' => self.parse_state = EscapeParseState::Csi(CsiState::default()),
                    b'=' => {
                        self.app_keypad = true;
                        self.parse_state = EscapeParseState::Ground;
                    }
                    b'>' => {
                        self.app_keypad = false;
                        self.parse_state = EscapeParseState::Ground;
                    }
                    b'c' => {
                        self.reset();
                        self.parse_state = EscapeParseState::Ground;
                    }
                    0x1b => {}
                    _ => self.parse_state = EscapeParseState::Ground,
                },
                EscapeParseState::Csi(mut csi) => {
                    if byte == 0x1b {
                        self.parse_state = EscapeParseState::Escape;
                        continue;
                    }
                    match byte {
                        b'?' if !csi.private
                            && csi.len == 0
                            && csi.current.is_none()
                            && !csi.bang =>
                        {
                            csi.private = true;
                            self.parse_state = EscapeParseState::Csi(csi);
                        }
                        b'0'..=b'9' => {
                            let digit = (byte - b'0') as u16;
                            let current = csi
                                .current
                                .unwrap_or(0)
                                .saturating_mul(10)
                                .saturating_add(digit);
                            csi.current = Some(current);
                            self.parse_state = EscapeParseState::Csi(csi);
                        }
                        b';' => {
                            csi.push_current();
                            self.parse_state = EscapeParseState::Csi(csi);
                        }
                        b'!' => {
                            csi.bang = true;
                            self.parse_state = EscapeParseState::Csi(csi);
                        }
                        b' ' => {
                            csi.space = true;
                            self.parse_state = EscapeParseState::Csi(csi);
                        }
                        0x40..=0x7e => {
                            csi.push_current();
                            self.handle_csi(csi, byte);
                            self.parse_state = EscapeParseState::Ground;
                        }
                        _ => self.parse_state = EscapeParseState::Ground,
                    }
                }
            }
        }
    }

    fn reset(&mut self) {
        self.app_cursor = false;
        self.app_keypad = false;
        self.alt_screen = false;
        self.mouse_mode = 0;
        self.mouse_encoding = 0;
        self.cursor_style = 0;
        self.synced_output = false;
    }

    fn soft_reset(&mut self) {
        self.app_cursor = false;
        self.app_keypad = false;
        self.cursor_style = 0;
    }

    fn handle_csi(&mut self, csi: CsiState, final_byte: u8) {
        if csi.bang && final_byte == b'p' {
            self.soft_reset();
            return;
        }
        if csi.space && final_byte == b'q' {
            let style = csi.params().first().copied().unwrap_or(0);
            self.cursor_style = if style <= 6 { style } else { 0 };
            return;
        }
        let set = match final_byte {
            b'h' => true,
            b'l' => false,
            _ => return,
        };
        for &param in csi.params() {
            if csi.private {
                match param {
                    1 => self.app_cursor = set,
                    47 | 1047 | 1049 => self.alt_screen = set,
                    9 | 1000 | 1002 | 1003 => self.update_mouse_mode(param, set),
                    1005 | 1006 | 1016 => self.update_mouse_encoding(param, set),
                    2026 => self.synced_output = set,
                    _ => {}
                }
            }
        }
    }

    fn update_mouse_mode(&mut self, param: u16, set: bool) {
        let mode = match param {
            9 => 1,
            1000 => 2,
            1002 => 3,
            1003 => 4,
            _ => return,
        };
        if set {
            self.mouse_mode = mode;
        } else if self.mouse_mode == mode {
            self.mouse_mode = 0;
        }
    }

    fn update_mouse_encoding(&mut self, param: u16, set: bool) {
        let encoding = match param {
            1005 => 1,
            1006 => 2,
            1016 => 3,
            _ => return,
        };
        if set {
            self.mouse_encoding = encoding;
        } else if self.mouse_encoding == encoding {
            self.mouse_encoding = 0;
        }
    }

    fn pack(&self, cursor_visible: bool, bracketed_paste: bool, echo: bool, icanon: bool) -> u16 {
        let mut mode = 0u16;
        if cursor_visible {
            mode |= 1;
        }
        if self.app_cursor {
            mode |= 1 << 1;
        }
        if self.app_keypad {
            mode |= 1 << 2;
        }
        if bracketed_paste {
            mode |= 1 << 3;
        }
        mode |= self.mouse_mode << 4;
        mode |= self.mouse_encoding << 7;
        if echo {
            mode |= 1 << 9;
        }
        if icanon {
            mode |= 1 << 10;
        }
        if self.alt_screen {
            mode |= 1 << 11;
        }
        mode |= (self.cursor_style & 7) << 12;
        mode
    }
}

// ── No-op sync timeout ──────────────────────────────────────────────────
// Disables Processor's built-in ?2026 sync buffering. We handle sync
// deferral in the server's snapshot logic — the Processor buffering would
// double-parse every byte (buffer on ?2026h, then replay on stop_sync).

#[derive(Default)]
struct NoSyncTimeout;

impl alacritty_terminal::vte::ansi::Timeout for NoSyncTimeout {
    fn set_timeout(&mut self, _: std::time::Duration) {}
    fn clear_timeout(&mut self) {}
    fn pending_timeout(&self) -> bool {
        false
    }
}

// ── Event proxy ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct BlitEventProxy {
    title: Arc<Mutex<Option<String>>>,
}

impl BlitEventProxy {
    fn new() -> Self {
        Self {
            title: Arc::new(Mutex::new(None)),
        }
    }
    fn take_title(&self) -> Option<String> {
        self.title.lock().unwrap().take()
    }
}

impl EventListener for BlitEventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(t) => {
                *self.title.lock().unwrap() = Some(t);
            }
            Event::ResetTitle => {
                *self.title.lock().unwrap() = Some(String::new());
            }
            _ => {}
        }
    }
}

// ── Dimensions adapter ──────────────────────────────────────────────────

struct TermDims {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermDims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

// ── Search types ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SearchSource {
    Title = 0,
    Visible = 1,
    Scrollback = 2,
}

impl SearchSource {
    fn mask(self) -> u8 {
        1 << (self as u8)
    }
}

pub struct SearchResult {
    pub score: u32,
    pub primary_source: SearchSource,
    pub matched_sources: u8,
    pub context: String,
    pub scroll_offset: Option<usize>,
}

#[derive(Clone)]
#[allow(dead_code)]
struct SearchCandidate {
    score: u32,
    source: SearchSource,
    context: String,
    scroll_offset: Option<usize>,
}

// ── Main driver ─────────────────────────────────────────────────────────

pub struct TerminalDriver {
    term: Term<BlitEventProxy>,
    processor: alacritty_terminal::vte::ansi::Processor<NoSyncTimeout>,
    event_proxy: BlitEventProxy,
    modes: ModeTracker,
    title: String,
    title_dirty: bool,
    saw_explicit_title: bool,
}

impl TerminalDriver {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        let dims = TermDims {
            cols: cols as usize,
            rows: rows as usize,
        };
        let event_proxy = BlitEventProxy::new();
        let term = Term::new(config, &dims, event_proxy.clone());

        Self {
            term,
            processor: alacritty_terminal::vte::ansi::Processor::default(),
            event_proxy,
            modes: ModeTracker::default(),
            title: String::new(),
            title_dirty: false,
            saw_explicit_title: false,
        }
    }

    pub fn process(&mut self, data: &[u8]) {
        self.modes.process(data);
        self.processor.advance(&mut self.term, data);
        self.refresh_title();
    }

    pub fn size(&self) -> (u16, u16) {
        let grid = self.term.grid();
        (grid.screen_lines() as u16, grid.columns() as u16)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let dims = TermDims {
            cols: cols as usize,
            rows: rows as usize,
        };
        self.term.resize(dims);
    }

    pub fn reset_modes(&mut self) {
        self.modes.reset();
    }

    pub fn mouse_event(
        &self,
        type_: u8,
        button: u8,
        col: u16,
        row: u16,
        echo: bool,
        icanon: bool,
    ) -> Option<Vec<u8>> {
        if self.modes.mouse_mode == 0 {
            return None;
        }
        if echo && icanon {
            return None;
        }

        let mode = self.modes.mouse_mode;
        match type_ {
            0 | 1 => {} // down/up
            2 => {
                if mode < 3 {
                    return None;
                }
                if mode == 3 && (3..64).contains(&button) {
                    return None;
                }
            }
            _ => return None,
        }

        let enc = self.modes.mouse_encoding;
        if enc == 2 {
            let cb = match type_ {
                1 => button,
                2 => button | 32,
                _ => button,
            };
            let suffix = if type_ == 1 { b'm' } else { b'M' };
            Some(format!("\x1b[<{};{};{}{}", cb, col + 1, row + 1, suffix as char).into_bytes())
        } else {
            let cb = match type_ {
                1 => 3u8,
                2 => button.wrapping_add(32),
                _ => button,
            };
            if col > 222 || row > 222 {
                return None;
            }
            Some(vec![
                0x1b,
                0x5b,
                0x4d,
                cb.wrapping_add(32),
                (col as u8).wrapping_add(33),
                (row as u8).wrapping_add(33),
            ])
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn take_title_dirty(&mut self) -> bool {
        std::mem::take(&mut self.title_dirty)
    }

    pub fn synced_output(&self) -> bool {
        self.modes.synced_output
    }

    pub fn total_lines(&self) -> u32 {
        self.term.grid().total_lines() as u32
    }

    pub fn cursor_position(&self) -> (u16, u16) {
        let cursor = self.term.grid().cursor.point;
        (cursor.line.0 as u16, cursor.column.0 as u16)
    }

    pub fn snapshot(&mut self, echo: bool, icanon: bool) -> FrameState {
        let (rows, cols) = self.size();
        let mode = self.pack_mode(echo, icanon);
        let cursor = self.term.grid().cursor.point;
        let cursor_row = (cursor.line.0 as u16).min(rows.saturating_sub(1));
        let cursor_col = (cursor.column.0 as u16).min(cols.saturating_sub(1));

        let total = self.term.grid().total_lines();
        let screen = self.term.grid().screen_lines();
        let scrollback_lines = total.saturating_sub(screen);

        let mut frame = self.build_frame(
            0,
            rows as usize,
            cols as usize,
            cursor_row,
            cursor_col,
            mode,
        );
        frame.set_scrollback_lines(scrollback_lines.min(u32::MAX as usize) as u32);
        frame
    }

    pub fn scrollback_frame(&mut self, offset: usize) -> FrameState {
        let (rows, cols) = self.size();
        let total = self.term.grid().total_lines();
        let screen = self.term.grid().screen_lines();
        let scrollback_lines = total.saturating_sub(screen);

        let mut frame = self.build_frame(offset, rows as usize, cols as usize, 0, 0, 0);
        frame.set_scrollback_lines(scrollback_lines.min(u32::MAX as usize) as u32);
        frame
    }

    pub fn get_text_range(
        &self,
        start_tail: u32,
        start_col: u16,
        end_tail: u32,
        end_col: u16,
    ) -> String {
        let grid = self.term.grid();
        let total = grid.total_lines();
        let screen = grid.screen_lines();
        let history = total.saturating_sub(screen);
        let cols = grid.columns();

        let last_line = Line(screen as i32 - 1);
        let tail_to_line = |tail: u32| -> Line {
            Line(last_line.0 - tail as i32)
        };

        let start_line = tail_to_line(start_tail);
        let end_line = tail_to_line(end_tail);

        let min_line = -(history as i32);

        let mut result = String::new();
        let mut line_i = start_line.0.max(min_line);
        let end_i = end_line.0.min(last_line.0);

        while line_i <= end_i {
            let line_idx = Line(line_i);
            let grid_row = &grid[line_idx];
            let c0 = if line_i == start_line.0 {
                (start_col as usize).min(cols.saturating_sub(1))
            } else {
                0
            };
            let c1 = if line_i == end_line.0 {
                (end_col as usize).min(cols.saturating_sub(1))
            } else {
                cols.saturating_sub(1)
            };

            let mut line_text = String::new();
            for col_idx in c0..=c1 {
                let cell = &grid_row[Column(col_idx)];
                if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let c = if cell.c == '\0' { ' ' } else { cell.c };
                line_text.push(c);
                for &zw in cell.zerowidth().into_iter().flatten() {
                    line_text.push(zw);
                }
            }
            result.push_str(line_text.trim_end());

            let is_wrapped = grid_row
                .last()
                .is_some_and(|c| c.flags.contains(CellFlags::WRAPLINE));
            if line_i < end_i && !is_wrapped {
                result.push('\n');
            }

            line_i += 1;
        }
        result
    }

    pub fn search_result(&self, query: &str) -> Option<SearchResult> {
        let query = query.trim();
        if query.is_empty() {
            return None;
        }

        let grid = self.term.grid();
        let screen = grid.screen_lines();

        // Build regex — the query IS a regex pattern (case-insensitive).
        let mut regex = match RegexSearch::new(&format!("(?i){query}")) {
            Ok(r) => r,
            Err(_) => return None, // invalid regex
        };

        // ── Title match (regex on title string) ─────────────────────
        let title_candidate = if !self.title.is_empty() {
            regex::RegexBuilder::new(query)
                .case_insensitive(true)
                .build()
                .ok()
                .and_then(|re| re.find(&self.title))
                .map(|m| {
                    let idx = m.start();
                    let start = idx.saturating_sub(SEARCH_CONTEXT_BEFORE);
                    let end = (m.end() + SEARCH_CONTEXT_AFTER).min(self.title.len());
                    let mut score = SEARCH_TITLE_BASE + SEARCH_TITLE_MATCH_BONUS;
                    if idx == 0 {
                        score += SEARCH_TITLE_PREFIX_BONUS;
                    }
                    SearchCandidate {
                        score,
                        source: SearchSource::Title,
                        context: self.title[start..end].to_owned(),
                        scroll_offset: None,
                    }
                })
        } else {
            None
        };

        // Search forward from the top of the viewport for a visible match.
        let viewport_top = Point::new(Line(0), Column(0));
        let visible_match = self
            .term
            .search_next(&mut regex, viewport_top, Direction::Right, Side::Left, None)
            .filter(|m| m.start().line.0 >= 0 && m.start().line.0 < screen as i32);

        let visible_candidate = visible_match.as_ref().map(|m| {
            let context = self.extract_match_context(m);
            SearchCandidate {
                score: SEARCH_VISIBLE_BASE + SEARCH_VISIBLE_LINE_BONUS,
                source: SearchSource::Visible,
                context,
                scroll_offset: None,
            }
        });

        // Search backward from viewport top for a scrollback match.
        let scrollback_match = if grid.total_lines() > screen {
            let history_top = Point::new(Line(-(grid.history_size() as i32)), Column(0));
            self.term
                .search_next(&mut regex, viewport_top, Direction::Left, Side::Left, None)
                .filter(|m| m.start().line.0 < 0)
                .or_else(|| {
                    // Also try forward from the very top of history
                    self.term
                        .search_next(&mut regex, history_top, Direction::Right, Side::Left, None)
                        .filter(|m| m.start().line.0 < 0)
                })
        } else {
            None
        };

        let scrollback_candidate = scrollback_match.as_ref().map(|m| {
            // Convert match line to scroll offset.
            // Line(-1) = 1 line above viewport = scroll_offset 1
            let offset = (-m.start().line.0) as usize;
            let context = self.extract_match_context(m);
            SearchCandidate {
                score: SEARCH_SCROLLBACK_BASE + SEARCH_SCROLLBACK_LINE_BONUS,
                source: SearchSource::Scrollback,
                context,
                scroll_offset: Some(offset),
            }
        });

        // ── Combine results ─────────────────────────────────────────
        let mut total_score = 0u32;
        let mut matched_sources = 0u8;
        let mut primary: Option<SearchCandidate> = None;
        let mut jump: Option<SearchCandidate> = None;

        for candidate in [title_candidate, visible_candidate, scrollback_candidate]
            .into_iter()
            .flatten()
        {
            total_score = total_score.saturating_add(candidate.score);
            matched_sources |= candidate.source.mask();
            if candidate.scroll_offset.is_some()
                && jump
                    .as_ref()
                    .is_none_or(|best| candidate.score > best.score)
            {
                jump = Some(candidate.clone());
            }
            if primary
                .as_ref()
                .is_none_or(|best| candidate.score > best.score)
            {
                primary = Some(candidate);
            }
        }

        let primary = primary?;
        Some(SearchResult {
            score: total_score,
            primary_source: primary.source,
            matched_sources,
            context: primary.context,
            scroll_offset: jump.and_then(|c| c.scroll_offset),
        })
    }

    /// Extract text around a search match for context display.
    fn extract_match_context(&self, m: &std::ops::RangeInclusive<Point>) -> String {
        let grid = self.term.grid();
        let cols = grid.columns();
        let line = m.start().line;

        // Extract the full line text
        if line.0 < -(grid.history_size() as i32) || line.0 >= grid.screen_lines() as i32 {
            return String::new();
        }
        let row = &grid[line];
        let mut text = String::new();
        for col in 0..cols {
            let cell = &row[Column(col)];
            if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                continue;
            }
            let c = if cell.c == '\0' { ' ' } else { cell.c };
            text.push(c);
        }
        let text = text.trim_end().to_owned();

        // Trim to context window around the match column
        let match_col = m.start().column.0;
        let start = match_col.saturating_sub(SEARCH_CONTEXT_BEFORE);
        let end = (match_col + SEARCH_CONTEXT_AFTER).min(text.len());
        if start < text.len() {
            text[start..end.min(text.len())].to_owned()
        } else {
            text
        }
    }

    // ── Private helpers ─────────────────────────────────────────────────

    fn refresh_title(&mut self) {
        if let Some(new_title) = self.event_proxy.take_title() {
            if !new_title.is_empty() || self.saw_explicit_title {
                self.saw_explicit_title = true;
            }
            let title = if self.saw_explicit_title {
                new_title
            } else {
                String::new()
            };
            if title != self.title {
                self.title = title;
                self.title_dirty = true;
            }
        }
    }

    fn pack_mode(&self, echo: bool, icanon: bool) -> u16 {
        let term_mode = self.term.mode();
        let cursor_visible = term_mode.contains(TermMode::SHOW_CURSOR);
        let bracketed_paste = term_mode.contains(TermMode::BRACKETED_PASTE);
        self.modes
            .pack(cursor_visible, bracketed_paste, echo, icanon)
    }

    fn build_frame(
        &self,
        scroll_offset: usize,
        rows: usize,
        cols: usize,
        cursor_row: u16,
        cursor_col: u16,
        mode: u16,
    ) -> FrameState {
        let grid = self.term.grid();
        let mut cells = vec![0u8; rows * cols * CELL_SIZE];
        let mut overflow = BTreeMap::new();

        let total = grid.total_lines();
        let screen = grid.screen_lines();
        let history = total.saturating_sub(screen);

        for row in 0..rows {
            // Line indexing: Line(0) is top of viewport, Line(-(n+1)) is scrollback
            let line_idx = if scroll_offset == 0 {
                Line(row as i32)
            } else {
                // Scrollback: go `scroll_offset` lines above the viewport top
                let hist_line = history as i32 - scroll_offset as i32 + row as i32;
                if hist_line < 0 {
                    continue;
                }
                // Convert to grid line: negative = history
                Line(row as i32 - scroll_offset as i32)
            };

            if line_idx.0 < -(history as i32) || line_idx.0 >= screen as i32 {
                continue;
            }

            let grid_row = &grid[line_idx];
            let row_start = row * cols;

            for col_idx in 0..cols {
                let cell = &grid_row[Column(col_idx)];
                let flat = row_start + col_idx;
                encode_cell(
                    cell,
                    &mut cells[flat * CELL_SIZE..][..CELL_SIZE],
                    flat,
                    &mut overflow,
                );
            }

            // Line wrapping
            if grid_row
                .last()
                .is_some_and(|c| c.flags.contains(CellFlags::WRAPLINE))
            {
                // set wrapped flag on frame
            }
        }

        let mut frame = FrameState::from_parts(
            rows as u16,
            cols as u16,
            cursor_row,
            cursor_col,
            mode,
            self.title.clone(),
            cells,
        );
        *frame.overflow_mut() = overflow;

        // Set line wrap flags
        for row in 0..rows {
            let line_idx = if scroll_offset == 0 {
                Line(row as i32)
            } else {
                Line(row as i32 - scroll_offset as i32)
            };
            let history = (grid.total_lines() - grid.screen_lines()) as i32;
            if line_idx.0 < -history || line_idx.0 >= screen as i32 {
                continue;
            }
            let grid_row = &grid[line_idx];
            if grid_row
                .last()
                .is_some_and(|c| c.flags.contains(CellFlags::WRAPLINE))
            {
                frame.set_wrapped(row as u16, true);
            }
        }

        frame
    }
}

// ── Cell encoding ───────────────────────────────────────────────────────

/// Encode a cell into the 12-byte blit wire format.
/// Hot path — called 240K+ times per frame at large terminal sizes.
#[inline(always)]
fn encode_cell(
    cell: &alacritty_terminal::term::cell::Cell,
    buf: &mut [u8],
    flat_index: usize,
    overflow: &mut BTreeMap<usize, String>,
) {
    use alacritty_terminal::vte::ansi::Color;

    // Fast path: encode fg+bg colors inline to avoid function call overhead.
    let mut f0 = 0u8;
    match &cell.fg {
        Color::Named(NamedColor::Foreground) => {}
        Color::Named(n) => {
            f0 |= 1;
            buf[2] = *n as u8;
            buf[3] = 0;
            buf[4] = 0;
        }
        Color::Indexed(i) => {
            f0 |= 1;
            buf[2] = *i;
            buf[3] = 0;
            buf[4] = 0;
        }
        Color::Spec(rgb) => {
            f0 |= 2;
            buf[2] = rgb.r;
            buf[3] = rgb.g;
            buf[4] = rgb.b;
        }
    }
    match &cell.bg {
        Color::Named(NamedColor::Background) => {
            buf[5] = 0;
            buf[6] = 0;
            buf[7] = 0;
        }
        Color::Named(n) => {
            f0 |= 1 << 2;
            buf[5] = *n as u8;
            buf[6] = 0;
            buf[7] = 0;
        }
        Color::Indexed(i) => {
            f0 |= 1 << 2;
            buf[5] = *i;
            buf[6] = 0;
            buf[7] = 0;
        }
        Color::Spec(rgb) => {
            f0 |= 2 << 2;
            buf[5] = rgb.r;
            buf[6] = rgb.g;
            buf[7] = rgb.b;
        }
    }

    let flags = cell.flags;
    if flags.contains(CellFlags::BOLD) {
        f0 |= 1 << 4;
    }
    if flags.contains(CellFlags::DIM) {
        f0 |= 1 << 5;
    }
    if flags.contains(CellFlags::ITALIC) {
        f0 |= 1 << 6;
    }
    if flags.intersects(
        CellFlags::UNDERLINE
            | CellFlags::DOUBLE_UNDERLINE
            | CellFlags::UNDERCURL
            | CellFlags::DOTTED_UNDERLINE
            | CellFlags::DASHED_UNDERLINE,
    ) {
        f0 |= 1 << 7;
    }
    buf[0] = f0;

    let mut f1 = 0u8;
    if flags.contains(CellFlags::INVERSE) {
        f1 |= 1;
    }
    if flags.contains(CellFlags::WIDE_CHAR) {
        f1 |= 1 << 1;
    }

    // Encode character content — fast path for ASCII (most common).
    let c = cell.c;
    if c <= '\x7f' && c > ' ' && cell.extra.is_none() {
        // Single ASCII byte, no zero-width chars.
        f1 |= 1 << 3; // content_len = 1
        buf[8] = c as u8;
        buf[9] = 0;
        buf[10] = 0;
        buf[11] = 0;
    } else if c == '\0' || c == ' ' {
        buf[8] = 0;
        buf[9] = 0;
        buf[10] = 0;
        buf[11] = 0;
    } else {
        let mut char_buf = [0u8; 4];
        let s = c.encode_utf8(&mut char_buf);
        let zw = cell.zerowidth();
        if let Some(zw) = zw {
            let mut full = String::from(c);
            for &zc in zw {
                full.push(zc);
            }
            let bytes = full.as_bytes();
            if bytes.len() <= 4 {
                f1 |= (bytes.len() as u8) << 3;
                buf[8..8 + bytes.len()].copy_from_slice(bytes);
                buf[8 + bytes.len()..12].fill(0);
            } else {
                f1 |= 7 << 3;
                let hash = fnv1a_32(bytes);
                buf[8..12].copy_from_slice(&hash.to_le_bytes());
                overflow.insert(flat_index, full);
            }
        } else {
            let len = s.len();
            f1 |= (len as u8) << 3;
            buf[8..8 + len].copy_from_slice(s.as_bytes());
            buf[8 + len..12].fill(0);
        }
    }
    buf[1] = f1;
}

fn fnv1a_32(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_terminal_creation() {
        let driver = TerminalDriver::new(24, 80, 1000);
        assert_eq!(driver.size(), (24, 80));
        assert_eq!(driver.title(), "");
        assert_eq!(driver.cursor_position(), (0, 0));
    }

    #[test]
    fn process_text() {
        let mut driver = TerminalDriver::new(24, 80, 1000);
        driver.process(b"Hello, world!");
        let frame = driver.snapshot(true, true);
        assert_eq!(frame.rows(), 24);
        assert_eq!(frame.cols(), 80);
    }

    #[test]
    fn title_tracking() {
        let mut driver = TerminalDriver::new(24, 80, 1000);
        driver.process(b"\x1b]0;My Title\x07");
        assert!(driver.take_title_dirty());
        assert_eq!(driver.title(), "My Title");
    }

    #[test]
    fn mouse_mode_tracking() {
        let mut driver = TerminalDriver::new(24, 80, 1000);
        // Enable any-event tracking
        driver.process(b"\x1b[?1003h");
        assert_eq!(driver.modes.mouse_mode, 4);
        assert_eq!(driver.modes.mouse_encoding, 0); // X10 default

        // Enable SGR encoding
        driver.process(b"\x1b[?1006h");
        assert_eq!(driver.modes.mouse_encoding, 2);

        // Mouse event should work
        let evt = driver.mouse_event(2, 35, 10, 5, false, false);
        assert!(evt.is_some());

        // Cooked mode should suppress
        let evt = driver.mouse_event(2, 35, 10, 5, true, true);
        assert!(evt.is_none());

        // Disable mouse
        driver.process(b"\x1b[?1003l");
        assert_eq!(driver.modes.mouse_mode, 0);

        // Setting a new mouse mode must not reset encoding
        driver.process(b"\x1b[?1006h");
        assert_eq!(driver.modes.mouse_encoding, 2);
        driver.process(b"\x1b[?1000h");
        assert_eq!(driver.modes.mouse_encoding, 2);
    }

    #[test]
    fn resize() {
        let mut driver = TerminalDriver::new(24, 80, 1000);
        driver.resize(40, 120);
        assert_eq!(driver.size(), (40, 120));
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    #[test]
    fn scrollback_works() {
        let mut driver = TerminalDriver::new(5, 80, 1000);
        // Write 10 lines (more than viewport of 5)
        for i in 0..10 {
            driver.process(format!("line {i}\r\n").as_bytes());
        }
        let snap = driver.snapshot(true, true);
        assert!(
            snap.scrollback_lines() > 0,
            "should have scrollback lines, got {}",
            snap.scrollback_lines()
        );

        // Scrollback frame at offset 1 should show different content than viewport
        let scroll1 = driver.scrollback_frame(1);
        assert_ne!(
            snap.cells(),
            scroll1.cells(),
            "scrollback should differ from viewport"
        );

        // Scrollback frame at offset 0 should match viewport
        let scroll0 = driver.scrollback_frame(0);
        // Not necessarily identical to snapshot (cursor/mode differ) but cells should match
        assert_eq!(
            snap.cells(),
            scroll0.cells(),
            "offset 0 scrollback should match viewport cells"
        );
    }

    #[test]
    fn process_produces_nonempty_snapshot() {
        let mut driver = TerminalDriver::new(24, 80, 1000);
        driver.process(b"Hello, world!\r\n");
        let frame = driver.snapshot(true, true);
        // Check that the first row has content
        let cells = frame.cells();
        // First cell should have 'H'
        let f1 = cells[1]; // flags byte 1
        let content_len = (f1 >> 3) & 7;
        assert!(
            content_len > 0,
            "first cell should have content, got len={content_len}, f1={f1:#010b}"
        );
        assert_eq!(cells[8], b'H', "first cell content should be 'H'");
    }

    #[test]
    fn welcome_emoji_width() {
        let mut welcome: Vec<u8> = Vec::new();
        welcome.extend_from_slice("╚═════╝ ╚══════╝╚═╝   ╚═╝ https://blit.sh\r\n".as_bytes());
        welcome.extend_from_slice("          with \u{2764}\u{FE0F} from https://indent.com".as_bytes());

        let mut driver = TerminalDriver::new(24, 80, 1000);
        driver.process(&welcome);
        let frame = driver.snapshot(true, true);

        fn last_content_col(frame: &blit_remote::FrameState, row: u16, cols: u16) -> u16 {
            let mut last = 0;
            for col in 0..cols {
                let content = frame.cell_content(row, col);
                if content.trim() != "" {
                    last = col;
                }
            }
            last
        }

        let cols = frame.cols();
        let line6_last = last_content_col(&frame, 0, cols);
        let line7_last = last_content_col(&frame, 1, cols);

        assert_eq!(
            line6_last, line7_last,
            "last content column should match: line6={line6_last}, line7={line7_last}"
        );

        let heart_col = 15usize;
        let f1 = frame.cells()[(1 * cols as usize + heart_col) * CELL_SIZE + 1];
        assert!(
            f1 & (1 << 1) != 0,
            "heart cell at col {heart_col} should have WIDE_CHAR flag, f1={f1:#010b}"
        );
    }
}
