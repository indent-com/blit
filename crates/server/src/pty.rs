#[cfg(unix)]
mod pty_unix;
#[cfg(unix)]
pub use pty_unix::*;

#[cfg(windows)]
mod pty_windows;
#[cfg(windows)]
pub use pty_windows::*;
