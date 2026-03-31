/// blit-chaos: writes N random characters per second at random screen positions.
/// Useful for stress-testing blit's throughput and rendering pipeline.
///
/// Usage: blit-chaos [CPS]   (default: 1000 characters per second)
use std::io::{self, Write};
use std::time::{Duration, Instant};

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
}

struct RawMode {
    saved: libc::termios,
}

impl RawMode {
    fn enter() -> Self {
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            libc::tcgetattr(libc::STDIN_FILENO, &mut saved);
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            raw.c_cc[libc::VMIN] = 0;
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

struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        const RESET: &[u8] = b"\x1b[?25h\x1b[0m\x1b[2J\x1b[H\x1b[?1049l";
        unsafe {
            libc::write(libc::STDOUT_FILENO, RESET.as_ptr().cast(), RESET.len());
        }
    }
}

const GLYPHS: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*+-=[]{}|;:,.<>?";

fn push_dec(buf: &mut Vec<u8>, mut n: u16) {
    if n >= 100 {
        buf.push(b'0' + (n / 100) as u8);
        n %= 100;
        buf.push(b'0' + (n / 10) as u8);
        buf.push(b'0' + (n % 10) as u8);
    } else if n >= 10 {
        buf.push(b'0' + (n / 10) as u8);
        buf.push(b'0' + (n % 10) as u8);
    } else {
        buf.push(b'0' + n as u8);
    }
}

fn main() -> io::Result<()> {
    let target_cps: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);

    let interval = Duration::from_nanos(1_000_000_000 / target_cps.max(1));

    let mut out = io::stdout();

    write!(out, "\x1b[?1049h\x1b[?25l\x1b[2J")?;
    out.flush()?;

    let _raw = RawMode::enter();
    let _cleanup = Cleanup;

    let mut rng = Rng::new();
    let (mut cols, mut rows) = get_term_size();

    let mut seq: Vec<u8> = Vec::with_capacity(16);
    let mut stdin_buf = [0u8; 64];
    let mut chars_written: u64 = 0;
    let start = Instant::now();
    let mut next_emit = Instant::now();
    let mut last_title = Instant::now();

    loop {
        // Non-blocking stdin check for quit keys.
        let n = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                stdin_buf.as_mut_ptr().cast(),
                stdin_buf.len(),
            )
        };
        if n > 0 {
            for &b in &stdin_buf[..n as usize] {
                if b == b'q' || b == 0x03 || b == 0x04 || b == 0x1b {
                    return Ok(());
                }
            }
        }

        let now = Instant::now();
        if now < next_emit {
            std::hint::spin_loop();
            continue;
        }
        next_emit += interval;
        // Don't let the deadline drift arbitrarily far behind.
        if now > next_emit + interval * 8 {
            next_emit = now + interval;
        }

        let (new_cols, new_rows) = get_term_size();
        if new_cols != cols || new_rows != rows {
            cols = new_cols;
            rows = new_rows;
            write!(out, "\x1b[2J")?;
        }

        let row = (rng.next() % rows as u64) as u16 + 1;
        let col = (rng.next() % cols as u64) as u16 + 1;
        let ch = GLYPHS[(rng.next() % GLYPHS.len() as u64) as usize];
        seq.clear();
        seq.extend_from_slice(b"\x1b[");
        push_dec(&mut seq, row);
        seq.push(b';');
        push_dec(&mut seq, col);
        seq.push(b'H');
        seq.push(ch);
        out.write_all(&seq)?;
        out.flush()?;
        chars_written += 1;

        if last_title.elapsed() >= Duration::from_secs(1) {
            let elapsed = start.elapsed().as_secs_f64();
            let actual_cps = chars_written as f64 / elapsed.max(0.001);
            write!(
                out,
                "\x1b]0;blit-chaos | {chars_written} chars | {actual_cps:.0} cps\x07"
            )?;
            last_title = Instant::now();
        }

        out.flush()?;
    }
}
