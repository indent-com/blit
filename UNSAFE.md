# Unsafe code in blit

Unsafe code is confined to three crates (`server`, `browser`, `compositor`) that need direct POSIX/Win32 terminal/process APIs, foreign function declarations, or graphics APIs. The remaining crates contain zero `unsafe` blocks.

This document focuses on the non-obvious parts — the invariants that are easy to break.

## The `waitpid` race

The server has two independent call sites for `waitpid`:

1. **Per-PTY cleanup** (`cleanup_pty`) — sends `SIGHUP`, closes the master fd, then calls `waitpid(child_pid, WNOHANG)` for the specific child.
2. **Background zombie reaper** — calls `waitpid(-1, WNOHANG)` every 5 seconds to sweep any zombies.

These intentionally race. The reaper can collect a child before `cleanup_pty` gets to it — that's fine because `cleanup_pty` uses `WNOHANG` and tolerates `ECHILD`. Neither call site blocks. If you change either to use blocking `waitpid`, you'll deadlock.

## The fork/exec sequence

`spawn_pty` in [`crates/server/src/pty/pty_unix.rs`](crates/server/src/pty/pty_unix.rs) runs a specific post-fork sequence in the child that must not be reordered:

```
child: close(master) -> setsid() -> ioctl(TIOCSCTTY) -> dup2(slave, 0/1/2)
     -> close(slave) [if slave > 2] -> close_fds_except(3) -> signal(SIGPIPE, SIG_DFL)
     -> set_qos_user_interactive() [macOS] -> chdir() -> execve(envp)
```

