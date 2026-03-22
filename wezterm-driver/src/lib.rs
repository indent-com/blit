use blit_remote::FrameState;
use std::cmp::min;
use std::io;
use std::sync::Arc;
use wezterm_surface::CursorVisibility;
use wezterm_term::color::{ColorAttribute, ColorPalette, RgbColor};
use wezterm_term::{Intensity, Terminal, TerminalConfiguration, TerminalSize, Underline};

const CELL_SIZE: usize = blit_remote::CELL_SIZE;
const SEARCH_TITLE_BASE: u32 = 1400;
const SEARCH_TITLE_PREFIX_BONUS: u32 = 240;
const SEARCH_TITLE_MATCH_BONUS: u32 = 120;
const SEARCH_VISIBLE_BASE: u32 = 360;
const SEARCH_VISIBLE_PREFIX_BONUS: u32 = 72;
const SEARCH_VISIBLE_LINE_BONUS: u32 = 32;
const SEARCH_SCROLLBACK_BASE: u32 = 120;
const SEARCH_SCROLLBACK_PREFIX_BONUS: u32 = 24;
const SEARCH_SCROLLBACK_LINE_BONUS: u32 = 12;
const SEARCH_CONTEXT_BEFORE: usize = 28;
const SEARCH_CONTEXT_AFTER: usize = 52;

pub const SEARCH_MATCH_TITLE: u8 = 1 << 0;
pub const SEARCH_MATCH_VISIBLE: u8 = 1 << 1;
pub const SEARCH_MATCH_SCROLLBACK: u8 = 1 << 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SearchSource {
    Title = 0,
    Visible = 1,
    Scrollback = 2,
}

