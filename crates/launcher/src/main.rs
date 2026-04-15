//! Tiny static launcher for blit on Linux.
//!
//! The main blit binary is dynamically linked against musl libc so that
//! `dlopen` works (needed for GPU acceleration: VA-API, NVENC, Vulkan).
//! Because the ELF interpreter path (PT_INTERP) must be absolute and we
//! don't know the install directory at build time, this launcher resolves
//! the bundled musl dynamic linker relative to its own location and
//! `exec()`s the real binary through it.
//!
//! Layout expected relative to this binary:
//!
//!     blit                          <- this launcher (static)
//!     lib/blit/blit                 <- real binary (dynamic musl)
//!     lib/blit/ld-musl-<arch>.so.1  <- musl dynamic linker

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let dir = launcher_dir();

    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        fatal("unsupported architecture");
    };

    let loader = dir.join(format!("lib/blit/ld-musl-{arch}.so.1"));
    let real_bin = dir.join("lib/blit/blit");

    if !loader.exists() {
        fatal(&format!("musl loader not found: {}", loader.display()));
    }
    if !real_bin.exists() {
        fatal(&format!("blit binary not found: {}", real_bin.display()));
    }

    // exec() replaces this process — it never returns on success.
    let err = Command::new(&loader)
        .arg(&real_bin)
        .args(std::env::args_os().skip(1))
        .env("BLIT_WRAPPER_DIR", &dir)
        .exec();

    fatal(&format!(
        "exec {}: {err}",
        loader.display()
    ));
}

/// Resolve the directory containing this launcher binary.
fn launcher_dir() -> PathBuf {
    // /proc/self/exe is always available on Linux and gives the
    // canonical absolute path even if invoked via $PATH.
    std::fs::read_link("/proc/self/exe")
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| {
            // Fallback: try current_exe (may fail in edge cases).
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
        })
}

fn fatal(msg: &str) -> ! {
    eprintln!("blit: {msg}");
    std::process::exit(1);
}
