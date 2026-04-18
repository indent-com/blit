//! Ripgrep-compatible search over a blit server's terminals.
//!
//! Each PTY is treated as a "file". Text is fetched via `C2S_COPY_RANGE` so
//! soft-wrapped logical lines stay intact. Matching is done with the Rust
//! `regex` crate (same default engine as ripgrep); lookaround and backrefs
//! are unsupported and surface as compile-time errors.

use std::io::IsTerminal;

use blit_remote::{S2C_TEXT, ServerMsg, msg_copy_range, parse_server_msg};
use regex::{Regex, RegexBuilder};

use crate::agent::{AgentConn, PtyInfo};
use crate::transport::Transport;

// ── Options ────────────────────────────────────────────────────────────────

pub(crate) struct Opts {
    // Patterns
    pub patterns: Vec<String>,
    pub fixed_strings: bool,
    pub word_regexp: bool,
    pub line_regexp: bool,

    // Case
    pub ignore_case: bool,
    pub case_sensitive: bool,
    pub smart_case: bool,

    // Multiline
    pub multiline: bool,
    pub multiline_dotall: bool,

    // Context
    pub after: usize,
    pub before: usize,
    pub context_separator: String,
    pub no_context_separator: bool,

    // Output shaping
    pub line_number: Option<bool>,
    pub with_filename: Option<bool>,
    pub heading: Option<bool>,
    pub column: bool,
    pub count: bool,
    pub count_matches: bool,
    pub files_with_matches: bool,
    pub files_without_match: bool,
    pub only_matching: bool,
    pub max_count: Option<u64>,
    pub passthru: bool,
    pub vimgrep: bool,
    pub json: bool,
    pub pretty: bool,
    pub null: bool,
    pub color: ColorChoice,
    pub field_context_separator: String,
    pub field_match_separator: String,

    // Limiters
    pub quiet: bool,
    pub no_messages: bool,
    pub stats: bool,
    pub stop_on_nonmatch: bool,
    pub invert: bool,

    // Sorting
    pub sort: SortOrder,