impl SearchSource {
    fn mask(self) -> u8 {
        match self {
            Self::Title => SEARCH_MATCH_TITLE,
            Self::Visible => SEARCH_MATCH_VISIBLE,
            Self::Scrollback => SEARCH_MATCH_SCROLLBACK,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    pub score: u32,
    pub primary_source: SearchSource,
    pub matched_sources: u8,
    pub context: String,
    pub scroll_offset: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SearchCandidate {
    score: u32,
    source: SearchSource,
    context: String,
    scroll_offset: Option<usize>,
}

#[derive(Debug)]
struct DriverConfig {
    scrollback: usize,
}

impl TerminalConfiguration for DriverConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EscapeParseState {
    Ground,
    Escape,
    Csi(CsiState),
}

impl Default for EscapeParseState {
    fn default() -> Self {
        Self::Ground
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CsiState {
    private: bool,
    bang: bool,
    params: [u16; 8],
    len: usize,
    current: Option<u16>,
}

impl CsiState {
    fn push_current(&mut self) {
        if let Some(current) = self.current.take() {
            if self.len < self.params.len() {
                self.params[self.len] = current;
                self.len += 1;
            }
        }
    }

    fn params(&self) -> &[u16] {
        &self.params[..self.len]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ModeTracker {
    app_cursor: bool,
    app_keypad: bool,
    alt_screen: bool,
    mouse_mode: u16,
    mouse_encoding: u16,
    parse_state: EscapeParseState,
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
    }

    fn soft_reset(&mut self) {
        self.app_cursor = false;
        self.app_keypad = false;
    }

    fn handle_csi(&mut self, csi: CsiState, final_byte: u8) {
        if csi.bang && final_byte == b'p' {
            self.soft_reset();
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
        mode
    }
}

pub struct TerminalDriver {
    terminal: Terminal,
    modes: ModeTracker,
    title: String,
    title_dirty: bool,
    saw_explicit_title: bool,
}

impl TerminalDriver {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        let config = Arc::new(DriverConfig { scrollback });
        let terminal = Terminal::new(
            TerminalSize {
                rows: rows as usize,
                cols: cols as usize,
                ..TerminalSize::default()
            },
            config,
            "blit-server",
            env!("CARGO_PKG_VERSION"),
            Box::new(io::sink()),
        );

        Self {
            terminal,
            modes: ModeTracker::default(),
            title: String::new(),
            title_dirty: false,
            saw_explicit_title: false,
        }
    }

    pub fn process(&mut self, data: &[u8]) {
        self.modes.process(data);
        // Wezterm can panic on pathological input (e.g. divide-by-zero in the
        // image handler for random binary data).  Catch and discard.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.terminal.advance_bytes(data);
        }));
        self.refresh_title();
    }

    pub fn size(&self) -> (u16, u16) {
        let size = self.terminal.get_size();
        (size.rows as u16, size.cols as u16)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let current_size = self.terminal.get_size();
        self.terminal.resize(TerminalSize {
            rows: rows as usize,
            cols: cols as usize,
            ..current_size
        });
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn search_result(&self, query: &str) -> Option<SearchResult> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return None;
        }

        let size = self.terminal.get_size();
        let screen = self.terminal.screen();
        let total_rows = screen.scrollback_rows();
        let visible_start = total_rows.saturating_sub(size.rows);
        let visible_lines = screen.lines_in_phys_range(visible_start..total_rows);
        let scrollback_lines = screen.lines_in_phys_range(0..visible_start);

        let title = search_text_candidate(
            &self.title,
            &query,
            SEARCH_TITLE_BASE,
            SEARCH_TITLE_PREFIX_BONUS,
            SEARCH_TITLE_MATCH_BONUS,
            SearchSource::Title,
        );
        let visible = search_lines_candidate(
            &visible_lines,
            size.cols,
            &query,
            SEARCH_VISIBLE_BASE,
            SEARCH_VISIBLE_PREFIX_BONUS,
            SEARCH_VISIBLE_LINE_BONUS,
            SearchSource::Visible,
            visible_start,
            visible_start,
            size.rows,
        );
        let scrollback = search_lines_candidate(
            &scrollback_lines,
            size.cols,
            &query,
            SEARCH_SCROLLBACK_BASE,
            SEARCH_SCROLLBACK_PREFIX_BONUS,
            SEARCH_SCROLLBACK_LINE_BONUS,
            SearchSource::Scrollback,
            0,
            visible_start,
            size.rows,
        );

        let mut total_score = 0u32;
        let mut matched_sources = 0u8;
        let mut primary: Option<SearchCandidate> = None;
        let mut jump: Option<SearchCandidate> = None;
        for candidate in [title, visible, scrollback].into_iter().flatten() {
            total_score = total_score.saturating_add(candidate.score);
            matched_sources |= candidate.source.mask();
            if candidate.scroll_offset.is_some()
                && jump
                    .as_ref()
                    .is_none_or(|best| search_candidate_better(&candidate, best))
            {
                jump = Some(candidate.clone());
            }
            if primary
                .as_ref()
                .is_none_or(|best| search_candidate_better(&candidate, best))
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
            scroll_offset: jump.and_then(|candidate| candidate.scroll_offset),
        })
    }

    pub fn search_rank(&self, query: &str) -> u32 {
        self.search_result(query).map(|result| result.score).unwrap_or(0)
    }

    pub fn take_title_dirty(&mut self) -> bool {
        std::mem::take(&mut self.title_dirty)
    }

    pub fn cursor_position(&self) -> (u16, u16) {
        let size = self.terminal.get_size();
        let cursor = self.terminal.cursor_pos();
        let row = clamp_u16(cursor.y, size.rows);
        let col = clamp_u16(cursor.x as i64, size.cols);
        (row, col)
    }

    pub fn snapshot(&mut self, echo: bool, icanon: bool) -> FrameState {
        let size = self.terminal.get_size();
        let screen = self.terminal.screen();
        let start = screen.scrollback_rows().saturating_sub(size.rows);
        let lines = screen.lines_in_phys_range(start..start + size.rows);
        let cursor = self.terminal.cursor_pos();
        let mode = self.pack_mode(echo, icanon);
        frame_from_lines(
            &lines,
            size.rows,
            size.cols,
            clamp_u16(cursor.y, size.rows),
            clamp_u16(cursor.x as i64, size.cols),
            mode,
            &self.title,
        )
    }

    pub fn scrollback_frame(&mut self, offset: usize) -> FrameState {
        let size = self.terminal.get_size();
        let screen = self.terminal.screen();
        let total_rows = screen.scrollback_rows();
        let visible_start = total_rows.saturating_sub(size.rows);
        let start = visible_start.saturating_sub(offset);
        let end = min(start + size.rows, total_rows);
        let lines = screen.lines_in_phys_range(start..end);
        frame_from_lines(&lines, size.rows, size.cols, 0, 0, 0, &self.title)
    }

    fn refresh_title(&mut self) {
        let raw = self.terminal.get_title();
        if raw != "wezterm" || self.saw_explicit_title {
            self.saw_explicit_title = true;
        }
        let title = if self.saw_explicit_title {
            raw.to_owned()
        } else {
            String::new()
        };
        if title != self.title {
            self.title = title;
            self.title_dirty = true;
        }
    }

    fn pack_mode(&self, echo: bool, icanon: bool) -> u16 {
        self.modes.pack(
            self.terminal.cursor_pos().visibility == CursorVisibility::Visible,
            self.terminal.bracketed_paste_enabled(),
            echo,
            icanon,
        )
    }
}

fn clamp_u16(value: i64, upper: usize) -> u16 {
    let upper = upper.saturating_sub(1) as i64;
    value.clamp(0, upper.max(0)) as u16
}

fn search_candidate_better(candidate: &SearchCandidate, best: &SearchCandidate) -> bool {
    candidate.score > best.score
        || (candidate.score == best.score && (candidate.source as u8) < (best.source as u8))
}

fn search_text_candidate(
    haystack: &str,
    query: &str,
    base: u32,
    prefix_bonus: u32,
    match_bonus: u32,
    source: SearchSource,
) -> Option<SearchCandidate> {
    if haystack.is_empty() || query.is_empty() {
        return None;
    }
    let lower = haystack.to_lowercase();
    let score = score_lower_text_match(
        &lower,
        query,
        base,
        prefix_bonus,
        match_bonus,
    );
    if score == 0 {
        None
    } else {
        Some(SearchCandidate {
            score,
            source,
            context: search_excerpt(haystack, &lower, query),
            scroll_offset: None,
        })
    }
}

fn search_scroll_offset_for_line(matched_row: usize, visible_start: usize, rows: usize) -> usize {
    let lead_rows = rows / 3;
    let target_start = matched_row.saturating_sub(lead_rows).min(visible_start);
    visible_start.saturating_sub(target_start)
}

fn search_lines_candidate(
    lines: &[wezterm_term::Line],
    cols: usize,
    query: &str,
    base: u32,
    prefix_bonus: u32,
    line_bonus: u32,
    source: SearchSource,
    first_row: usize,
    visible_start: usize,
    rows: usize,
) -> Option<SearchCandidate> {
    if lines.is_empty() || query.is_empty() {
        return None;
    }

    let mut best = 0u32;
    let mut best_row = first_row;
    let mut matched_lines = 0u32;
    let mut best_text = String::new();
    let mut best_lower = String::new();
    for (idx, line) in lines.iter().enumerate() {
        let line_text = searchable_line_text(line, cols);
        if line_text.is_empty() {
            continue;
        }
        let lower = line_text.to_lowercase();
        let line_score = score_lower_text_match(&lower, query, base, prefix_bonus, 0);
        if line_score == 0 {
            continue;
        }
        if line_score > best {
            best = line_score;
            best_row = first_row.saturating_add(idx);
            best_text = line_text;
            best_lower = lower;
        }
        matched_lines = matched_lines.saturating_add(1);
        if matched_lines >= 8 {
            break;
        }
    }

    if matched_lines == 0 {
        None
    } else {
        Some(SearchCandidate {
            score: best.saturating_add(matched_lines.saturating_sub(1) * line_bonus),
            source,
            context: search_excerpt(&best_text, &best_lower, query),
            scroll_offset: Some(search_scroll_offset_for_line(best_row, visible_start, rows)),
        })
    }
}

fn score_lower_text_match(
    haystack: &str,
    query: &str,
    base: u32,
    prefix_bonus: u32,
    match_bonus: u32,
) -> u32 {
    let mut matches = 0u32;
    let mut first_pos = None;
    let mut search_from = 0usize;
    while let Some(pos) = haystack[search_from..].find(query) {
        let absolute = search_from + pos;
        matches = matches.saturating_add(1);
        first_pos.get_or_insert(absolute);
        search_from = absolute.saturating_add(query.len());
        if matches >= 8 {
            break;
        }
    }
    if matches == 0 {
        return 0;
    }

    let mut score = base.saturating_add(matches.saturating_sub(1) * match_bonus);
    if let Some(pos) = first_pos {
        if pos == 0 {
            score = score.saturating_add(prefix_bonus);
        } else if is_search_boundary(haystack.as_bytes()[pos - 1]) {
            score = score.saturating_add(prefix_bonus / 2);
        }
        score = score.saturating_add(base.saturating_sub((pos as u32).min(base)) / 8);
    }
    score
}

fn is_search_boundary(byte: u8) -> bool {
    !byte.is_ascii_alphanumeric()
}

fn search_excerpt(text: &str, lower: &str, query: &str) -> String {
    let compact = compact_search_text(text);
    if compact.is_empty() {
        return compact;
    }

    if !(compact.is_ascii() && lower.is_ascii() && query.is_ascii()) {
        return compact;
    }

    let compact_lower = compact.to_ascii_lowercase();
    let Some(pos) = compact_lower.find(query) else {
        return compact;
    };

    let start = pos.saturating_sub(SEARCH_CONTEXT_BEFORE);
    let end = (pos + query.len() + SEARCH_CONTEXT_AFTER).min(compact.len());
    let mut excerpt = String::new();
    if start > 0 {
        excerpt.push_str("...");
    }
    excerpt.push_str(compact[start..end].trim());
    if end < compact.len() {
        excerpt.push_str("...");
    }
    excerpt
}

fn compact_search_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

fn searchable_line_text(line: &wezterm_term::Line, cols: usize) -> String {
    let mut text = String::new();
    let mut cursor = 0usize;
    for cell in line.visible_cells() {
        let cell_col = cell.cell_index();
        if cell_col >= cols {
            break;
        }
        if cell_col > cursor {
            text.extend(std::iter::repeat(' ').take(cell_col - cursor));
        }
        text.push_str(cell.str());
        cursor = (cell_col + cell.width()).min(cols);
    }
    while text.ends_with(' ') {
        text.pop();
    }
    compact_search_text(&text)
}

fn frame_from_lines(
    lines: &[wezterm_term::Line],
    rows: usize,
    cols: usize,
    cursor_row: u16,
    cursor_col: u16,
    mode: u16,
    title: &str,
) -> FrameState {
    let mut cells = vec![0u8; rows * cols * CELL_SIZE];
    let mut overflow = std::collections::BTreeMap::new();
    for row in 0..rows {
        let Some(line) = lines.get(row) else {
            break;
        };
        for cell in line.visible_cells() {
            let col = cell.cell_index();
            if col >= cols {
                continue;
            }
            let flat = row * cols + col;
            encode_visible_cell(
                &cell,
                &mut cells[flat * CELL_SIZE..][..CELL_SIZE],
                flat,
                &mut overflow,
            );
            for cont in 1..cell.width() {
                let cont_col = col + cont;
                if cont_col >= cols {
                    break;
                }
                let buf = &mut cells[(row * cols + cont_col) * CELL_SIZE..][..CELL_SIZE];
                buf.fill(0);
                buf[1] |= 1 << 2;
            }
        }
    }

    let mut frame = FrameState::from_parts(
        rows as u16,
        cols as u16,
        cursor_row,
        cursor_col,
        mode,
        title.to_owned(),
        cells,
    );
    *frame.overflow_mut() = overflow;
    frame
}

fn encode_visible_cell(
    cell: &wezterm_term::CellRef<'_>,
    buf: &mut [u8],
    flat_index: usize,
    overflow: &mut std::collections::BTreeMap<usize, String>,
) {
    buf.fill(0);
    let attrs = cell.attrs();

    let mut f0 = 0u8;
    encode_color(attrs.foreground(), &mut f0, &mut buf[2..5], false);
    encode_color(attrs.background(), &mut f0, &mut buf[5..8], true);
    match attrs.intensity() {
        Intensity::Bold => f0 |= 1 << 4,
        Intensity::Half => f0 |= 1 << 5,
        Intensity::Normal => {}
    }
    if attrs.italic() {
        f0 |= 1 << 6;
    }
    if attrs.underline() != Underline::None {
        f0 |= 1 << 7;
    }
    buf[0] = f0;

    let mut f1 = 0u8;
    if attrs.reverse() {
        f1 |= 1;
    }
    if cell.width() > 1 {
        f1 |= 1 << 1;
    }
    let s = cell.str();
    let bytes = s.as_bytes();
    if bytes.len() <= 4 {
        // Fits inline.
        f1 |= (bytes.len() as u8) << 3;
        buf[8..8 + bytes.len()].copy_from_slice(bytes);
    } else {
        // Overflow: store FNV-1a hash in bytes 8-11 for diff detection,
        // actual string in the overflow table.
        f1 |= 7 << 3; // content_len = 7 (sentinel)
        let hash = fnv1a_32(bytes);
        buf[8..12].copy_from_slice(&hash.to_le_bytes());
        overflow.insert(flat_index, s.to_owned());
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

fn encode_color(color: ColorAttribute, flags: &mut u8, dst: &mut [u8], is_bg: bool) {
    let shift = if is_bg { 2 } else { 0 };
    match color {
        ColorAttribute::Default => {}
        ColorAttribute::PaletteIndex(idx) => {
            *flags |= 1 << shift;
            dst[0] = idx;
        }
        ColorAttribute::TrueColorWithPaletteFallback(color, _)
        | ColorAttribute::TrueColorWithDefaultFallback(color) => {
            let (r, g, b) = RgbColor::from(color).to_tuple_rgb8();
            *flags |= 2 << shift;
            dst[0] = r;
            dst[1] = g;
            dst[2] = b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_is_visible_and_idle() {
        let mut driver = TerminalDriver::new(24, 80, 1000);
        let frame = driver.snapshot(false, false);
        assert_eq!(frame.mode() & 1, 1);
        assert_eq!(frame.mode() & (1 << 1), 0);
        assert_eq!((frame.mode() >> 4) & 7, 0);
    }

    #[test]
    fn detects_title_and_modes() {
        let mut driver = TerminalDriver::new(24, 80, 1000);
        driver.process(b"\x1b]2;hello\x07\x1b[?1h\x1b[?2004h\x1b[?1002h\x1b[?1006h");
        assert!(driver.take_title_dirty());
        assert_eq!(driver.title(), "hello");
        let frame = driver.snapshot(false, false);
        assert_ne!(frame.mode() & (1 << 1), 0);
        assert_ne!(frame.mode() & (1 << 3), 0);
        assert_eq!((frame.mode() >> 4) & 7, 3);
        assert_eq!((frame.mode() >> 7) & 3, 2);
    }

    #[test]
    fn tracks_chunked_keypad_and_legacy_mouse_sequences() {
        let mut driver = TerminalDriver::new(24, 80, 1000);
        driver.process(b"\x1b");
        driver.process(b"=");
        driver.process(b"\x1b[?9");
        driver.process(b"h\x1b[?101");
        driver.process(b"6h");

        let frame = driver.snapshot(false, false);
        assert_ne!(frame.mode() & (1 << 2), 0);
        assert_eq!((frame.mode() >> 4) & 7, 1);
        assert_eq!((frame.mode() >> 7) & 3, 3);
    }

    #[test]
    fn higher_mouse_modes_override_legacy_mode_until_cleared() {
        let mut driver = TerminalDriver::new(24, 80, 1000);
        driver.process(b"\x1b[?9h\x1b[?1000h\x1b[?1006h");
        let frame = driver.snapshot(false, false);
        assert_eq!((frame.mode() >> 4) & 7, 2);
        assert_eq!((frame.mode() >> 7) & 3, 2);

        driver.process(b"\x1b[?1000l");
        let frame = driver.snapshot(false, false);
        assert_eq!((frame.mode() >> 4) & 7, 0);
        assert_eq!((frame.mode() >> 7) & 3, 2);
    }

    #[test]
    fn resets_manual_modes_on_soft_and_full_reset() {
        let mut driver = TerminalDriver::new(24, 80, 1000);
        driver.process(b"\x1b=\x1b[?1h\x1b[?1002h\x1b[?1016h");

        driver.process(b"\x1b[!p");
        let frame = driver.snapshot(false, false);
        assert_eq!(frame.mode() & ((1 << 1) | (1 << 2)), 0);
        assert_eq!((frame.mode() >> 4) & 7, 3);
        assert_eq!((frame.mode() >> 7) & 3, 3);

        driver.process(b"\x1bc");
        let frame = driver.snapshot(false, false);
        assert_eq!(frame.mode() & ((1 << 1) | (1 << 2)), 0);
        assert_eq!((frame.mode() >> 4) & 7, 0);
        assert_eq!((frame.mode() >> 7) & 3, 0);
    }

    #[test]
    fn search_uses_plain_substring_queries() {
        let mut driver = TerminalDriver::new(4, 40, 1000);
        driver.process(b"\x1b]2;error log\x07");

        assert!(driver.search_rank("ror l") > 0);
        assert_eq!(driver.search_rank("log err"), 0);
    }

    #[test]
    fn search_weights_title_visible_and_scrollback() {
        let mut title = TerminalDriver::new(2, 40, 1000);
        title.process(b"\x1b]2;needle title\x07");

        let mut visible = TerminalDriver::new(2, 40, 1000);
        visible.process(b"plain\r\nneedle visible");

        let mut scrollback = TerminalDriver::new(2, 40, 1000);
        scrollback.process(b"needle scrollback\r\nplain\r\nbottom");

        let title_score = title.search_rank("needle");
        let visible_score = visible.search_rank("needle");
        let scrollback_score = scrollback.search_rank("needle");

        assert!(title_score > visible_score);
        assert!(visible_score > scrollback_score);
        assert!(scrollback_score > 0);
    }

    #[test]
    fn search_result_reports_primary_source_and_context() {
        let mut driver = TerminalDriver::new(2, 40, 1000);
        driver.process(b"\x1b]2;build log\x07");
        driver.process(b"plain\r\nneedle in terminal output");

        let result = driver.search_result("needle").expect("search result");
        assert_eq!(result.primary_source, SearchSource::Visible);
        assert_eq!(result.matched_sources, SEARCH_MATCH_VISIBLE);
        assert_eq!(result.scroll_offset, Some(0));
        assert!(result.context.contains("needle in terminal output"));
    }

    #[test]
    fn search_result_tracks_multiple_matching_sources() {
        let mut driver = TerminalDriver::new(2, 40, 1000);
        driver.process(b"\x1b]2;needle title\x07");
        driver.process(b"needle in visible\r\nbottom");

        let result = driver.search_result("needle").expect("search result");
        assert_eq!(result.primary_source, SearchSource::Title);
        assert_eq!(
            result.matched_sources,
            SEARCH_MATCH_TITLE | SEARCH_MATCH_VISIBLE
        );
        assert_eq!(result.scroll_offset, Some(0));
    }

    #[test]
    fn search_result_reports_scrollback_jump_offset() {
        let mut driver = TerminalDriver::new(3, 40, 1000);
        driver.process(b"top\r\nneedle in history\r\nmiddle\r\nlower\r\nbottom");

        let result = driver.search_result("needle").expect("search result");
        assert_eq!(result.primary_source, SearchSource::Scrollback);
        assert_eq!(result.scroll_offset, Some(2));
    }

    use blit_remote::build_update_msg;

    /// All emojis from emojiblast plus extra stress cases.  The test verifies
    /// every one round-trips through encode→diff→apply, and that the ones
    /// used by emojiblast are all 2-wide.
    const EMOJIBLAST_EMOJIS: &[&str] = &[
        "😀", "😂", "🥹", "😎", "🤩", "🥳", "😈", "👻", "💀", "👽",
        "🤖", "🎃", "🔥", "🌈", "🌊", "🍕", "🍔", "🌮", "🍣", "🎸",
        "🎮", "🚀", "🛸", "🏄", "🎯", "🧊", "💎", "🦊", "🐙", "🦑",
        "🦄", "🐉", "🦖", "🦕", "🐢", "🐬", "🦈", "🐝", "🦋", "🌻",
        "🌵", "🍄", "🎄", "🎪", "🎠", "🗿", "🏰", "🧲", "🔮", "🧿",
        "🎲", "🪁", "🛹", "🪂", "🏹", "🧩", "🪄", "🫧", "🪩", "🫠",
    ];
    /// Extra emoji that wezterm renders as 1-wide (VS16 sequences) or are
    /// multi-codepoint ZWJ sequences.  They still must encode and round-trip
    /// correctly, but emojiblast doesn't use them.
    const EXTRA_EMOJIS: &[&str] = &[
        "⚡", "🏔️", "⛩️", "♟️", "⚔️",
        "👨\u{200d}👩\u{200d}👧\u{200d}👦",
        "🏳️\u{200d}🌈",
        "👩🏽\u{200d}🚀",
    ];

    #[test]
    fn all_emojis_encode_and_round_trip() {
        let all: Vec<&str> = EMOJIBLAST_EMOJIS
            .iter()
            .chain(EXTRA_EMOJIS.iter())
            .copied()
            .collect();
        let mut narrow = Vec::new();
        for &emoji in &all {
            let mut driver = TerminalDriver::new(2, 20, 100);
            driver.process(format!("\x1b[H{}", emoji).as_bytes());

            let frame = driver.snapshot(false, false);

            // Check the emoji occupies 2 columns.
            let f1_0 = frame.cells()[1];
            let wide = f1_0 & 2 != 0;
            if !wide {
                narrow.push(emoji);
            }

            // The emoji should produce content in the first cell
            let content = frame.cell_content(0, 0);
            assert!(
                !content.is_empty() && content != " ",
                "emoji {:?} ({}B) produced no content in cell (0,0): got {:?}",
                emoji,
                emoji.len(),
                content,
            );

            // Build update msg and apply — must round-trip
            let msg = build_update_msg(1, &frame, &FrameState::default()).unwrap();
            let mut term = blit_remote::TerminalState::new(2, 20);
            let blit_remote::ServerMsg::Update { payload, .. } =
                blit_remote::parse_server_msg(&msg).unwrap()
            else {
                panic!("expected update for emoji {:?}", emoji);
            };
            assert!(
                term.feed_compressed(payload),
                "feed_compressed failed for emoji {:?}",
                emoji,
            );

            // Verify the client-side content matches
            let client_content = term.frame().cell_content(0, 0);
            assert_eq!(
                content, client_content,
                "round-trip mismatch for emoji {:?}: server={:?} client={:?}",
                emoji, content, client_content,
            );
        }
        // All emojiblast emojis must be 2-wide.
        let emojiblast_narrow: Vec<_> = narrow
            .iter()
            .filter(|e| EMOJIBLAST_EMOJIS.contains(e))
            .collect();
        assert!(
            emojiblast_narrow.is_empty(),
            "emojiblast emojis not 2-wide in wezterm: {:?}",
            emojiblast_narrow,
        );
    }

    // ── clamp_u16 ────────────────────────────────────────────────────────

    #[test]
    fn clamp_u16_in_range() {
        assert_eq!(clamp_u16(5, 10), 5);
        assert_eq!(clamp_u16(0, 10), 0);
        assert_eq!(clamp_u16(9, 10), 9);
    }

    #[test]
    fn clamp_u16_below_zero() {
        assert_eq!(clamp_u16(-1, 10), 0);
        assert_eq!(clamp_u16(-100, 10), 0);
    }

    #[test]
    fn clamp_u16_above_upper() {
        assert_eq!(clamp_u16(10, 10), 9);
        assert_eq!(clamp_u16(1000, 10), 9);
    }

    #[test]
    fn clamp_u16_upper_zero() {
        // upper=0 → saturating_sub(1) = 0, max(0) = 0
        assert_eq!(clamp_u16(5, 0), 0);
        assert_eq!(clamp_u16(-1, 0), 0);
    }

    // ── fnv1a_32 ────────────────────────────────────────────────────────

    #[test]
    fn fnv1a_32_empty() {
        // FNV-1a offset basis
        assert_eq!(fnv1a_32(b""), 0x811c_9dc5);
    }

    #[test]
    fn fnv1a_32_known_values() {
        // Known FNV-1a 32-bit values for short ASCII strings
        assert_eq!(fnv1a_32(b"a"), 0xe40c292c);
        assert_eq!(fnv1a_32(b"foobar"), 0xbf9cf968);
    }

    #[test]
    fn fnv1a_32_different_inputs_differ() {
        assert_ne!(fnv1a_32(b"hello"), fnv1a_32(b"world"));
    }

    // ── score_lower_text_match ──────────────────────────────────────────

    #[test]
    fn score_lower_exact_prefix() {
        let score = score_lower_text_match("hello world", "hello", 100, 50, 10);
        // Should include base + prefix_bonus
        assert!(score >= 150);
    }

    #[test]
    fn score_lower_middle_match() {
        let score = score_lower_text_match("say hello world", "hello", 100, 50, 10);
        // Should match but no full prefix bonus
        assert!(score > 0);
        assert!(score < score_lower_text_match("hello world", "hello", 100, 50, 10));
    }

    #[test]
    fn score_lower_no_match() {
        assert_eq!(score_lower_text_match("hello world", "xyz", 100, 50, 10), 0);
    }

    #[test]
    fn score_lower_multiple_matches() {
        let single = score_lower_text_match("foo bar", "foo", 100, 50, 10);
        let multi = score_lower_text_match("foo bar foo baz foo", "foo", 100, 50, 10);
        assert!(multi > single);
    }

    #[test]
    fn score_lower_boundary_bonus() {
        // Match after a space (boundary char) gets half prefix bonus
        let boundary = score_lower_text_match("x hello", "hello", 100, 50, 10);
        let non_boundary = score_lower_text_match("xhello", "hello", 100, 50, 10);
        assert!(boundary > non_boundary);
    }

    // ── is_search_boundary ──────────────────────────────────────────────

    #[test]
    fn is_search_boundary_space() {
        assert!(is_search_boundary(b' '));
    }

    #[test]
    fn is_search_boundary_punctuation() {
        assert!(is_search_boundary(b'/'));
        assert!(is_search_boundary(b'-'));
        assert!(is_search_boundary(b'.'));
        assert!(is_search_boundary(b':'));
    }

    #[test]
    fn is_search_boundary_alphanumeric() {
        assert!(!is_search_boundary(b'a'));
        assert!(!is_search_boundary(b'Z'));
        assert!(!is_search_boundary(b'0'));
        assert!(!is_search_boundary(b'9'));
    }

    // ── compact_search_text ─────────────────────────────────────────────

    #[test]
    fn compact_search_text_normalizes_whitespace() {
        assert_eq!(compact_search_text("hello   world"), "hello world");
        assert_eq!(compact_search_text("  leading"), "leading");
        assert_eq!(compact_search_text("trailing  "), "trailing");
        assert_eq!(compact_search_text("  both  "), "both");
    }

    #[test]
    fn compact_search_text_tabs_and_newlines() {
        assert_eq!(compact_search_text("a\tb\nc"), "a b c");
    }

    #[test]
    fn compact_search_text_empty() {
        assert_eq!(compact_search_text(""), "");
        assert_eq!(compact_search_text("   "), "");
    }

    #[test]
    fn compact_search_text_no_change() {
        assert_eq!(compact_search_text("already clean"), "already clean");
    }

    // ── search_excerpt ──────────────────────────────────────────────────

    #[test]
    fn search_excerpt_short_text() {
        let text = "hello world";
        let lower = "hello world";
        let result = search_excerpt(text, lower, "hello");
        assert!(result.contains("hello"));
        // Short text, no ellipsis at start
        assert!(!result.starts_with("..."));
    }

    #[test]
    fn search_excerpt_long_text_with_ellipsis() {
        let text = "A".repeat(100) + "needle" + &"B".repeat(100);
        let lower = text.to_lowercase();
        let result = search_excerpt(&text, &lower, "needle");
        assert!(result.contains("needle"));
        assert!(result.starts_with("..."));
        assert!(result.ends_with("..."));
    }

    #[test]
    fn search_excerpt_no_match_returns_compact() {
        let text = "hello world";
        let lower = "hello world";
        let result = search_excerpt(text, lower, "xyz");
        assert_eq!(result, "hello world");
    }

    // ── encode_color ────────────────────────────────────────────────────

    #[test]
    fn encode_color_default() {
        let mut flags = 0u8;
        let mut dst = [0u8; 3];
        encode_color(ColorAttribute::Default, &mut flags, &mut dst, false);
        assert_eq!(flags, 0);
        assert_eq!(dst, [0, 0, 0]);
    }

    #[test]
    fn encode_color_palette_index_fg() {
        let mut flags = 0u8;
        let mut dst = [0u8; 3];
        encode_color(ColorAttribute::PaletteIndex(42), &mut flags, &mut dst, false);
        assert_eq!(flags & 0x03, 1); // 1 << 0
        assert_eq!(dst[0], 42);
    }

    #[test]
    fn encode_color_palette_index_bg() {
        let mut flags = 0u8;
        let mut dst = [0u8; 3];
        encode_color(ColorAttribute::PaletteIndex(7), &mut flags, &mut dst, true);
        assert_eq!(flags & 0x0c, 4); // 1 << 2
        assert_eq!(dst[0], 7);
    }

    #[test]
    fn encode_color_true_color_fg() {
        let mut flags = 0u8;
        let mut dst = [0u8; 3];
        let color = wezterm_term::color::SrgbaTuple(0.5, 0.25, 1.0, 1.0);
        encode_color(
            ColorAttribute::TrueColorWithDefaultFallback(color),
            &mut flags,
            &mut dst,
            false,
        );
        assert_eq!(flags & 0x03, 2); // 2 << 0
        // Check that RGB bytes are populated (exact values depend on conversion)
        assert!(dst[0] > 0 || dst[1] > 0 || dst[2] > 0);
    }

    #[test]
    fn encode_color_true_color_bg() {
        let mut flags = 0u8;
        let mut dst = [0u8; 3];
        let color = wezterm_term::color::SrgbaTuple(1.0, 0.0, 0.0, 1.0);
        encode_color(
            ColorAttribute::TrueColorWithDefaultFallback(color),
            &mut flags,
            &mut dst,
            true,
        );
        assert_eq!(flags & 0x0c, 8); // 2 << 2
        assert_eq!(dst[0], 255); // red channel
    }

    // ── encode_visible_cell (via TerminalDriver round-trip) ─────────────

    #[test]
    fn encode_visible_cell_ascii_inline() {
        // Process a simple ASCII character and verify it encodes inline
        let mut driver = TerminalDriver::new(2, 10, 100);
        driver.process(b"\x1b[HA");
        let frame = driver.snapshot(false, false);
        let content = frame.cell_content(0, 0);
        assert_eq!(content, "A");
        // f1 bits 3..5 should encode length 1
        let f1 = frame.cells()[1];
        let content_len = (f1 >> 3) & 7;
        assert_eq!(content_len, 1);
    }

    #[test]
    fn encode_visible_cell_wide_char() {
        let mut driver = TerminalDriver::new(2, 10, 100);
        driver.process("全".as_bytes());
        let frame = driver.snapshot(false, false);
        let content = frame.cell_content(0, 0);
        assert_eq!(content, "全");
        // wide flag
        let f1 = frame.cells()[1];
        assert_ne!(f1 & (1 << 1), 0);
        // continuation cell
        let cont_f1 = frame.cells()[CELL_SIZE + 1];
        assert_ne!(cont_f1 & (1 << 2), 0);
    }

    #[test]
    fn survives_random_binary_torrent_and_recovers() {
        let mut driver = TerminalDriver::new(24, 80, 100);
        let mut rng = 0x12345678u64;

        // Feed ~1MB of pseudorandom binary data in 16KB chunks (simulating
        // `dd if=/dev/random bs=1M count=1`).  The terminal must not panic.
        for _ in 0..64 {
            let mut chunk = [0u8; 16384];
            for byte in chunk.iter_mut() {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                *byte = rng as u8;
            }
            driver.process(&chunk);
        }

        // Snapshot must succeed and produce a valid frame.
        let garbled = driver.snapshot(false, false);
        assert_eq!(garbled.rows(), 24);
        assert_eq!(garbled.cols(), 80);
        assert_eq!(garbled.cells().len(), 24 * 80 * CELL_SIZE);

        // Diff round-trip: build an update from the garbled frame, apply it
        // to a fresh TerminalState, verify the cells match exactly.
        let msg = build_update_msg(1, &garbled, &FrameState::default()).unwrap();
        let mut term = blit_remote::TerminalState::new(24, 80);
        let blit_remote::ServerMsg::Update { payload, .. } =
            blit_remote::parse_server_msg(&msg).unwrap()
        else {
            panic!("expected update");
        };
        assert!(term.feed_compressed(payload));
        assert_eq!(term.frame().cells(), garbled.cells());

        // Now feed a clean prompt — the terminal must recover and display it.
        driver.process(b"\x1bc");          // full reset (RIS)
        driver.process(b"\x1b[H\x1b[2J"); // cursor home + clear
        driver.process(b"$ hello");

        let clean = driver.snapshot(true, true);
        assert!(clean.cells() != garbled.cells());
        assert_eq!(clean.get_text(0, 0, 0, 6), "$ hello");
    }
}
