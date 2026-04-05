# Unsafe code in blit

Unsafe code is confined to four crates (`server`, `cli`, `browser`, `compositor`) that need direct POSIX terminal/process APIs, foreign function declarations, or graphics APIs. The remaining crates contain zero `unsafe` blocks.

This document focuses on the non-obvious parts — the invariants that are easy to break.

## The `waitpid` race

The server has two independent call sites for `waitpid`:

1. **Per-PTY cleanup** (`cleanup_pty`) — sends `SIGHUP`, closes the master fd, then calls `waitpid(child_pid, WNOHANG)` for the specific child.
2. **Background zombie reaper** — calls `waitpid(-1, WNOHANG)` every 5 seconds to sweep any zombies.

These intentionally race. The reaper can collect a child before `cleanup_pty` gets to it — that's fine because `cleanup_pty` uses `WNOHANG` and tolerates `ECHILD`. Neither call site blocks. If you change either to use blocking `waitpid`, you'll deadlock.

## The fork/exec sequence

`spawn_pty` in [`crates/server/src/pty/pty_unix.rs`](crates/server/src/pty/pty_unix.rs) runs a specific post-fork sequence in the child that must not be reordered:

```
child: close(master) -> setsid() -> ioctl(TIOCSCTTY) -> dup2(slave, 0/1/2) -> close(slave)
     -> close_fds_except(3) -> signal(SIGPIPE, SIG_DFL) -> chdir() -> execve(envp)
```

