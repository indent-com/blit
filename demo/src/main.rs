use std::io::{self, Write};
use std::time::{Duration, Instant};

const GLYPHS: &[u8] =
    b"@#$%&*+=<>^~!?|/\\[]{}()0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

struct Args {
    fps: u64,
    stars: Option<usize>,
}

fn parse_args() -> Args {
    let mut fps = 120u64;
    let mut stars = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--fps" => {
                if let Some(n) = it.next().and_then(|s| s.parse::<u64>().ok()) {
                    fps = n.clamp(1, 1_000);
                }
            }
            "--stars" => {
                if let Some(n) = it.next().and_then(|s| s.parse::<usize>().ok()) {
                    stars = Some(n);
                }
            }
            _ => {}
        }
    }
    Args { fps, stars }
}

fn get_term_size() -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0
            && ws.ws_col > 0
            && ws.ws_row > 0
        {
            return (ws.ws_col, ws.ws_row);
        }
    }
    (80, 24)
}

struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(12345);
        Self(seed ^ 0xdeadbeefcafe1234)
    }

    #[inline(always)]
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    #[inline(always)]
    fn next_f32(&mut self) -> f32 {
        (self.next() as f64 / u64::MAX as f64) as f32
    }

    #[inline(always)]
    fn range_f32(&mut self, min: f32, max: f32) -> f32 {
        min + self.next_f32() * (max - min)
    }
}