`setsid` must come before `TIOCSCTTY` (can't set a controlling terminal without being a session leader). `dup2` must come before closing the slave fd (otherwise stdio points at nothing). The `close(slave)` is conditional on `slave > 2` — if slave happens to be fd 0/1/2, the `dup2` calls already aliased it. `close(master)` happens first in the child because the child must not hold the master fd — if it did, reads from master in the parent would never see EOF when the child exits.

`close_fds_except(3)` closes all inherited parent fds (IPC listener, other PTY masters, epoll fd, compositor fds) to prevent the child from accessing other sessions. `signal(SIGPIPE, SIG_DFL)` resets SIGPIPE handling since the Rust runtime sets it to SIG_IGN, which breaks piped commands.

The child's environment is built before `fork()` via `build_child_env()` and passed to `execve()` — this avoids calling `std::env::set_var`/`remove_var` after fork in a multi-threaded process (not async-signal-safe per POSIX). PATH resolution is also done before fork via `resolve_in_path()`.

On the parent side, `close(slave)` is equally important — the parent must not hold the slave fd, or the master won't get a hangup when the child exits.

## Compositor child spawn

`spawn_compositor_child` in [`crates/server/src/lib.rs`](crates/server/src/lib.rs) is a simpler fork/exec path used to launch Wayland GUI commands (e.g. `foot`). The child calls `chdir()`, mutates the environment (`set_var`/`remove_var` for `XDG_RUNTIME_DIR`, `WAYLAND_DISPLAY`, `DISPLAY`), then `execvp`. Unlike the PTY spawn path, it does not call `setsid`, `TIOCSCTTY`, or `close_fds_except` — the child inherits the parent's fd table. This is acceptable because compositor children don't need a controlling terminal and don't interact with PTY sessions.

## Windows ConPTY in `server`

[`crates/server/src/pty/pty_windows.rs`](crates/server/src/pty/pty_windows.rs) implements PTY support on Windows via the ConPTY API. The unsafe surface covers:

- **`CreatePseudoConsole`/`ClosePseudoConsole`** — creating and destroying the pseudo console. The console handle is stored in `PtyHandle` and must outlive all pipe I/O.
- **`CreateProcessW`** — launching the child process attached to the ConPTY via `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`. The attribute list is initialized with `InitializeProcThreadAttributeList` + `UpdateProcThreadAttribute`.
- **`ReadFile`/`WriteFile`** — blocking reads from the output pipe (in a dedicated reader thread) and writes to the input pipe. The pipe handles come from `CreatePipe` and must be closed in the correct order — the parent's end of the input pipe is closed by `ClosePseudoConsole`, and the child's inherited ends are closed immediately after `CreateProcessW`.
- **`unsafe impl Send + Sync`** for `PtyHandle`, `PtyWriteTarget`, and `SendHandle` — these wrap raw Windows `HANDLE` values that are safe to use from any thread once created.

## fd-passing via `recvmsg`

The server uses `SCM_RIGHTS` ancillary data to receive client connection fds over a Unix socket (from systemd socket activation or the gateway). The `recv_fd` function calls `recvmsg` with a manually constructed `msghdr` and `cmsghdr`, then extracts the fd from the control message.

The received fd is immediately wrapped in `from_raw_fd` to transfer ownership to Rust. If the `from_raw_fd` call were skipped or the fd were used after being wrapped, you'd get a double-close.

## Environment variable mutation in the child

`std::env::set_var` and `std::env::remove_var` are `unsafe` as of Rust edition 2024 because they mutate global process state and are not thread-safe. The PTY spawn path builds the child environment before `fork()` via `build_child_env()` and passes it to `execve()`, avoiding post-fork `set_var`/`remove_var`.

The compositor child path (`spawn_compositor_child` in `crates/server/src/lib.rs`) still calls `set_var`/`remove_var` after fork to set `XDG_RUNTIME_DIR`, `WAYLAND_DISPLAY`, and remove `DISPLAY`. This is tolerated because the child calls `execvp` immediately after, replacing the process image, and no other thread exists in the child after `fork()`.

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

The calloop event loop calls `unsafe { display.get_mut() }` to obtain a mutable reference to the Wayland `Display` for `dispatch_clients` and `flush_clients`. This is sound because the `Generic` source callback has exclusive access to the fd at that point — no other calloop callback touches the display concurrently.

## GPU encoder dlopen in `server`

`crates/server/src/gpu_libs.rs` loads GPU driver libraries at runtime via `dlopen`/`dlsym` with fallback names (VA-API: `libva.so.2`/`libva.so`, `libva-drm.so.2`/`libva-drm.so`; NVENC: `libcuda.so.1`/`libcuda.so`, `libnvidia-encode.so.1`/`libnvidia-encode.so`). Function pointers are resolved once via `OnceLock` and stored in static `Send + Sync` structs. The `DynLib` wrapper calls `dlclose` in its `Drop` impl. The invariants: every `dlsym` result is null-checked before transmuting to a typed function pointer; the `DynLib` handle must outlive all resolved pointers (enforced by storing `_lib: DynLib` in each `*Fns` struct); and the function signatures must exactly match the C driver ABI.

## NVENC direct encoder in `server`

`crates/server/src/nvenc_encode.rs` drives NVIDIA's NVENC hardware encoder through the function pointer table returned by `NvEncodeAPICreateInstance`. All NVENC structs are opaque byte arrays sized to match `nv-codec-headers` 12.1.14.0 — fields are written at verified offsets rather than through `#[repr(C)]` struct translation, because the SDK structs contain large reserved arrays and padding that change between API versions.

The `NvEncFunctionList` struct must match the SDK's `NV_ENCODE_API_FUNCTION_LIST` layout exactly — each function pointer slot corresponds to a specific API entry point. A 64-slot `_future` padding array absorbs new entries added by newer SDK versions. The struct version tags embed the API version (12.1) and a type version via `NVENCAPI_STRUCT_VERSION(v) = NVENCAPI_VERSION | (v << 16) | (0x7 << 28)` — some structs additionally set bit 31. Getting any of these wrong produces `NV_ENC_ERR_INVALID_VERSION` (error 15).

The CUDA context (`cuCtxCreate_v2`) is created per encoder instance and must remain alive for the encoder's lifetime (stored as `_cuda_ctx`). Input pixels are written into NVENC-allocated buffers via `nvEncLockInputBuffer`/`nvEncUnlockInputBuffer` using raw pointer arithmetic — the `pitch` returned by the lock must be respected, not the logical width.

## VA-API direct encoder in `server`

`crates/server/src/vaapi_encode.rs` implements H.264 and H.265 (HEVC) encoding via VA-API's C interface loaded through `gpu_libs.rs`. Both encoders (`VaapiDirectEncoder` for H.264, `VaapiHevcEncoder` for H.265) follow the same pattern: all VA-API parameter buffer structs (SPS, PPS, slice) are accessed as raw byte arrays at verified offsets rather than `#[repr(C)]` struct translation, since the VA-API headers contain complex bitfields.

Surface pixel upload uses `vaDeriveImage` + `vaMapBuffer` to get a raw pointer into driver-owned memory. Writes into this mapping use the image-reported `pitches` (not packed width). The mapping must be unmapped (`vaUnmapBuffer`) and the derived image destroyed (`vaDestroyImage`) before the surface is submitted for encoding. Violating this ordering corrupts the driver's internal state.

Encoded bitstream readback walks a linked list of `VACodedBufferSegment` structs via raw pointer arithmetic at hardcoded offsets (`CBS_SIZE_OFF`, `CBS_BUF_OFF`, `CBS_NEXT_OFF`), reading each segment's data pointer and size to copy out the NAL units.

## DMA-BUF CPU fallback in `server`

`crates/server/src/surface_encoder.rs` reads DMA-BUF pixel data via `mmap` + `DMA_BUF_IOCTL_SYNC` when no zero-copy GPU import path is available. The `mmap` size is determined by `lseek(SEEK_END)` on the fd. The sync start/end brackets ensure cache coherence with the GPU. The mapped slice must not outlive the `munmap` call.

## DMA-BUF zero-copy color conversion in `server`

`crates/server/src/dmabuf_zerocopy.rs` implements VA-API Video Processing Pipeline (VPP) for zero-copy DMA-BUF color space conversion (BGRA to NV12). It loads a separate set of VA-API VPP function pointers (declared as local `unsafe extern "C" fn` types) and calls them through the same `gpu_libs.rs` mechanism. The `VaapiVppContext` struct holds the VA display, config, context, and surfaces — all must be destroyed in the correct order in `Drop`. The `convert_dmabuf` method imports a client DMA-BUF as a BGRA `VASurface` and runs VPP conversion to an NV12 output surface. `unsafe impl Send` is required because the VA-API handles are raw pointers.

## Audit checklist

- **fd leaks** — every `openpty`/`dup2`/`close` path must close all fds on failure, including in the child after a failed `execvp` (which falls through to `_exit`).
- **`waitpid` semantics** — both call sites must use `WNOHANG` and handle the case where the other already reaped the child.
- **macOS guards** — `proc_pidinfo` and `pthread_set_qos_class_self_np` must stay behind `#[cfg(target_os = "macos")]`.
- **WASM boundary** — `crates/browser/` targets `wasm32-unknown-unknown` and must never import `libc` or `std::os::unix`.
- **NVENC struct sizes** — every NVENC struct must be sized to match `nv-codec-headers` 12.1 exactly. The driver validates sizes via version tags — an oversized or undersized struct silently fails with error 15.
- **NVENC function list slots** — the function pointer index in `NvEncFunctionList` must match the SDK header's field order. A wrong slot calls a different function with incompatible arguments → undefined behavior.
- **VA-API byte offsets** — the SPS/PPS/slice parameter buffer offsets are hand-verified against `va_enc_h264.h` and `va_enc_hevc.h`. If VA-API bumps struct versions, these must be re-verified.
- **dlopen lifetime** — the `DynLib` handle in each `*Fns` struct must not be dropped while function pointers are still callable. The `OnceLock<Option<*Fns>>` pattern ensures this for `'static`.
- **Windows handle leaks** — every `CreatePipe`/`CreatePseudoConsole`/`CreateProcessW` path must close all handles on failure. `CloseHandle(pi.hThread)` must be called after `CreateProcessW` since the thread handle is unused.
- **VPP destroy order** — `dmabuf_zerocopy.rs` must destroy surfaces before context, and context before config, matching the VA-API object hierarchy.
- **VA-API encoder VPP lifetime** — `VaapiDirectEncoder` and `VaapiHevcEncoder` in `vaapi_encode.rs` each own an `Option<VppContext>` that shares the parent encoder's VA display handle. The VPP context must be dropped (`self.vpp.take()`) **before** `vaTerminate(self.display)` in the encoder's `Drop` impl — otherwise `VppContext::drop` calls `vaDestroyContext` on a terminated display, causing a SIGSEGV. Rust's default field drop order does not guarantee this.