`setsid` must come before `TIOCSCTTY` (can't set a controlling terminal without being a session leader). `dup2` must come before closing the slave fd (otherwise stdio points at nothing). `close(master)` happens first in the child because the child must not hold the master fd — if it did, reads from master in the parent would never see EOF when the child exits.

`close_fds_except(3)` closes all inherited parent fds (IPC listener, other PTY masters, epoll fd, compositor fds) to prevent the child from accessing other sessions. `signal(SIGPIPE, SIG_DFL)` resets SIGPIPE handling since the Rust runtime sets it to SIG_IGN, which breaks piped commands.

The child's environment is built before `fork()` via `build_child_env()` and passed to `execve()` — this avoids calling `std::env::set_var`/`remove_var` after fork in a multi-threaded process (not async-signal-safe per POSIX). PATH resolution is also done before fork via `resolve_in_path()`.

On the parent side, `close(slave)` is equally important — the parent must not hold the slave fd, or the master won't get a hangup when the child exits.

## fd-passing via `recvmsg`

The server uses `SCM_RIGHTS` ancillary data to receive client connection fds over a Unix socket (from systemd socket activation or the gateway). The `recv_fd` function calls `recvmsg` with a manually constructed `msghdr` and `cmsghdr`, then extracts the fd from the control message.

The received fd is immediately wrapped in `from_raw_fd` to transfer ownership to Rust. If the `from_raw_fd` call were skipped or the fd were used after being wrapped, you'd get a double-close.

## Why `libc::write` instead of `std::io`

The `cli` crate uses raw `libc::write(STDOUT_FILENO, ...)` in two places instead of `std::io::stdout()`:

1. **`Drop` impls** that emit terminal reset sequences — `stdout().write()` takes a mutex lock, which can deadlock if the process is unwinding from a panic that already holds the lock.
2. **`write_all_stdout`** in the frame output hot path — avoids the lock overhead on every frame.

## Environment variable mutation in the child

`std::env::set_var` and `std::env::remove_var` are `unsafe` as of Rust edition 2024 because they mutate global process state and are not thread-safe. The server now builds the child environment before `fork()` via `build_child_env()` and passes it to `execve()`, avoiding post-fork `set_var`/`remove_var` entirely.

## macOS-specific FFI

Two macOS-only calls that aren't in the `libc` crate:

- **`proc_pidinfo(PROC_PIDVNODEPATHINFO)`** — gets the child process's working directory by reinterpreting a raw byte buffer as `proc_vnodepathinfo`. The pointer cast is sound only if the buffer is large enough and the syscall succeeds (checked via return value).
- **`pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE)`** — declared as a local `unsafe extern "C"` function. Bumps thread priority so the frame scheduler gets lower latency. Harmless if it fails.

## WASM FFI in `browser`

`crates/browser/src/lib.rs` declares an `unsafe extern "C"` block for JavaScript helper functions injected via `#[wasm_bindgen(inline_js)]`. The functions (`blitFillTextCodePoint`, `blitFillTextStretched`, `blitFillText`, `blitMeasureMaxOverhang`) are called from safe Rust through wasm-bindgen's generated bindings. The `unsafe` marker is required by edition 2024 for all `extern` blocks.

## Dmabuf pixel reads in `compositor`

`read_dmabuf_pixels` in [`crates/compositor/src/imp.rs`](crates/compositor/src/imp.rs) calls `dmabuf.map_plane()` to get a raw pointer and length, then uses `std::slice::from_raw_parts(ptr, len)` to create byte slices from the mapped memory regions.

The invariants: `map_plane` must return a valid mapping whose `ptr()` is non-null and `length()` accurately describes the mapped region. Each mapping is bracketed by `sync_plane(START|READ)` / `sync_plane(END|READ)` to ensure cache coherence with the GPU. The slices must not outlive the `DmabufMapping` objects — currently they don't because both stay local to the helper closure that reads each plane.

The SHM path in `commit()` uses the same pattern (`std::slice::from_raw_parts`) via `with_buffer_contents`, which smithay invokes with a pointer to the shared memory pool. The safety contract is the same: the slice is only used within the callback closure.

`spawn_compositor` calls `std::env::set_var("XDG_RUNTIME_DIR", …)` inside an `unsafe` block when the variable is unset (e.g. macOS). This is called once at the start of the compositor thread before any Wayland socket is created. The invariant: no other thread reads `XDG_RUNTIME_DIR` concurrently at that point; the variable is only consumed by `ListeningSocketSource::new_auto` immediately after.

## GPU encoder dlopen in `server`

`crates/server/src/gpu_libs.rs` loads GPU driver libraries at runtime via `dlopen`/`dlsym` (VA-API: `libva.so.2`, `libva-drm.so.2`; NVENC: `libcuda.so.1`, `libnvidia-encode.so.1`). Function pointers are resolved once via `OnceLock` and stored in static `Send + Sync` structs. The invariants: every `dlsym` result is null-checked before transmuting to a typed function pointer; the `DynLib` handle must outlive all resolved pointers (enforced by storing `_lib: DynLib` in each `*Fns` struct); and the function signatures must exactly match the C driver ABI.

## NVENC direct encoder in `server`

`crates/server/src/nvenc_encode.rs` drives NVIDIA's NVENC hardware encoder through the function pointer table returned by `NvEncodeAPICreateInstance`. All NVENC structs are opaque byte arrays sized to match `nv-codec-headers` 12.1.14.0 — fields are written at verified offsets rather than through `#[repr(C)]` struct translation, because the SDK structs contain large reserved arrays and padding that change between API versions.

The `NvEncFunctionList` struct must match the SDK's `NV_ENCODE_API_FUNCTION_LIST` layout exactly — each function pointer slot corresponds to a specific API entry point. A 64-slot `_future` padding array absorbs new entries added by newer SDK versions. The struct version tags embed the API version (12.1) and a type version via `NVENCAPI_STRUCT_VERSION(v) = NVENCAPI_VERSION | (v << 16) | (0x7 << 28)` — some structs additionally set bit 31. Getting any of these wrong produces `NV_ENC_ERR_INVALID_VERSION` (error 15).

The CUDA context (`cuCtxCreate_v2`) is created per encoder instance and must remain alive for the encoder's lifetime (stored as `_cuda_ctx`). Input pixels are written into NVENC-allocated buffers via `nvEncLockInputBuffer`/`nvEncUnlockInputBuffer` using raw pointer arithmetic — the `pitch` returned by the lock must be respected, not the logical width.

## VA-API direct encoder in `server`

`crates/server/src/vaapi_encode.rs` implements H.264 encoding via VA-API's C interface loaded through `gpu_libs.rs`. Like the NVENC encoder, all VA-API parameter buffer structs (SPS, PPS, slice) are accessed as raw byte arrays at verified offsets rather than `#[repr(C)]` struct translation, since the VA-API headers contain complex bitfields.

Surface pixel upload uses `vaDeriveImage` + `vaMapBuffer` to get a raw pointer into driver-owned memory. Writes into this mapping use the image-reported `pitches` (not packed width). The mapping must be unmapped (`vaUnmapBuffer`) and the derived image destroyed (`vaDestroyImage`) before the surface is submitted for encoding. Violating this ordering corrupts the driver's internal state.

## DMA-BUF CPU fallback in `server`

`crates/server/src/surface_encoder.rs` reads DMA-BUF pixel data via `mmap` + `DMA_BUF_IOCTL_SYNC` when no zero-copy GPU import path is available. The `mmap` size is determined by `lseek(SEEK_END)` on the fd. The sync start/end brackets ensure cache coherence with the GPU. The mapped slice must not outlive the `munmap` call.

## Audit checklist

- **fd leaks** — every `openpty`/`dup2`/`close` path must close all fds on failure, including in the child after a failed `execvp` (which falls through to `_exit`).
- **`waitpid` semantics** — both call sites must use `WNOHANG` and handle the case where the other already reaped the child.
- **`Drop` signal safety** — no allocations, no locks, no `stdout()` — use `libc::write` directly.
- **macOS guards** — `proc_pidinfo` and `pthread_set_qos_class_self_np` must stay behind `#[cfg(target_os = "macos")]`.
- **WASM boundary** — `crates/browser/` targets `wasm32-unknown-unknown` and must never import `libc` or `std::os::unix`.
- **NVENC struct sizes** — every NVENC struct must be sized to match `nv-codec-headers` 12.1 exactly. The driver validates sizes via version tags — an oversized or undersized struct silently fails with error 15.
- **NVENC function list slots** — the function pointer index in `NvEncFunctionList` must match the SDK header's field order. A wrong slot calls a different function with incompatible arguments → undefined behavior.
- **VA-API byte offsets** — the SPS/PPS/slice parameter buffer offsets are hand-verified against `va_enc_h264.h`. If VA-API bumps struct versions, these must be re-verified.
- **dlopen lifetime** — the `DynLib` handle in each `*Fns` struct must not be dropped while function pointers are still callable. The `OnceLock<Option<*Fns>>` pattern ensures this for `'static`.