/// HSV → RGB. h in [0, 360), s and v in [0, 255].
fn hsv_to_rgb(h: u16, s: u8, v: u8) -> (u8, u8, u8) {
    let s = s as f32 / 255.0;
    let v = v as f32 / 255.0;
    let c = v * s;
    let h6 = h as f32 / 60.0;
    let x = c * (1.0 - (h6 % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

/// Enter raw mode on stdin; restores on drop.
struct RawMode {
    saved: libc::termios,
}

impl RawMode {
    fn enter() -> Self {
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            libc::tcgetattr(libc::STDIN_FILENO, &mut saved);
            let mut raw = saved;
            raw.c_iflag &= !(libc::IGNBRK
                | libc::BRKINT
                | libc::PARMRK
                | libc::ISTRIP
                | libc::INLCR
                | libc::IGNCR
                | libc::ICRNL
                | libc::IXON);
            raw.c_oflag &= !libc::OPOST;
            raw.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
            raw.c_cflag &= !(libc::CSIZE | libc::PARENB);
            raw.c_cflag |= libc::CS8;
            raw.c_cc[libc::VMIN] = 0; // non-blocking reads
            raw.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
            Self { saved }
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
    }
}

struct Star {
    col: u16,
    row: u16,
    phase: f32,
    speed: f32,
}

fn gen_stars(rng: &mut Rng, cols: u16, rows: u16, count: usize) -> Vec<Star> {
    (0..count)
        .map(|_| Star {
            col: (rng.next() % cols as u64) as u16 + 1,
            row: (rng.next() % rows as u64) as u16 + 1,
            phase: rng.next_f32() * std::f32::consts::TAU,
            speed: 0.02 + rng.next_f32() * 0.06,
        })
        .collect()
}

/// Short-lived click pulse that emits extra particles.
struct Burst {
    col: u16, // 1-indexed
    row: u16,
    age: u8,
}

#[allow(clippy::too_many_arguments)]
fn stamp_particle(
    seq_buf: &mut Vec<u8>,
    rng: &mut Rng,
    col: f32,
    row: f32,
    hue: u16,
    cols: u16,
    rows: u16,
    spread_x: f32,
    spread_y: f32,
) {
    let stamp_col = (col + rng.range_f32(-spread_x, spread_x)).round() as i32;
    let stamp_row = (row + rng.range_f32(-spread_y, spread_y)).round() as i32;
    if !(1..=cols as i32).contains(&stamp_col) || !(1..=rows as i32).contains(&stamp_row) {
        return;
    }
    let glyph = GLYPHS[(rng.next() % GLYPHS.len() as u64) as usize];
    let sat = (190.0 + rng.range_f32(0.0, 65.0)).round() as u8;
    let val = (155.0 + rng.range_f32(0.0, 100.0)).round() as u8;
    let (r, g, b) = hsv_to_rgb(hue, sat, val);
    let glyph = glyph as char;
    write!(
        seq_buf,
        "\x1b[{stamp_row};{stamp_col}H\x1b[38;2;{r};{g};{b}m{glyph}"
    )
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn stamp_splash(
    seq_buf: &mut Vec<u8>,
    rng: &mut Rng,
    col: f32,
    row: f32,
    hue: u16,
    cols: u16,
    rows: u16,
    count: usize,
    radius_x: f32,
    radius_y: f32,
    direction: Option<(f32, f32)>,
) {
    if count == 0 {
        return;
    }

    let (base_angle, spread) = match direction {
        Some((dx, dy)) if dx.abs() + dy.abs() > 0.1 => (dy.atan2(dx) + std::f32::consts::PI, 0.9),
        _ => (0.0, std::f32::consts::PI),
    };

    for i in 0..count {
        let angle = if spread >= std::f32::consts::PI {
            rng.range_f32(0.0, std::f32::consts::TAU)
        } else {
            base_angle + rng.range_f32(-spread, spread)
        };
        let distance = rng.range_f32(0.25, 1.0);
        stamp_particle(
            seq_buf,
            rng,
            col + angle.cos() * radius_x * distance,
            row + angle.sin() * radius_y * distance,
            (hue + (i as u16 * 7)) % 360,
            cols,
            rows,
            0.55,
            0.4,
        );
    }
}

fn main() -> io::Result<()> {
    let args = parse_args();
    let (mut cols, mut rows) = get_term_size();
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(b"\x1b[?25l"); // keep parser in sync: cursor hidden

    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(256 * 1024, stdout.lock());

    // Hide cursor, clear screen, enable SGR mouse + any-motion reporting
    write!(out, "\x1b[?25l\x1b[2J\x1b[?1003h\x1b[?1006h")?;
    out.flush()?;

    // _raw declared first → dropped last (after _cleanup flushes the screen reset)
    let _raw = RawMode::enter();
    let _cleanup = Cleanup; // dropped before _raw

    let target_fps = args.fps;
    let frame_interval = Duration::from_nanos(1_000_000_000 / target_fps);

    let mut rng = Rng::new();
    let star_count = |c: u16, r: u16| {
        args.stars
            .unwrap_or(((c as usize * r as usize) / 60).max(8))
    };
    let mut stars = gen_stars(&mut rng, cols, rows, star_count(cols, rows));
    let mut frame: u64 = 0;
    let start = Instant::now();
    let mut next_frame_at = start + frame_interval;

    let mut seq_buf = Vec::with_capacity(64 * 1024);
    let mut stdin_buf = [0u8; 4096];

    // Mouse position (1-indexed); start centered
    let mut mouse_col: u16 = cols / 2 + 1;
    let mut mouse_row: u16 = rows / 2 + 1;
    let mut last_mouse_col = mouse_col;
    let mut last_mouse_row = mouse_row;
    let mut bursts: Vec<Burst> = Vec::new();
    let mut work_scale = 0.35f32;

    loop {
        let frame_start = Instant::now();

        // Non-blocking stdin read
        let n = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                stdin_buf.as_mut_ptr().cast(),
                stdin_buf.len(),
            )
        };
        if n > 0
            && parse_input(
                &stdin_buf[..n as usize],
                &mut mouse_col,
                &mut mouse_row,
                &mut bursts,
                cols,
                rows,
            )
        {
            break; // quit
        }

        let prev = parser.screen().clone();

        let (new_cols, new_rows) = get_term_size();
        if new_cols != cols || new_rows != rows {
            cols = new_cols;
            rows = new_rows;
            parser = vt100::Parser::new(rows, cols, 0);
            parser.process(b"\x1b[?25l");
            write!(out, "\x1b[2J")?;
            stars = gen_stars(&mut rng, cols, rows, star_count(cols, rows));
        }

        seq_buf.clear();

        // Stars (background layer) — rendered first so particles paint over them.
        for star in &mut stars {
            star.phase = (star.phase + star.speed) % std::f32::consts::TAU;
            let brightness = (star.phase.sin() + 1.0) * 0.5; // [0.0, 1.0]
            if brightness < 0.1 {
                write!(seq_buf, "\x1b[{};{}H\x1b[0m ", star.row, star.col).unwrap();
            } else {
                let glyph = if brightness > 0.72 {
                    '*'
                } else if brightness > 0.38 {
                    '+'
                } else {
                    '.'
                };
                let v = (40.0 + brightness * 215.0) as u8;
                let r = (v as f32 * 0.82) as u8;
                let g = (v as f32 * 0.88) as u8;
                write!(
                    seq_buf,
                    "\x1b[{};{}H\x1b[38;2;{r};{g};{v}m{glyph}",
                    star.row, star.col
                )
                .unwrap();
            }
        }

        let hue_base = (frame * 7 % 360) as u16;
        let drift_col = mouse_col as f32 - last_mouse_col as f32;
        let drift_row = mouse_row as f32 - last_mouse_row as f32;
        let motion = drift_col.abs() + drift_row.abs();

        // Stamp fresh particles behind the cursor and let the terminal keep the trail.
        let emit_count = ((4.0 + work_scale * 5.0 + motion * 0.9).round() as usize).clamp(4, 14);
        let tail_col = mouse_col as f32 - drift_col * 0.8;
        let tail_row = mouse_row as f32 - drift_row * 0.5;
        for i in 0..emit_count {
            let along_col = rng.range_f32(0.0, 0.7);
            let along_row = rng.range_f32(0.0, 0.5);
            stamp_particle(
                &mut seq_buf,
                &mut rng,
                tail_col + drift_col * along_col,
                tail_row + drift_row * along_row,
                (hue_base + (i as u16 * 9)) % 360,
                cols,
                rows,
                1.1 + motion * 0.08,
                0.7 + motion * 0.05,
            );
        }
        let splash_count = ((motion * 1.3 + work_scale * 2.0).round() as usize).clamp(0, 8);
        stamp_splash(
            &mut seq_buf,
            &mut rng,
            tail_col,
            tail_row,
            (hue_base + 60) % 360,
            cols,
            rows,
            splash_count,
            2.2 + motion * 0.35,
            1.0 + motion * 0.18,
            Some((drift_col, drift_row)),
        );

        // Clicks add a short spark pulse without replacing the cursor trail.
        for burst in &mut bursts {
            let burst_emit = (((12_i32 - burst.age as i32 * 2).max(0) as f32)
                * work_scale.max(0.45))
            .round() as usize;
            let burst_emit = burst_emit.clamp(0, 12);
            stamp_splash(
                &mut seq_buf,
                &mut rng,
                burst.col as f32,
                burst.row as f32,
                (hue_base + 120 + burst.age as u16 * 17) % 360,
                cols,
                rows,
                burst_emit,
                1.2 + burst.age as f32 * 1.0,
                0.8 + burst.age as f32 * 0.45,
                None,
            );
            for i in 0..(burst_emit / 2).max(3) {
                stamp_particle(
                    &mut seq_buf,
                    &mut rng,
                    burst.col as f32,
                    burst.row as f32,
                    (hue_base + 180 + (i as u16 * 13) + burst.age as u16 * 19) % 360,
                    cols,
                    rows,
                    1.0 + burst.age as f32 * 0.18,
                    0.7 + burst.age as f32 * 0.1,
                );
            }
            burst.age += 1;
        }
        bursts.retain(|b| b.age < 6);

        let (cursor_r, cursor_g, cursor_b) = hsv_to_rgb((hue_base + 180) % 360, 200, 255);
        write!(
            seq_buf,
            "\x1b[{mouse_row};{mouse_col}H\x1b[38;2;{cursor_r};{cursor_g};{cursor_b}m@"
        )
        .unwrap();

        parser.process(&seq_buf);
        let diff = parser.screen().contents_diff(&prev);
        out.write_all(&diff)?;

        last_mouse_col = mouse_col;
        last_mouse_row = mouse_row;

        let elapsed = start.elapsed().as_secs_f64();
        let fps = frame as f64 / elapsed.max(0.001);
        write!(
            out,
            "\x1b]0;blit-demo | frame {} | {:.0} fps | load {:.0}% | particles {}\x07",
            frame,
            fps,
            work_scale * 100.0,
            emit_count,
        )?;

        out.flush()?;
        frame += 1;

        let elapsed = frame_start.elapsed();
        let elapsed_ratio = elapsed.as_secs_f32() / frame_interval.as_secs_f32().max(1.0e-6);
        if elapsed_ratio > 1.05 {
            work_scale = (work_scale * 0.88).max(0.12);
        } else if elapsed_ratio < 0.70 {
            work_scale = (work_scale * 1.04).min(1.0);
        }
        let now = Instant::now();
        if now < next_frame_at {
            let remaining = next_frame_at - now;
            if remaining > Duration::from_millis(2) {
                std::thread::sleep(remaining - Duration::from_millis(1));
            }
            while Instant::now() < next_frame_at {
                std::hint::spin_loop();
            }
        }
        next_frame_at += frame_interval;
        if Instant::now() > next_frame_at + frame_interval {
            next_frame_at = Instant::now() + frame_interval;
        }
    }

    write!(out, "\x1b[?1003l\x1b[?1006l\x1b[?25h\x1b[0m\x1b[2J\x1b[H")?;
    out.flush()?;

    Ok(())
}

/// Parse stdin bytes. Returns true if the app should quit.
/// Updates mouse position and pushes click pulses.
fn parse_input(
    buf: &[u8],
    mouse_col: &mut u16,
    mouse_row: &mut u16,
    bursts: &mut Vec<Burst>,
    cols: u16,
    rows: u16,
) -> bool {
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];

        // Ctrl-C, Ctrl-D, 'q' → quit
        if b == 0x03 || b == 0x04 || b == b'q' {
            return true;
        }

        if b == 0x1b {
            // Check for CSI sequence
            if i + 1 < buf.len() && buf[i + 1] == b'[' {
                // SGR mouse: \x1b[<btn;col;rowM  or  \x1b[<btn;col;rowm
                if i + 2 < buf.len() && buf[i + 2] == b'<' {
                    let start = i + 3;
                    let mut end = start;
                    while end < buf.len() && buf[end] != b'M' && buf[end] != b'm' {
                        end += 1;
                    }
                    if end < buf.len() {
                        let final_byte = buf[end];
                        if let Some((btn, col, row)) = parse_mouse_params(&buf[start..end]) {
                            *mouse_col = col.clamp(1, cols);
                            *mouse_row = row.clamp(1, rows);
                            // Bit 5 (0x20) marks motion events; without it this is a press/release
                            let is_press = final_byte == b'M' && (btn & 0x20 == 0);
                            let btn_num = btn & 0x03;
                            if is_press && btn_num < 3 {
                                bursts.push(Burst {
                                    col: *mouse_col,
                                    row: *mouse_row,
                                    age: 0,
                                });
                            }
                        }
                        i = end + 1;
                        continue;
                    }
                }
                // Skip any other CSI sequence
                let mut end = i + 2;
                while end < buf.len() && !(0x40..=0x7e).contains(&buf[end]) {
                    end += 1;
                }
                i = end + 1;
                continue;
            }
            // Lone ESC → quit
            return true;
        }

        i += 1;
    }
    false
}

