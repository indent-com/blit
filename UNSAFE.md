# Unsafe code in blit

Unsafe code is confined to four crates (`server`, `cli`, `browser`, `compositor`) that need direct POSIX terminal/process APIs, foreign function declarations, or graphics APIs. The remaining crates contain zero `unsafe` blocks.

This document focuses on the non-obvious parts — the invariants that are easy to break.

## The `waitpid` race

The server has two independent call sites for `waitpid`:

1. **Per-PTY cleanup** (`cleanup_pty`) — sends `SIGHUP`, closes the master fd, then calls `waitpid(child_pid, WNOHANG)` for the specific child.
2. **Background zombie reaper** — calls `waitpid(-1, WNOHANG)` every 5 seconds to sweep any zombies.

These intentionally race. The reaper can collect a child before `cleanup_pty` gets to it — that's fine because `cleanup_pty` uses `WNOHANG` and tolerates `ECHILD`. Neither call site blocks. If you change either to use blocking `waitpid`, you'll deadlock.

## The fork/exec sequence

`spawn_pty` in [`crates/server/src/lib.rs`](crates/server/src/lib.rs) runs a specific post-fork sequence in the child that must not be reordered:

```
child: setsid() -> ioctl(TIOCSCTTY) -> dup2(slave, 0/1/2) -> close(slave) -> chdir() -> execvp()
```

`setsid` must come before `TIOCSCTTY` (can't set a controlling terminal without being a session leader). `dup2` must come before closing the slave fd (otherwise stdio points at nothing). `close(master)` happens first in the child because the child must not hold the master fd — if it did, reads from master in the parent would never see EOF when the child exits.

On the parent side, `close(slave)` is equally important — the parent must not hold the slave fd, or the master won't get a hangup when the child exits.

## fd-passing via `recvmsg`

The server uses `SCM_RIGHTS` ancillary data to receive client connection fds over a Unix socket (from systemd socket activation or the gateway). The `recv_fd` function calls `recvmsg` with a manually constructed `msghdr` and `cmsghdr`, then extracts the fd from the control message.

The received fd is immediately wrapped in `from_raw_fd` to transfer ownership to Rust. If the `from_raw_fd` call were skipped or the fd were used after being wrapped, you'd get a double-close.

## Why `libc::write` instead of `std::io`

The `cli` crate uses raw `libc::write(STDOUT_FILENO, ...)` in two places instead of `std::io::stdout()`:

1. **`Drop` impls** that emit terminal reset sequences — `stdout().write()` takes a mutex lock, which can deadlock if the process is unwinding from a panic that already holds the lock.
2. **`write_all_stdout`** in the frame output hot path — avoids the lock overhead on every frame.

## Environment variable mutation in the child

`std::env::set_var` and `std::env::remove_var` are `unsafe` as of Rust edition 2024 because they mutate global process state and are not thread-safe. The server calls them in two `spawn_pty` functions, immediately after `fork()`, to set `TERM`/`COLORTERM`, clear `COLUMNS`/`LINES`, and strip `BLIT_*` variables before `execvp`. This is sound because the child process is single-threaded after `fork`.

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

## VA-API frame setup in `server`

`crates/server/src/surface_encoder.rs` uses FFmpeg's raw C API to configure `AVHWFramesContext`, allocate VA-API-backed frames, and upload CPU-owned NV12 frames into those surfaces before calling `h264_vaapi`. The invariants: the server only creates encoders for even, non-zero dimensions; `av_buffer_ref` must copy the frames context into `AVCodecContext::hw_frames_ctx` before the temporary `AVBufferRef` is unreffed; and every raw pointer (`AVCodecContext`, `AVHWFramesContext`, `AVFrame`) is only dereferenced while the owning Rust wrapper or local buffer ref is still alive.

The software upload path writes directly into FFmpeg-owned frame planes using the frame-reported `linesize` values, not packed-width assumptions. If that code ever ignores `linesize`, writes past `plane_height * stride`, or keeps slices alive across another FFmpeg call that reallocates the frame, it becomes immediate memory corruption.

## Audit checklist

- **fd leaks** — every `openpty`/`dup2`/`close` path must close all fds on failure, including in the child after a failed `execvp` (which falls through to `_exit`).
- **`waitpid` semantics** — both call sites must use `WNOHANG` and handle the case where the other already reaped the child.
- **`Drop` signal safety** — no allocations, no locks, no `stdout()` — use `libc::write` directly.
- **macOS guards** — `proc_pidinfo` and `pthread_set_qos_class_self_np` must stay behind `#[cfg(target_os = "macos")]`.
- **WASM boundary** — `crates/browser/` targets `wasm32-unknown-unknown` and must never import `libc` or `std::os::unix`.
