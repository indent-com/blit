//! Static launcher for blit on Linux.
//!
//! The main blit binary is dynamically linked against musl libc so that
//! `dlopen` works (needed for GPU acceleration: VA-API, NVENC, Vulkan).
//! Because the ELF interpreter path (PT_INTERP) must be absolute and we
//! don't know the install directory at build time, this launcher resolves
//! the bundled musl dynamic linker relative to its own location and
//! `exec()`s the real binary through it.
//!
//! Expected install layout (PREFIX = e.g. /usr/local):
//!
//!     PREFIX/bin/blit                          <- this launcher (static)
//!     PREFIX/lib/blit/blit                     <- real binary (dynamic musl)
//!     PREFIX/lib/blit/ld-musl-<arch>.so.1      <- musl dynamic linker

use std::ffi::CString;
use std::path::PathBuf;

#[cfg(target_arch = "x86_64")]
const LOADER_NAME: &str = "ld-musl-x86_64.so.1";
#[cfg(target_arch = "aarch64")]
const LOADER_NAME: &str = "ld-musl-aarch64.so.1";

const WRAPPER_ENV: &str = "BLIT_WRAPPER_DIR";

fn main() {
    let exe = std::env::current_exe().unwrap_or_else(|e| fatal(&format!("cannot resolve self: {e}")));

    // bin_dir = e.g. /usr/local/bin
    let bin_dir = exe.parent().unwrap_or_else(|| fatal("cannot resolve parent dir"));

    // Derive PREFIX by stripping the trailing /bin component.
    let prefix: PathBuf = if bin_dir.file_name().map(|n| n == "bin").unwrap_or(false) {
        bin_dir.parent().unwrap_or(bin_dir).to_path_buf()
    } else {
        bin_dir.to_path_buf()
    };

    let lib_dir = prefix.join("lib/blit");
    let loader = lib_dir.join(LOADER_NAME);
    let real_bin = lib_dir.join("blit");

    // Set BLIT_WRAPPER_DIR so the real binary can find the launcher for re-exec.
    // SAFETY: single-threaded launcher, no other threads can observe env.
    unsafe { std::env::set_var(WRAPPER_ENV, bin_dir) };

    // Build argv: [loader, real_bin, original_args[1..]]
    let mut args: Vec<CString> = Vec::new();
    let loader_c = to_cstring(&loader);
    let real_bin_c = to_cstring(&real_bin);
    args.push(loader_c.clone());
    args.push(real_bin_c);
    for arg in std::env::args_os().skip(1) {
        args.push(CString::new(arg.into_encoded_bytes()).unwrap_or_else(|_| fatal("invalid argument")));
    }

    // Build envp from current (modified) environment.
    let envp: Vec<CString> = std::env::vars_os()
        .map(|(k, v)| {
            let mut entry = k.into_encoded_bytes();
            entry.push(b'=');
            entry.extend_from_slice(&v.into_encoded_bytes());
            CString::new(entry).unwrap_or_else(|_| fatal("invalid env"))
        })
        .collect();

    let argv_ptrs: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).chain(std::iter::once(std::ptr::null())).collect();
    let envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|e| e.as_ptr()).chain(std::iter::once(std::ptr::null())).collect();

    unsafe {
        libc::execve(loader_c.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
    }

    // execve only returns on failure.
    fatal(&format!("exec failed: {}", std::io::Error::last_os_error()));
}

fn fatal(msg: &str) -> ! {
    eprintln!("blit: {msg}");
    std::process::exit(1);
}

fn to_cstring(p: &std::path::Path) -> CString {
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_os_str().as_bytes()).unwrap_or_else(|_| fatal("path contains null"))
}