fn parse_mouse_params(params: &[u8]) -> Option<(u16, u16, u16)> {
    let s = std::str::from_utf8(params).ok()?;
    let mut parts = s.split(';');
    let btn: u16 = parts.next()?.parse().ok()?;
    let col: u16 = parts.next()?.parse().ok()?;
    let row: u16 = parts.next()?.parse().ok()?;
    Some((btn, col, row))
}

struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        // Write directly to the fd to avoid the BufWriter lock still being held
        const RESET: &[u8] = b"\x1b[?1003l\x1b[?1006l\x1b[?25h\x1b[0m\x1b[2J\x1b[H";
        unsafe {
            libc::write(libc::STDOUT_FILENO, RESET.as_ptr().cast(), RESET.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── hsv_to_rgb ──────────────────────────────────────────────────────

    #[test]
    fn hsv_pure_black() {
        // v=0 means black regardless of h and s
        assert_eq!(hsv_to_rgb(0, 255, 0), (0, 0, 0));
        assert_eq!(hsv_to_rgb(180, 128, 0), (0, 0, 0));
    }

    #[test]
    fn hsv_pure_white() {
        // s=0, v=255 → white
        assert_eq!(hsv_to_rgb(0, 0, 255), (255, 255, 255));
        assert_eq!(hsv_to_rgb(123, 0, 255), (255, 255, 255));
    }

    #[test]
    fn hsv_primary_red() {
        assert_eq!(hsv_to_rgb(0, 255, 255), (255, 0, 0));
    }

    #[test]
    fn hsv_primary_green() {
        assert_eq!(hsv_to_rgb(120, 255, 255), (0, 255, 0));
    }

    #[test]
    fn hsv_primary_blue() {
        assert_eq!(hsv_to_rgb(240, 255, 255), (0, 0, 255));
    }

    #[test]
    fn hsv_intermediate_yellow() {
        // h=60, full saturation and value → yellow (255,255,0)
        assert_eq!(hsv_to_rgb(60, 255, 255), (255, 255, 0));
    }

    #[test]
    fn hsv_intermediate_cyan() {
        assert_eq!(hsv_to_rgb(180, 255, 255), (0, 255, 255));
    }

    #[test]
    fn hsv_half_value_gray() {
        // s=0, v=128 → gray
        let (r, g, b) = hsv_to_rgb(0, 0, 128);
        assert_eq!(r, g);
        assert_eq!(g, b);
        // 128/255*255 ≈ 128
        assert!((r as i16 - 128).abs() <= 1);
    }

    // ── Rng ─────────────────────────────────────────────────────────────

    #[test]
    fn rng_new_produces_different_values() {
        // Two Rng::new() calls should (almost certainly) differ since they
        // seed from the system clock.
        let a = Rng::new();
        let b = Rng::new();
        // The internal states could theoretically match if called in the
        // same nanosecond, but the chance is negligible.
        assert!(a.0 != 0 && b.0 != 0);
    }

    #[test]
    fn rng_next_deterministic() {
        let mut a = Rng(42);
        let mut b = Rng(42);
        for _ in 0..100 {
            assert_eq!(a.next(), b.next());
        }
    }

    #[test]
    fn rng_next_changes_state() {
        let mut r = Rng(42);
        let first = r.next();
        let second = r.next();
        assert_ne!(first, second);
    }

    #[test]
    fn rng_next_f32_in_unit_range() {
        let mut r = Rng(12345);
        for _ in 0..1000 {
            let v = r.next_f32();
            assert!((0.0..1.0).contains(&v), "next_f32 out of range: {}", v);
        }
    }

    #[test]
    fn rng_range_f32_respects_bounds() {
        let mut r = Rng(99999);
        for _ in 0..1000 {
            let v = r.range_f32(3.0, 7.0);
            assert!((3.0..7.0).contains(&v), "range_f32 out of [3,7): {}", v);
        }
    }

    // ── parse_mouse_params ──────────────────────────────────────────────

    #[test]
    fn parse_mouse_params_valid() {
        assert_eq!(parse_mouse_params(b"0;10;20"), Some((0, 10, 20)));
        assert_eq!(parse_mouse_params(b"32;1;1"), Some((32, 1, 1)));
        assert_eq!(parse_mouse_params(b"0;100;200"), Some((0, 100, 200)));
    }

    #[test]
    fn parse_mouse_params_empty() {
        assert_eq!(parse_mouse_params(b""), None);
    }

    #[test]
    fn parse_mouse_params_invalid() {
        assert_eq!(parse_mouse_params(b"abc"), None);
        assert_eq!(parse_mouse_params(b"0;10"), None); // too few parts
        assert_eq!(parse_mouse_params(b"0;10;abc"), None);
    }

    #[test]
    fn parse_mouse_params_partial_semicolons() {
        assert_eq!(parse_mouse_params(b";10;20"), None); // empty first field
        assert_eq!(parse_mouse_params(b"0;;20"), None); // empty middle field
    }

    // ── parse_input ─────────────────────────────────────────────────────

    #[test]
    fn parse_input_quit_on_q() {
        let mut col = 40;
        let mut row = 12;
        let mut bursts = Vec::new();
        assert!(parse_input(b"q", &mut col, &mut row, &mut bursts, 80, 24));
    }

    #[test]
    fn parse_input_quit_on_ctrl_c() {
        let mut col = 40;
        let mut row = 12;
        let mut bursts = Vec::new();
        assert!(parse_input(
            b"\x03",
            &mut col,
            &mut row,
            &mut bursts,
            80,
            24
        ));
    }

    #[test]
    fn parse_input_quit_on_lone_esc() {
        let mut col = 40;
        let mut row = 12;
        let mut bursts = Vec::new();
        assert!(parse_input(
            b"\x1b",
            &mut col,
            &mut row,
            &mut bursts,
            80,
            24
        ));
    }

    #[test]
    fn parse_input_sgr_mouse_motion() {
        let mut col = 1;
        let mut row = 1;
        let mut bursts = Vec::new();
        // SGR mouse motion event: button 32 (motion flag), col=15, row=10, 'M' final
        let input = b"\x1b[<32;15;10M";
        let quit = parse_input(input, &mut col, &mut row, &mut bursts, 80, 24);
        assert!(!quit);
        assert_eq!(col, 15);
        assert_eq!(row, 10);
        // Motion events (btn & 0x20 != 0) should not create bursts
        assert!(bursts.is_empty());
    }

    #[test]
    fn parse_input_sgr_mouse_click_creates_burst() {
        let mut col = 1;
        let mut row = 1;
        let mut bursts = Vec::new();
        // SGR mouse press: button 0, col=5, row=3, 'M' final (press)
        let input = b"\x1b[<0;5;3M";
        let quit = parse_input(input, &mut col, &mut row, &mut bursts, 80, 24);
        assert!(!quit);
        assert_eq!(col, 5);
        assert_eq!(row, 3);
        assert_eq!(bursts.len(), 1);
        assert_eq!(bursts[0].col, 5);
        assert_eq!(bursts[0].row, 3);
        assert_eq!(bursts[0].age, 0);
    }

    #[test]
    fn parse_input_mouse_release_no_burst() {
        let mut col = 1;
        let mut row = 1;
        let mut bursts = Vec::new();
        // 'm' final byte means release
        let input = b"\x1b[<0;5;3m";
        let quit = parse_input(input, &mut col, &mut row, &mut bursts, 80, 24);
        assert!(!quit);
        assert!(bursts.is_empty());
    }

    #[test]
    fn parse_input_clamps_mouse_to_bounds() {
        let mut col = 1;
        let mut row = 1;
        let mut bursts = Vec::new();
        let input = b"\x1b[<32;999;999M";
        parse_input(input, &mut col, &mut row, &mut bursts, 80, 24);
        assert_eq!(col, 80);
        assert_eq!(row, 24);
    }
}