    // Target selection
    pub ids: Vec<u16>,
    pub tag: Option<String>,
    pub title: Option<String>,
    pub running: bool,
    pub exited: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorChoice {
    Auto,
    Always,
    Never,
    Ansi,
}

impl ColorChoice {
    fn parse(s: &str) -> Self {
        match s {
            "always" => Self::Always,
            "never" => Self::Never,
            "ansi" => Self::Ansi,
            _ => Self::Auto,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortOrder {
    None,
    PathAsc,
    PathDesc,
}

impl Opts {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_cli(
        pattern: Option<String>,
        ids: Vec<u16>,
        regexps: Vec<String>,
        pattern_files: Vec<String>,
        fixed_strings: bool,
        word_regexp: bool,
        line_regexp: bool,
        ignore_case: bool,
        case_sensitive: bool,
        smart_case: bool,
        multiline: bool,
        multiline_dotall: bool,
        after_context: usize,
        before_context: usize,
        context: Option<usize>,
        context_separator: String,
        no_context_separator: bool,
        line_number: bool,
        no_line_number: bool,
        with_filename: bool,
        no_filename: bool,
        heading: bool,
        no_heading: bool,
        column: bool,
        count: bool,
        count_matches: bool,
        files_with_matches: bool,
        files_without_match: bool,
        only_matching: bool,
        max_count: Option<u64>,
        passthru: bool,
        vimgrep: bool,
        json: bool,
        pretty: bool,
        null: bool,
        color: String,
        field_context_separator: String,
        field_match_separator: String,
        quiet: bool,
        no_messages: bool,
        stats: bool,
        stop_on_nonmatch: bool,
        invert: bool,
        sort: Option<String>,
        sortr: Option<String>,
        tag: Option<String>,
        title: Option<String>,
        running: bool,
        exited: bool,
        _all: bool,
    ) -> Result<Self, String> {
        let mut patterns = Vec::new();
        if let Some(p) = pattern {
            patterns.push(p);
        }
        patterns.extend(regexps);

        // Read patterns from files
        for path in &pattern_files {
            let data = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
            for line in data.lines() {
                if !line.is_empty() {
                    patterns.push(line.to_string());
                }
            }
        }

        if patterns.is_empty() {
            return Err("no pattern provided (pass one positionally or with -e/-f)".into());
        }

        let (after, before) = match context {
            Some(n) => (n, n),
            None => (after_context, before_context),
        };

        let sort = match (sort.as_deref(), sortr.as_deref()) {
            (Some("path"), _) => SortOrder::PathAsc,
            (_, Some("path")) => SortOrder::PathDesc,
            _ => SortOrder::None,
        };

        let line_number = if no_line_number {
            Some(false)
        } else if line_number {
            Some(true)
        } else {
            None
        };
        let with_filename = if no_filename {
            Some(false)
        } else if with_filename {
            Some(true)
        } else {
            None
        };
        let heading = if no_heading {
            Some(false)
        } else if heading {
            Some(true)
        } else {
            None
        };

        Ok(Self {
            patterns,
            fixed_strings,
            word_regexp,
            line_regexp,
            ignore_case,
            case_sensitive,
            smart_case,
            multiline,
            multiline_dotall,
            after,
            before,
            context_separator,
            no_context_separator,
            line_number,
            with_filename,
            heading,
            column,
            count,
            count_matches,
            files_with_matches,
            files_without_match,
            only_matching,
            max_count,
            passthru,
            vimgrep,
            json,
            pretty,
            null,
            color: ColorChoice::parse(&color),
            field_context_separator,
            field_match_separator,
            quiet,
            no_messages,
            stats,
            stop_on_nonmatch,
            invert,
            sort,
            ids,
            tag,
            title,
            running,
            exited,
        })
    }
}

// ── Regex assembly ─────────────────────────────────────────────────────────

fn build_regex(opts: &Opts) -> Result<Regex, String> {
    let pats: Vec<String> = opts
        .patterns
        .iter()
        .map(|p| {
            let mut s = if opts.fixed_strings {
                regex::escape(p)
            } else {
                p.clone()
            };
            if opts.word_regexp {
                s = format!(r"\b(?:{s})\b");
            }
            s
        })
        .collect();

    let mut combined = if pats.len() == 1 {
        pats[0].clone()
    } else {
        format!("(?:{})", pats.join("|"))
    };
    if opts.line_regexp {
        combined = format!(r"\A(?:{combined})\z");
    }

    let ignore = if opts.case_sensitive {
        false
    } else if opts.ignore_case {
        true
    } else if opts.smart_case {
        !opts
            .patterns
            .iter()
            .any(|p| p.chars().any(|c| c.is_uppercase()))
    } else {
        false
    };

    RegexBuilder::new(&combined)
        .case_insensitive(ignore)
        .multi_line(opts.multiline)
        .dot_matches_new_line(opts.multiline_dotall)
        .build()
        .map_err(|e| format!("invalid pattern: {e}"))
}

// ── Target selection ───────────────────────────────────────────────────────

struct Target {
    id: u16,
    tag: String,
    title: String,
    running: bool,
}

fn select_targets(conn: &AgentConn, opts: &Opts) -> Result<Vec<Target>, String> {
    let mut rows: Vec<Target> = conn
        .ptys
        .iter()
        .map(|p: &PtyInfo| Target {
            id: p.id,
            tag: p.tag.clone(),
            title: conn.titles.get(&p.id).cloned().unwrap_or_default(),
            running: !conn.exited.contains_key(&p.id),
        })
        .collect();

    if !opts.ids.is_empty() {
        let by_id: std::collections::HashMap<u16, usize> =
            rows.iter().enumerate().map(|(i, r)| (r.id, i)).collect();
        let mut picked = Vec::with_capacity(opts.ids.len());
        for id in &opts.ids {
            match by_id.get(id) {
                Some(&i) => picked.push(Target {
                    id: rows[i].id,
                    tag: rows[i].tag.clone(),
                    title: rows[i].title.clone(),
                    running: rows[i].running,
                }),
                None => {
                    if opts.no_messages {
                        eprintln!("blit: pty {id} not found (skipped)");
                    } else {
                        return Err(format!("pty {id} not found"));
                    }
                }
            }
        }
        rows = picked;
    } else {
        if let Some(t) = &opts.tag {
            rows.retain(|r| r.tag.contains(t));
        }
        if let Some(t) = &opts.title {
            rows.retain(|r| r.title.contains(t));
        }
        if opts.running {
            rows.retain(|r| r.running);
        }
        if opts.exited {
            rows.retain(|r| !r.running);
        }
    }

    match opts.sort {
        SortOrder::PathAsc => rows.sort_by_key(|r| r.id),
        SortOrder::PathDesc => rows.sort_by_key(|r| std::cmp::Reverse(r.id)),
        SortOrder::None => {}
    }

    if rows.is_empty() && !opts.no_messages {
        return Err("no terminals match the given filters".into());
    }

    Ok(rows)
}

// ── Color ──────────────────────────────────────────────────────────────────

struct Paint {
    enabled: bool,
}

impl Paint {
    fn new(opts: &Opts) -> Self {
        // --pretty defaults color to `always`, but an explicit --color wins.
        let enabled = match opts.color {
            ColorChoice::Always | ColorChoice::Ansi => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => opts.pretty || std::io::stdout().is_terminal(),
        };
        Self { enabled }
    }
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn match_(&self, s: &str) -> String {
        self.wrap("1;31", s)
    }
    fn line_num(&self, s: &str) -> String {
        self.wrap("32", s)
    }
    fn path(&self, s: &str) -> String {
        self.wrap("35", s)
    }
    fn sep(&self, s: &str) -> String {
        self.wrap("36", s)
    }
}

// ── Run loop ───────────────────────────────────────────────────────────────

#[derive(Default)]
struct Stats {
    bytes: u64,
    terminals_searched: u64,
    terminals_with_matches: u64,
    total_matches: u64,
    matched_lines: u64,
}

pub(crate) async fn run(transport: Transport, opts: Opts) -> Result<i32, String> {
    // Restore default SIGPIPE so `blit terminal grep | head` exits cleanly
    // instead of panicking on broken pipe. Rust's runtime ignores SIGPIPE by
    // default, which is wrong for a grep-like tool meant to be piped.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let re = build_regex(&opts)?;

    let mut conn = AgentConn::connect(transport).await?;
    let targets = select_targets(&conn, &opts)?;

    let multi = targets.len() > 1;
    let paint = Paint::new(&opts);
    let mut stats = Stats::default();

    // Effective flags (rg-style defaults).
    let eff_heading = if opts.vimgrep || opts.json || opts.only_matching_files() {
        false
    } else {
        opts.heading.unwrap_or_else(|| {
            // Default: on when multi-PTY + TTY, or whenever --pretty is active.
            opts.pretty || (multi && std::io::stdout().is_terminal())
        })
    };
    let eff_filename = opts
        .with_filename
        .unwrap_or(multi || opts.vimgrep || opts.json);
    let eff_line_number = opts
        .line_number
        .unwrap_or(!opts.only_matching_files() && !opts.count_like());

    for (i, t) in targets.iter().enumerate() {
        let nonce = (i as u16).wrapping_add(1);
        conn.send(&msg_copy_range(
            nonce,
            t.id,
            // i32::MAX (not u32::MAX): the server casts `tail as i32`, and
            // u32::MAX wraps to -1, producing an empty range.
            i32::MAX as u32,
            0,
            0,
            u16::MAX,
            0,
        ))
        .await?;

        let text = loop {
            let data = conn.recv().await?;
            if data.is_empty() {
                continue;
            }
            if data[0] == S2C_TEXT
                && let Some(ServerMsg::Text {
                    nonce: n,
                    text,
                    pty_id,
                    ..
                }) = parse_server_msg(&data)
                && n == nonce
                && pty_id == t.id
            {
                break text.to_string();
            }
        };

        stats.bytes += text.len() as u64;
        stats.terminals_searched += 1;

        let had = scan_pty(
            &re,
            &opts,
            t,
            &text,
            &paint,
            eff_heading,
            eff_filename,
            eff_line_number,
            &mut stats,
        );
        if had {
            stats.terminals_with_matches += 1;
        }

        if opts.quiet && stats.total_matches > 0 {
            break;
        }
    }

    if opts.json {
        // Global summary event for --json.
        let summary = serde_json::json!({
            "type": "summary",
            "data": {
                "elapsed_total": { "secs": 0, "nanos": 0, "human": "0s" },
                "stats": {
                    "elapsed": { "secs": 0, "nanos": 0, "human": "0s" },
                    "searches": stats.terminals_searched,
                    "searches_with_match": stats.terminals_with_matches,
                    "bytes_searched": stats.bytes,
                    "bytes_printed": 0,
                    "matched_lines": stats.matched_lines,
                    "matches": stats.total_matches,
                }
            }
        });
        println!("{summary}");
    } else if opts.stats && !opts.quiet {
        println!();
        println!("{} matches", stats.total_matches);
        println!("{} matched lines", stats.matched_lines);
        println!(
            "{} terminals contained matches",
            stats.terminals_with_matches
        );
        println!("{} terminals searched", stats.terminals_searched);
        println!("{} bytes searched", stats.bytes);
    }

    Ok(if stats.terminals_with_matches > 0 {
        0
    } else {
        1
    })
}

impl Opts {
    fn only_matching_files(&self) -> bool {
        self.files_with_matches || self.files_without_match
    }
    fn count_like(&self) -> bool {
        self.count || self.count_matches
    }
}

// ── Per-PTY scan ───────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn scan_pty(
    re: &Regex,
    opts: &Opts,
    t: &Target,
    text: &str,
    paint: &Paint,
    heading: bool,
    filename: bool,
    line_number: bool,
    stats: &mut Stats,
) -> bool {
    let path = format!("pty:{}", t.id);
    let lines = split_lines(text);

    // ── Collect matches per line ──
    let mut per_line: Vec<LineHit> = Vec::new();
    if opts.multiline {
        // Multiline: regex may span \n; report each match against the line it
        // starts on. `-v` is not meaningful in this mode and is ignored.
        let whole = text.as_bytes();
        let matches: Vec<(usize, usize)> =
            re.find_iter(text).map(|m| (m.start(), m.end())).collect();
        if matches.is_empty() && !opts.invert {
            return render_zero_matches(opts, &path, paint);
        }
        for (s, e) in matches {
            let line_no = 1 + whole[..s].iter().filter(|&&b| b == b'\n').count();
            let line_start = whole[..s]
                .iter()
                .rposition(|&b| b == b'\n')
                .map_or(0, |p| p + 1);
            // End of the *starting* line (not of the whole match, which may
            // span newlines). Multiline matches are reported on their
            // starting line only.
            let start_line_end = whole[line_start..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|p| line_start + p)
                .unwrap_or(whole.len());
            let line_text = std::str::from_utf8(&whole[line_start..start_line_end])
                .unwrap_or("")
                .to_string();
            let rel_s = s - line_start;
            // Clamp match end to the starting line's length for highlighting.
            let rel_e = (e - line_start).min(start_line_end - line_start);
            per_line.push(LineHit {
                line_no: line_no as u64,
                text: strip_cr(&line_text),
                matches: vec![(rel_s, rel_e)],
            });
        }
    } else {
        for (idx, line) in lines.iter().enumerate() {
            let mut matches: Vec<(usize, usize)> = Vec::new();
            for m in re.find_iter(line) {
                matches.push((m.start(), m.end()));
            }
            let is_match = !matches.is_empty();
            if is_match ^ opts.invert {
                per_line.push(LineHit {
                    line_no: (idx as u64) + 1,
                    text: line.to_string(),
                    matches: if opts.invert { Vec::new() } else { matches },
                });
            }
        }
    }

    // Apply max_count (per-PTY cap on match COUNT, matching rg semantics).
    if let Some(cap) = opts.max_count {
        per_line.truncate(cap as usize);
    }

    // Update stats.
    stats.matched_lines += per_line.len() as u64;
    for h in &per_line {
        if h.matches.is_empty() {
            stats.total_matches += 1; // invert counts line as 1
        } else {
            stats.total_matches += h.matches.len() as u64;
        }
    }

    let had_match = !per_line.is_empty();

    // ── Render mode precedence ──

    if opts.quiet {
        return had_match;
    }

    if opts.json {
        render_json(opts, t, &path, &per_line, text);
        return had_match;
    }

    if opts.files_with_matches || opts.files_without_match {
        let print = if opts.files_with_matches {
            had_match
        } else {
            !had_match
        };
        if print {
            if opts.null {
                print!("{path}\0");
            } else {
                println!("{}", paint.path(&path));
            }
        }
        return had_match;
    }

    if opts.count || opts.count_matches {
        if had_match {
            let count = if opts.count_matches {
                per_line
                    .iter()
                    .map(|h| {
                        if h.matches.is_empty() {
                            1
                        } else {
                            h.matches.len() as u64
                        }
                    })
                    .sum::<u64>()
            } else {
                per_line.len() as u64
            };
            let sep = if opts.null { "\0" } else { ":" };
            println!("{}{}{}", paint.path(&path), paint.sep(sep), count);
        }
        return had_match;
    }

    if opts.vimgrep {
        render_vimgrep(opts, &path, &per_line, paint);
        return had_match;
    }

    if opts.only_matching {
        render_only_matching(opts, &path, &per_line, paint, filename, line_number);
        return had_match;
    }

    if opts.passthru {
        render_passthru(
            opts,
            &path,
            &lines,
            &per_line,
            paint,
            filename,
            line_number,
            heading,
            t.id,
        );
        return had_match;
    }

    // Default rendering with context.
    if had_match && heading {
        print_heading(&path, t, paint);
    }

    render_default(
        opts,
        &path,
        &lines,
        &per_line,
        paint,
        filename,
        line_number,
        heading,
    );

    had_match
}

struct LineHit {
    line_no: u64,
    text: String,
    matches: Vec<(usize, usize)>,
}

fn split_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text.split('\n').map(strip_cr).collect();
    // Drop phantom trailing empty line when text ends with \n.
    if text.ends_with('\n')
        && let Some(last) = out.last()
        && last.is_empty()
    {
        out.pop();
    }
    out
}

fn strip_cr(s: &str) -> String {
    if let Some(stripped) = s.strip_suffix('\r') {
        stripped.to_string()
    } else {
        s.to_string()
    }
}

fn print_heading(path: &str, t: &Target, paint: &Paint) {
    let tag = if t.tag.is_empty() {
        String::new()
    } else {
        format!(" (tag={})", t.tag)
    };
    let title = if t.title.is_empty() {
        String::new()
    } else {
        format!(" (title={})", t.title)
    };
    let status = if t.running { "" } else { " [exited]" };
    println!("{}{tag}{title}{status}", paint.path(path));
}

fn col_of(line: &str, byte: usize) -> usize {
    line[..byte.min(line.len())].chars().count() + 1
}

// ── Default renderer (with context) ────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_default(
    opts: &Opts,
    path: &str,
    lines: &[String],
    hits: &[LineHit],
    paint: &Paint,
    filename: bool,
    line_number: bool,
    heading: bool,
) {
    if hits.is_empty() {
        return;
    }

    // Build a set of matched line numbers for quick look-up.
    let matched: std::collections::HashMap<u64, &LineHit> =
        hits.iter().map(|h| (h.line_no, h)).collect();

    // Determine the "extent" of each match group (for context).
    let mut groups: Vec<(u64, u64)> = Vec::new();
    for h in hits {
        let lo = h.line_no.saturating_sub(opts.before as u64).max(1);
        let hi = (h.line_no + opts.after as u64).min(lines.len() as u64);
        match groups.last_mut() {
            Some(g) if g.1 + 1 >= lo => g.1 = g.1.max(hi),
            _ => groups.push((lo, hi)),
        }
    }

    let mut first_group = true;
    let mut last_end: Option<u64> = None;
    let emit_ctx_sep = !opts.no_context_separator && (opts.before > 0 || opts.after > 0);

    for (lo, hi) in groups {
        if !first_group
            && emit_ctx_sep
            && let Some(le) = last_end
            && le + 1 < lo
        {
            println!("{}", paint.sep(&opts.context_separator));
        }
        first_group = false;
        for ln in lo..=hi {
            let idx = (ln as usize) - 1;
            let line_text = lines.get(idx).map(String::as_str).unwrap_or("");
            if let Some(h) = matched.get(&ln) {
                emit_line(
                    opts,
                    path,
                    h.line_no,
                    line_text,
                    &h.matches,
                    paint,
                    filename,
                    line_number,
                    heading,
                    true,
                    opts.column,
                );
            } else {
                emit_line(
                    opts,
                    path,
                    ln,
                    line_text,
                    &[],
                    paint,
                    filename,
                    line_number,
                    heading,
                    false,
                    false,
                );
            }
        }
        last_end = Some(hi);

        if opts.stop_on_nonmatch {
            // If there's any matched line in this group followed by non-match
            // (which is the case as soon as after_context > 0), break.
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_line(
    opts: &Opts,
    path: &str,
    line_no: u64,
    text: &str,
    matches: &[(usize, usize)],
    paint: &Paint,
    filename: bool,
    line_number: bool,
    heading: bool,
    is_match: bool,
    show_column: bool,
) {
    let mut parts: Vec<String> = Vec::new();
    let sep_str = if is_match {
        &opts.field_match_separator
    } else {
        &opts.field_context_separator
    };
    if filename && !heading {
        if opts.null {
            parts.push(format!("{}\0", paint.path(path)));
        } else {
            parts.push(format!("{}{}", paint.path(path), paint.sep(sep_str)));
        }
    }
    if line_number {
        parts.push(format!(
            "{}{}",
            paint.line_num(&line_no.to_string()),
            paint.sep(sep_str)
        ));
    }
    if show_column && !matches.is_empty() {
        let col = col_of(text, matches[0].0);
        parts.push(format!(
            "{}{}",
            paint.line_num(&col.to_string()),
            paint.sep(sep_str)
        ));
    }
    let rendered = if is_match && !matches.is_empty() {
        highlight(text, matches, paint)
    } else {
        text.to_string()
    };
    parts.push(rendered);
    println!("{}", parts.join(""));
}

fn highlight(text: &str, matches: &[(usize, usize)], paint: &Paint) -> String {
    if !paint.enabled || matches.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + matches.len() * 10);
    let mut cursor = 0usize;
    for &(s, e) in matches {
        if s < cursor {
            continue;
        }
        out.push_str(&text[cursor..s]);
        out.push_str(&paint.match_(&text[s..e]));
        cursor = e;
    }
    out.push_str(&text[cursor..]);
    out
}

fn render_zero_matches(opts: &Opts, path: &str, paint: &Paint) -> bool {
    if opts.files_without_match {
        if opts.null {
            print!("{path}\0");
        } else {
            println!("{}", paint.path(path));
        }
    }
    false
}

// ── Vimgrep renderer ───────────────────────────────────────────────────────

fn render_vimgrep(opts: &Opts, path: &str, hits: &[LineHit], paint: &Paint) {
    for h in hits {
        for &(s, _e) in &h.matches {
            let col = col_of(&h.text, s);
            println!(
                "{}{}{}{}{}{}{}",
                paint.path(path),
                paint.sep(&opts.field_match_separator),
                paint.line_num(&h.line_no.to_string()),
                paint.sep(&opts.field_match_separator),
                paint.line_num(&col.to_string()),
                paint.sep(&opts.field_match_separator),
                highlight(&h.text, &h.matches, paint),
            );
        }
        if h.matches.is_empty() {
            // invert case: emit a single entry per inverted line, column 1
            println!(
                "{}{}{}{}1{}{}",
                paint.path(path),
                paint.sep(&opts.field_match_separator),
                paint.line_num(&h.line_no.to_string()),
                paint.sep(&opts.field_match_separator),
                paint.sep(&opts.field_match_separator),
                h.text,
            );
        }
    }
}

// ── only-matching renderer ─────────────────────────────────────────────────

fn render_only_matching(
    opts: &Opts,
    path: &str,
    hits: &[LineHit],
    paint: &Paint,
    filename: bool,
    line_number: bool,
) {
    for h in hits {
        if h.matches.is_empty() {
            // invert case: -o + -v is unusual; rg emits whole line
            emit_line(
                opts,
                path,
                h.line_no,
                &h.text,
                &[],
                paint,
                filename,
                line_number,
                false,
                true,
                false,
            );
            continue;
        }
        for &(s, e) in &h.matches {
            let slice = &h.text[s..e];
            let mut parts: Vec<String> = Vec::new();
            if filename {
                if opts.null {
                    parts.push(format!("{}\0", paint.path(path)));
                } else {
                    parts.push(format!(
                        "{}{}",
                        paint.path(path),
                        paint.sep(&opts.field_match_separator)
                    ));
                }
            }
            if line_number {
                parts.push(format!(
                    "{}{}",
                    paint.line_num(&h.line_no.to_string()),
                    paint.sep(&opts.field_match_separator)
                ));
            }
            parts.push(paint.match_(slice));
            println!("{}", parts.join(""));
        }
    }
}

// ── passthru renderer ──────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_passthru(
    opts: &Opts,
    path: &str,
    lines: &[String],
    hits: &[LineHit],
    paint: &Paint,
    filename: bool,
    line_number: bool,
    heading: bool,
    _pty_id: u16,
) {
    let matched: std::collections::HashMap<u64, &LineHit> =
        hits.iter().map(|h| (h.line_no, h)).collect();
    if !hits.is_empty() && heading {
        println!("{}", paint.path(path));
    }
    for (idx, line) in lines.iter().enumerate() {
        let ln = (idx as u64) + 1;
        if let Some(h) = matched.get(&ln) {
            emit_line(
                opts,
                path,
                ln,
                line,
                &h.matches,
                paint,
                filename,
                line_number,
                heading,
                true,
                opts.column,
            );
        } else {
            emit_line(
                opts,
                path,
                ln,
                line,
                &[],
                paint,
                filename,
                line_number,
                heading,
                false,
                false,
            );
        }
    }
}

// ── JSON renderer (rg schema) ──────────────────────────────────────────────

fn render_json(opts: &Opts, t: &Target, path: &str, hits: &[LineHit], text: &str) {
    let begin = serde_json::json!({
        "type": "begin",
        "data": { "path": { "text": path } }
    });
    println!("{begin}");

    // For --json we emit match events only (no context events here — the rg
    // JSON schema supports `context` events, but we don't synthesize context
    // lines in JSON mode to keep the output compact; -A/-B/-C are ignored
    // under --json, matching rg's behavior for stripped outputs.
    let _ = opts;
    let bytes = text.as_bytes();
    for h in hits {
        // Absolute byte offset of this line in the full text: find nth '\n' pos + 1.
        let mut offset = 0usize;
        let mut seen = 0u64;
        for (i, &b) in bytes.iter().enumerate() {
            if seen + 1 == h.line_no {
                offset = i;
                break;
            }
            if b == b'\n' {
                seen += 1;
                if seen + 1 == h.line_no {
                    offset = i + 1;
                    break;
                }
            }
        }
        let submatches: Vec<serde_json::Value> = h
            .matches
            .iter()
            .map(|&(s, e)| {
                serde_json::json!({
                    "match": { "text": &h.text.get(s..e).unwrap_or("") },
                    "start": s,
                    "end": e,
                })
            })
            .collect();
        let ev = serde_json::json!({
            "type": "match",
            "data": {
                "path": { "text": path },
                "lines": { "text": format!("{}\n", h.text) },
                "line_number": h.line_no,
                "absolute_offset": offset,
                "submatches": submatches,
            }
        });
        println!("{ev}");
    }

    let had_match = !hits.is_empty();
    let end = serde_json::json!({
        "type": "end",
        "data": {
            "path": { "text": path },
            "binary_offset": null,
            "stats": {
                "elapsed": { "secs": 0, "nanos": 0, "human": "0s" },
                "searches": 1,
                "searches_with_match": if had_match { 1 } else { 0 },
                "bytes_searched": text.len() as u64,
                "bytes_printed": 0,
                "matched_lines": hits.len() as u64,
                "matches": hits.iter().map(|h| if h.matches.is_empty() { 1 } else { h.matches.len() as u64 }).sum::<u64>(),
            }
        }
    });
    println!("{end}");
    let _ = t;
}
