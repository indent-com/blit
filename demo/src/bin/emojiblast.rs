/// blit-emojiblast: fills the screen with random emojis as fast as possible.
/// Useful for stress-testing wide-character rendering and throughput.
///
/// Usage: blit-emojiblast
use std::io::{self, Write};

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

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const EMOJIS: &[&str] = &[
    "😀", "😂", "🥹", "😎", "🤩", "🥳", "😈", "👻", "💀", "👽",
    "🤖", "🎃", "🔥", "🌈", "🌊", "🍕", "🍔", "🌮", "🍣", "🎸",
    "🎮", "🚀", "🛸", "🏄", "🎯", "🧊", "💎", "🦊", "🐙", "🦑",
    "🦄", "🐉", "🦖", "🦕", "🐢", "🐬", "🦈", "🐝", "🦋", "🌻",
    "🌵", "🍄", "🎄", "🎪", "🎠", "🗿", "🏰", "🧲", "🔮", "🧿",
    "🎲", "🪁", "🛹", "🪂", "🏹", "🧩", "🪄", "🫧", "🪩", "🫠",
];

fn main() {
    let mut rng = Rng::new();
    let mut out = io::BufWriter::with_capacity(64 * 1024, io::stdout().lock());

    // Switch to alt screen, hide cursor.
    let _ = write!(out, "\x1b[?1049h\x1b[?25l");

    // Restore main screen on Ctrl-C / exit.
    unsafe {
        libc::signal(libc::SIGINT, cleanup as *const () as usize);
        libc::signal(libc::SIGTERM, cleanup as *const () as usize);
    }

    loop {
        let (cols, rows) = get_term_size();
        let emojis_per_row = cols as usize / 2;
        if emojis_per_row == 0 {
            continue;
        }

        let _ = write!(out, "\x1b[H");
        for row in 0..rows {
            for _ in 0..emojis_per_row {
                let emoji = EMOJIS[rng.next() as usize % EMOJIS.len()];
                let _ = out.write_all(emoji.as_bytes());
            }
            if cols % 2 != 0 {
                let _ = out.write_all(b" ");
            }
            if row + 1 < rows {
                let _ = out.write_all(b"\r\n");
            }
        }
        let _ = out.flush();
    }
}

extern "C" fn cleanup(_sig: libc::c_int) {
    // Async-signal-safe: raw write + _exit.
    let seq = b"\x1b[?25h\x1b[?1049l";
    unsafe {
        libc::write(libc::STDOUT_FILENO, seq.as_ptr() as *const _, seq.len());
        libc::_exit(0);
    }
}
