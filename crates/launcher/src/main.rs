//! Tiny no_std static launcher for blit on Linux.
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
//!     PREFIX/bin/blit                          <- this launcher (static, no_std)
//!     PREFIX/lib/blit/blit                     <- real binary (dynamic musl)
//!     PREFIX/lib/blit/ld-musl-<arch>.so.1      <- musl dynamic linker

#![no_std]
#![no_main]

use core::panic::PanicInfo;
use core::ptr;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    unsafe { libc::_exit(127) }
}

const MAX_PATH: usize = libc::PATH_MAX as usize;

#[cfg(target_arch = "x86_64")]
const LOADER_SUFFIX: &[u8] = b"/lib/blit/ld-musl-x86_64.so.1\0";
#[cfg(target_arch = "aarch64")]
const LOADER_SUFFIX: &[u8] = b"/lib/blit/ld-musl-aarch64.so.1\0";

const BLIT_SUFFIX: &[u8] = b"/lib/blit/blit\0";
const BIN_COMPONENT: &[u8] = b"/bin";
const WRAPPER_PREFIX: &[u8] = b"BLIT_WRAPPER_DIR=";
const PROC_SELF_EXE: &[u8] = b"/proc/self/exe\0";

unsafe fn alloc_ptrs(count: usize) -> *mut *const libc::c_char {
    let p = unsafe { libc::malloc(count * core::mem::size_of::<*const libc::c_char>()) };
    if p.is_null() {
        fatal(b"blit: out of memory\n");
    }
    let p = p.cast::<*const libc::c_char>();
    unsafe {
        libc::memset(p.cast(), 0, count * core::mem::size_of::<*const libc::c_char>());
    }
    p
}

unsafe fn count_null_terminated(p: *const *const libc::c_char) -> usize {
    let mut n: usize = 0;
    if !p.is_null() {
        unsafe {
            while !(*p.add(n)).is_null() {
                n += 1;
            }
        }
    }
    n
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn main(
    argc: libc::c_int,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
) -> libc::c_int {
    unsafe {
        // Resolve our own path via /proc/self/exe.
        let mut exe_buf = [0u8; MAX_PATH];
        let exe_len = libc::readlink(
            PROC_SELF_EXE.as_ptr().cast(),
            exe_buf.as_mut_ptr().cast(),
            MAX_PATH - 1,
        );
        if exe_len <= 0 {
            fatal(b"blit: cannot read /proc/self/exe\n");
        }
        let exe_len = exe_len as usize;

        // Find parent directory of the binary (the bin/ dir).
        let mut bin_dir_len = exe_len;
        while bin_dir_len > 0 && exe_buf[bin_dir_len - 1] != b'/' {
            bin_dir_len -= 1;
        }
        if bin_dir_len > 0 {
            bin_dir_len -= 1; // strip the '/' itself
        }

        // Derive PREFIX by stripping the trailing /bin component.
        // If the parent dir doesn't end in /bin, fall back to it directly
        // (handles non-standard layouts where everything is in one dir).
        let mut prefix_len = bin_dir_len;
        if bin_dir_len >= BIN_COMPONENT.len() {
            let tail_start = bin_dir_len - BIN_COMPONENT.len();
            if slice_eq(&exe_buf[tail_start..bin_dir_len], BIN_COMPONENT) {
                // Ends with /bin — strip it, but keep at least root.
                prefix_len = if tail_start > 0 { tail_start } else { 1 };
            }
        }

        // Build loader path: <prefix> + LOADER_SUFFIX.
        let mut loader = [0u8; MAX_PATH];
        if prefix_len + LOADER_SUFFIX.len() >= MAX_PATH {
            fatal(b"blit: path too long\n");
        }
        copy_bytes(&exe_buf, prefix_len, &mut loader, 0);
        copy_bytes(LOADER_SUFFIX, LOADER_SUFFIX.len(), &mut loader, prefix_len);

        // Build real binary path: <prefix> + BLIT_SUFFIX.
        let mut real_bin = [0u8; MAX_PATH];
        if prefix_len + BLIT_SUFFIX.len() >= MAX_PATH {
            fatal(b"blit: path too long\n");
        }
        copy_bytes(&exe_buf, prefix_len, &mut real_bin, 0);
        copy_bytes(BLIT_SUFFIX, BLIT_SUFFIX.len(), &mut real_bin, prefix_len);

        // Build BLIT_WRAPPER_DIR=<prefix>/bin (null-terminated).
        // Points at the bin/ directory so blit_exe() returns the launcher path.
        let mut wrapper_env = [0u8; MAX_PATH];
        let env_pfx = WRAPPER_PREFIX.len();
        if env_pfx + bin_dir_len + 1 >= MAX_PATH {
            fatal(b"blit: path too long\n");
        }
        copy_bytes(WRAPPER_PREFIX, env_pfx, &mut wrapper_env, 0);
        copy_bytes(&exe_buf, bin_dir_len, &mut wrapper_env, env_pfx);
        wrapper_env[env_pfx + bin_dir_len] = 0;

        // Build new argv: [loader, real_bin, original_argv[1..], NULL].
        // argc + 2: one extra slot for loader (replacing argv[0]) plus the
        // real binary inserted before the original args, plus NULL terminator.
        let new_argc = argc as usize + 2;
        let new_argv = alloc_ptrs(new_argc);
        *new_argv.add(0) = loader.as_ptr().cast();
        *new_argv.add(1) = real_bin.as_ptr().cast();
        let mut i: usize = 1;
        while i < argc as usize {
            *new_argv.add(i + 1) = *argv.add(i);
            i += 1;
        }
        *new_argv.add(argc as usize + 1) = ptr::null();

        // Build new envp: existing env + BLIT_WRAPPER_DIR, NULL-terminated.
        let envc = count_null_terminated(envp);
        let new_envp = alloc_ptrs(envc + 2); // +1 for BLIT_WRAPPER_DIR, +1 for NULL
        let mut dst: usize = 0;
        let mut replaced = false;
        let mut j: usize = 0;
        while j < envc {
            let entry = *envp.add(j);
            if starts_with_cstr(entry, WRAPPER_PREFIX) {
                *new_envp.add(dst) = wrapper_env.as_ptr().cast();
                replaced = true;
            } else {
                *new_envp.add(dst) = entry;
            }
            dst += 1;
            j += 1;
        }
        if !replaced {
            *new_envp.add(dst) = wrapper_env.as_ptr().cast();
            dst += 1;
        }
        *new_envp.add(dst) = ptr::null();

        libc::execve(loader.as_ptr().cast(), new_argv, new_envp);

        // execve only returns on failure.
        fatal(b"blit: exec failed\n");
    }
}

unsafe fn fatal(msg: &[u8]) -> ! {
    unsafe {
        libc::write(2, msg.as_ptr().cast(), msg.len());
        libc::_exit(1)
    }
}

fn copy_bytes(src: &[u8], len: usize, dst: &mut [u8], offset: usize) {
    let mut i = 0;
    while i < len {
        dst[offset + i] = src[i];
        i += 1;
    }
}

fn slice_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

unsafe fn starts_with_cstr(cstr: *const libc::c_char, prefix: &[u8]) -> bool {
    unsafe {
        let mut i = 0;
        while i < prefix.len() {
            if *cstr.add(i) as u8 != prefix[i] {
                return false;
            }
            i += 1;
        }
        true
    }
}
