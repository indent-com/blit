#[cfg(unix)]
mod ipc_unix;
#[cfg(unix)]
pub use ipc_unix::*;

#[cfg(windows)]
mod ipc_windows;
#[cfg(windows)]
pub use ipc_windows::*;
