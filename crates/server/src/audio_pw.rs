//! Direct libpipewire-0.3 client via runtime `dlopen`: replaces the
//! pw-cat subprocess + pipe read pipeline with an in-process capture
//! stream that delivers PCM frames straight to the Opus encoder.
//!
//! Loaded at runtime (no link-time dependency on libpipewire) so the
//! server binary still starts on systems without PipeWire installed —
//! on those systems audio stays disabled, same behaviour as the
//! missing-pw-cat fallback path it replaces.  Direct integration means
//! we set the PipeWire quantum ourselves, so capture cadence is ours
//! to control — no 100 ms pw-cat buffering jitter.

#![cfg(target_os = "linux")]

use libc::{RTLD_LAZY, RTLD_LOCAL, dlopen, dlsym};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

// ── Opaque handles ────────────────────────────────────────────────────

#[repr(C)]
struct PwThreadLoop {
    _priv: [u8; 0],
}
#[repr(C)]
struct PwLoop {
    _priv: [u8; 0],
}
#[repr(C)]
struct PwStream {
    _priv: [u8; 0],
}
#[repr(C)]
struct PwProperties {
    _priv: [u8; 0],
}

// ── Buffer structs (must match C layout exactly) ──────────────────────

#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
    user_data: *mut c_void,
    size: u64,
    requested: u64,
    /// Capture-cycle nanoseconds (since PW 1.0.5).  Unused here but kept
    /// so the struct matches libpipewire's layout — pw_buffer is
    /// allocated by the library so ABI drift here would silently corrupt
    /// subsequent fields or over-read.
    time: u64,
}

#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut c_void,
    datas: *mut SpaData,
}

#[repr(C)]
struct SpaData {
    type_: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}

#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}

/// `pw_stream_events` vtable — version 2 of the interface.  Despite the
/// `PW_VERSION_STREAM_EVENTS` macro only being 2, libpipewire reads two
/// additional methods (`command` since 0.3.39 at min-version 1 and
/// `trigger_done` since 0.3.40 at min-version 2) behind the standard
/// `spa_callbacks_call` version-check gate.  A shorter struct **will
/// SEGV**: libpipewire indexes past our struct into adjacent bytes and
/// invokes them as a function pointer.  Include every field for the
/// declared version; rustc's niche optimisation makes `Option<fn>::None`
/// equal to a NULL pointer, matching a zero-initialised C struct.
#[repr(C)]
struct PwStreamEvents {
    version: u32,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, i32, i32, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut PwBuffer)>,
    process: Option<unsafe extern "C" fn(*mut c_void)>,
    drained: Option<unsafe extern "C" fn(*mut c_void)>,
    command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    trigger_done: Option<unsafe extern "C" fn(*mut c_void)>,
}

const PW_VERSION_STREAM_EVENTS: u32 = 2;

// ── PipeWire constants ────────────────────────────────────────────────

const PW_DIRECTION_INPUT: i32 = 0;
const PW_ID_ANY: u32 = u32::MAX;
const PW_STREAM_FLAG_AUTOCONNECT: u32 = 1 << 0;
const PW_STREAM_FLAG_MAP_BUFFERS: u32 = 1 << 2;
const PW_STREAM_FLAG_RT_PROCESS: u32 = 1 << 4;

// ── SPA POD constants ─────────────────────────────────────────────────

const SPA_TYPE_ID: u32 = 3;
const SPA_TYPE_INT: u32 = 4;
const SPA_TYPE_OBJECT: u32 = 15;
const SPA_TYPE_OBJECT_FORMAT: u32 = 0x40003;
const SPA_PARAM_ENUM_FORMAT: u32 = 3;
const SPA_FORMAT_MEDIA_TYPE: u32 = 1;
const SPA_FORMAT_MEDIA_SUBTYPE: u32 = 2;
const SPA_FORMAT_AUDIO_FORMAT: u32 = 0x10001;
const SPA_FORMAT_AUDIO_RATE: u32 = 0x10003;
const SPA_FORMAT_AUDIO_CHANNELS: u32 = 0x10004;
const SPA_MEDIA_TYPE_AUDIO: u32 = 1;
const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;
const SPA_AUDIO_FORMAT_F32_LE: u32 = 283;

// ── Resolved symbols ──────────────────────────────────────────────────

type FnPwInit = unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char);
type FnPwDeinit = unsafe extern "C" fn();
type FnPwThreadLoopNew = unsafe extern "C" fn(*const c_char, *const c_void) -> *mut PwThreadLoop;
type FnPwThreadLoopDestroy = unsafe extern "C" fn(*mut PwThreadLoop);
type FnPwThreadLoopStart = unsafe extern "C" fn(*mut PwThreadLoop) -> c_int;
type FnPwThreadLoopStop = unsafe extern "C" fn(*mut PwThreadLoop);
type FnPwThreadLoopGetLoop = unsafe extern "C" fn(*mut PwThreadLoop) -> *mut PwLoop;
type FnPwStreamNewSimple = unsafe extern "C" fn(
    *mut PwLoop,
    *const c_char,
    *mut PwProperties,
    *const PwStreamEvents,
    *mut c_void,
) -> *mut PwStream;
type FnPwStreamDestroy = unsafe extern "C" fn(*mut PwStream);
type FnPwStreamConnect =
    unsafe extern "C" fn(*mut PwStream, i32, u32, u32, *mut *const c_void, u32) -> c_int;
type FnPwStreamDisconnect = unsafe extern "C" fn(*mut PwStream) -> c_int;
type FnPwStreamDequeueBuffer = unsafe extern "C" fn(*mut PwStream) -> *mut PwBuffer;
type FnPwStreamQueueBuffer = unsafe extern "C" fn(*mut PwStream, *mut PwBuffer) -> c_int;
type FnPwPropertiesNew = unsafe extern "C" fn(*const c_char) -> *mut PwProperties;
type FnPwPropertiesSet =
    unsafe extern "C" fn(*mut PwProperties, *const c_char, *const c_char) -> c_int;

struct Syms {
    pw_init: FnPwInit,
    pw_thread_loop_new: FnPwThreadLoopNew,
    pw_thread_loop_destroy: FnPwThreadLoopDestroy,
    pw_thread_loop_start: FnPwThreadLoopStart,
    pw_thread_loop_stop: FnPwThreadLoopStop,
    pw_thread_loop_get_loop: FnPwThreadLoopGetLoop,
    pw_stream_new_simple: FnPwStreamNewSimple,
    pw_stream_destroy: FnPwStreamDestroy,
    pw_stream_connect: FnPwStreamConnect,
    pw_stream_disconnect: FnPwStreamDisconnect,
    pw_stream_dequeue_buffer: FnPwStreamDequeueBuffer,
    pw_stream_queue_buffer: FnPwStreamQueueBuffer,
    pw_properties_new: FnPwPropertiesNew,
    pw_properties_set: FnPwPropertiesSet,
    /// Kept for completeness; called on process shutdown (never, in
    /// practice — the process is exiting anyway).  Allowing dead_code
    /// keeps the symbol table symmetric with the C API.
    #[allow(dead_code)]
    pw_deinit: FnPwDeinit,
}

// SAFETY: these are pure function pointers — no interior state.
unsafe impl Send for Syms {}
unsafe impl Sync for Syms {}

/// Last dlopen/dlsym error, if the load failed.  Exposed for diagnostic
/// messages so operators running on distros where libpipewire-0.3.so.0
/// isn't in the default loader path (Nix, Alpine without musl variant,
/// etc.) see an actionable error rather than "audio disabled".
static LOAD_ERROR: OnceLock<String> = OnceLock::new();

/// Error message from the last attempt to load libpipewire.  Empty
/// string if the load hasn't been attempted yet or succeeded.
pub fn load_error() -> &'static str {
    LOAD_ERROR.get().map(String::as_str).unwrap_or("")
}

fn record_dlerror(context: &str) {
    unsafe {
        let e = libc::dlerror();
        let detail = if e.is_null() {
            String::from("(no dlerror)")
        } else {
            CStr::from_ptr(e).to_string_lossy().into_owned()
        };
        let _ = LOAD_ERROR.set(format!("{context}: {detail}"));
    }
}

/// Returns the resolved PipeWire symbols, loading + `pw_init`-ing the
/// library on first call.  Returns `None` if libpipewire-0.3.so.0 is
/// not installed / not resolvable via the dynamic linker, mirroring
/// the pre-existing missing-binary fallback.
fn syms() -> Option<&'static Syms> {
    static CACHE: OnceLock<Option<Syms>> = OnceLock::new();
    CACHE
        .get_or_init(|| unsafe {
            // Try the SONAME first, then fall back to the unversioned
            // symlink — distributions/devel packages vary in which one
            // is available without the full `-dev` package.
            let candidates = [c"libpipewire-0.3.so.0", c"libpipewire-0.3.so"];
            let mut handle = ptr::null_mut();
            for name in candidates {
                handle = dlopen(name.as_ptr(), RTLD_LAZY | RTLD_LOCAL);
                if !handle.is_null() {
                    break;
                }
            }
            if handle.is_null() {
                record_dlerror("dlopen libpipewire-0.3.so.0 failed (check LD_LIBRARY_PATH)");
                return None;
            }

            // Resolve a single symbol, returning None if any fail.  The
            // library handle is never dlclose'd — intentional: holding
            // it open for the process lifetime avoids any risk of
            // dangling function pointers, and we'd only unload on
            // shutdown anyway.
            macro_rules! sym {
                ($name:literal, $ty:ty) => {{
                    let cname = CString::new($name).ok()?;
                    let ptr = dlsym(handle, cname.as_ptr());
                    if ptr.is_null() {
                        record_dlerror(&format!("dlsym {} failed", $name));
                        return None;
                    }
                    std::mem::transmute::<*mut c_void, $ty>(ptr)
                }};
            }

            let syms = Syms {
                pw_init: sym!("pw_init", FnPwInit),
                pw_deinit: sym!("pw_deinit", FnPwDeinit),
                pw_thread_loop_new: sym!("pw_thread_loop_new", FnPwThreadLoopNew),
                pw_thread_loop_destroy: sym!("pw_thread_loop_destroy", FnPwThreadLoopDestroy),
                pw_thread_loop_start: sym!("pw_thread_loop_start", FnPwThreadLoopStart),
                pw_thread_loop_stop: sym!("pw_thread_loop_stop", FnPwThreadLoopStop),
                pw_thread_loop_get_loop: sym!("pw_thread_loop_get_loop", FnPwThreadLoopGetLoop),
                pw_stream_new_simple: sym!("pw_stream_new_simple", FnPwStreamNewSimple),
                pw_stream_destroy: sym!("pw_stream_destroy", FnPwStreamDestroy),
                pw_stream_connect: sym!("pw_stream_connect", FnPwStreamConnect),
                pw_stream_disconnect: sym!("pw_stream_disconnect", FnPwStreamDisconnect),
                pw_stream_dequeue_buffer: sym!("pw_stream_dequeue_buffer", FnPwStreamDequeueBuffer),
                pw_stream_queue_buffer: sym!("pw_stream_queue_buffer", FnPwStreamQueueBuffer),
                pw_properties_new: sym!("pw_properties_new", FnPwPropertiesNew),
                pw_properties_set: sym!("pw_properties_set", FnPwPropertiesSet),
            };

            // One-time global init.  `pw_init(NULL, NULL)` is documented
            // as safe to call multiple times but we only call it once
            // because our load is behind a OnceLock.
            (syms.pw_init)(ptr::null_mut(), ptr::null_mut());

            Some(syms)
        })
        .as_ref()
}

/// Whether libpipewire-0.3.so.0 is available on this system.
pub fn available() -> bool {
    syms().is_some()
}

// ── SPA POD builder ───────────────────────────────────────────────────

/// Build the `EnumFormat` POD for a 48 kHz stereo F32_LE capture stream.
///
/// The SPA POD format is a binary serialisation with 8-byte alignment
/// between consecutive pods.  Top level is an Object POD whose body is
/// { object_type, object_id, properties... }.  Each property in the
/// body is { key: u32, flags: u32, value_pod: POD }, with value_pod
/// padded up to the next 8-byte boundary.  The Object body itself is
/// wrapped in a POD header { size, type=Object }.
///
/// **Alignment matters:** libpipewire reads POD fields assuming 8-byte
/// alignment of the containing allocation.  Returning a `Vec<u8>` is
/// unsafe because the backing buffer's alignment is `align_of::<u8>()
/// == 1` — libpipewire will SIGSEGV on a word-sized load.  We return a
/// `Vec<u64>` which is guaranteed `align_of::<u64>() == 8`, then view
/// its bytes.  Callers pass `vec.as_ptr() as *const c_void`.
fn build_audio_format_pod() -> Vec<u64> {
    let mut body: Vec<u8> = Vec::with_capacity(128);
    body.extend_from_slice(&SPA_TYPE_OBJECT_FORMAT.to_le_bytes());
    body.extend_from_slice(&SPA_PARAM_ENUM_FORMAT.to_le_bytes());

    fn prop(out: &mut Vec<u8>, key: u32, pod_type: u32, value: &[u8]) {
        out.extend_from_slice(&key.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // flags
        out.extend_from_slice(&(value.len() as u32).to_le_bytes()); // pod body size
        out.extend_from_slice(&pod_type.to_le_bytes());
        out.extend_from_slice(value);
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
    }

    prop(
        &mut body,
        SPA_FORMAT_MEDIA_TYPE,
        SPA_TYPE_ID,
        &SPA_MEDIA_TYPE_AUDIO.to_le_bytes(),
    );
    prop(
        &mut body,
        SPA_FORMAT_MEDIA_SUBTYPE,
        SPA_TYPE_ID,
        &SPA_MEDIA_SUBTYPE_RAW.to_le_bytes(),
    );
    prop(
        &mut body,
        SPA_FORMAT_AUDIO_FORMAT,
        SPA_TYPE_ID,
        &SPA_AUDIO_FORMAT_F32_LE.to_le_bytes(),
    );
    prop(
        &mut body,
        SPA_FORMAT_AUDIO_RATE,
        SPA_TYPE_INT,
        &48000i32.to_le_bytes(),
    );
    prop(
        &mut body,
        SPA_FORMAT_AUDIO_CHANNELS,
        SPA_TYPE_INT,
        &2i32.to_le_bytes(),
    );

    // Wrap body in the outer POD header.
    let mut bytes = Vec::with_capacity(8 + body.len());
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&SPA_TYPE_OBJECT.to_le_bytes());
    bytes.extend_from_slice(&body);
    // Repack as `Vec<u64>` so the allocation is 8-byte aligned —
    // libpipewire word-loads POD fields and crashes on a misaligned
    // allocation (the Vec<u8> returned before this change had 1-byte
    // alignment, which tripped a SIGSEGV during format negotiation).
    assert!(
        bytes.len().is_multiple_of(8),
        "POD bytes must be 8-byte multiple"
    );
    let mut aligned: Vec<u64> = vec![0u64; bytes.len() / 8];
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), aligned.as_mut_ptr() as *mut u8, bytes.len());
    }
    aligned
}

// ── Process callback state ────────────────────────────────────────────

/// Heap-allocated state passed through libpipewire as the `user_data`
/// pointer.  Lifetime: from `Capture::start` until the Capture is
/// dropped (the thread-loop is stopped before the Box is freed so no
/// callback fires with a dangling pointer).
struct CaptureState {
    stream: *mut PwStream,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Flipped to false on Capture::drop so the callback stops forwarding
    /// (tx.send would still succeed into a dropped receiver, but we'd
    /// rather no-op quickly).
    active: AtomicBool,
}

// SAFETY: the pointers inside are only touched from the PW thread-loop
// callback (while active) and from Drop (after stop).  tx is Send+Sync.
unsafe impl Send for CaptureState {}
unsafe impl Sync for CaptureState {}

/// PW thread-loop calls this on every cycle.  RT-safe: no allocations
/// on the hot path beyond the Vec clone into the mpsc (unbounded, so
/// never blocks).
unsafe extern "C" fn on_process(data: *mut c_void) {
    unsafe {
        let state = &*(data as *const CaptureState);
        if !state.active.load(Ordering::Acquire) {
            return;
        }
        let Some(s) = syms() else {
            return;
        };
        let buf = (s.pw_stream_dequeue_buffer)(state.stream);
        if buf.is_null() {
            return;
        }
        let pw_buf = &*buf;
        let spa_buf = pw_buf.buffer;
        if !spa_buf.is_null() {
            let sb = &*spa_buf;
            if sb.n_datas >= 1 && !sb.datas.is_null() {
                let d = &*sb.datas;
                if !d.chunk.is_null() && !d.data.is_null() {
                    let c = &*d.chunk;
                    let size = c.size as usize;
                    let offset = c.offset as usize % d.maxsize.max(1) as usize;
                    if size > 0 {
                        let src = (d.data as *const u8).add(offset);
                        let slice = std::slice::from_raw_parts(src, size);
                        // Unbounded channel — send never blocks.  If the
                        // receiver has been dropped, this silently fails;
                        // the Capture's Drop will have flipped `active`
                        // off before that point in the normal path.
                        let _ = state.tx.send(slice.to_vec());
                    }
                }
            }
        }
        (s.pw_stream_queue_buffer)(state.stream, buf);
    }
}

const STREAM_EVENTS: PwStreamEvents = PwStreamEvents {
    version: PW_VERSION_STREAM_EVENTS,
    destroy: None,
    state_changed: None,
    control_info: None,
    io_changed: None,
    param_changed: None,
    add_buffer: None,
    remove_buffer: None,
    process: Some(on_process),
    drained: None,
    command: None,
    trigger_done: None,
};

// ── Public capture handle ─────────────────────────────────────────────

/// Owns the PipeWire thread-loop + stream for one capture session.
/// Samples arrive as interleaved F32 LE stereo (4 bytes/sample × 2
/// channels) at 48 kHz through the receiver returned by `start`.
///
/// Drop disconnects + destroys the stream and joins the thread-loop,
/// so dropping the Capture is sufficient cleanup — there's nothing
/// async to await.
pub struct Capture {
    thread_loop: *mut PwThreadLoop,
    stream: *mut PwStream,
    state: *mut CaptureState,
}

// SAFETY: libpipewire itself is thread-safe behind pw_thread_loop_lock;
// all mutations after construction go through that lock (or Drop).
unsafe impl Send for Capture {}

impl Capture {
    /// Start a capture stream connected to the PipeWire daemon at
    /// `runtime_dir` (via `PIPEWIRE_REMOTE`), targeting the named sink's
    /// monitor output.  The per-PW-instance runtime dir is set via
    /// environment so a process-wide `pw_init` still works for multiple
    /// compositors (each runs its own daemon under a unique path).
    pub fn start(
        runtime_dir: &Path,
        target_node: &str,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Vec<u8>>), String> {
        let s = syms().ok_or_else(|| "libpipewire-0.3.so.0 not available".to_string())?;

        // Point this load of PipeWire at our private daemon.  These are
        // read inside pw_context_connect (invoked by pw_stream_new_simple)
        // so the env must be set before that point.  Thread-locality is
        // fine for our use — the PW thread-loop inherits the env.
        // SAFETY: modifying the process env isn't thread-safe, but we
        // only call start() from synchronous compositor init (single
        // thread at that point) before any PW stream exists.
        unsafe {
            std::env::set_var(
                "PIPEWIRE_REMOTE",
                runtime_dir.join("pipewire-0").as_os_str(),
            );
            std::env::set_var("XDG_RUNTIME_DIR", runtime_dir.as_os_str());
        }

        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();

        unsafe {
            let name = CString::new("blit-capture").unwrap();
            let thread_loop = (s.pw_thread_loop_new)(name.as_ptr(), ptr::null());
            if thread_loop.is_null() {
                return Err("pw_thread_loop_new failed".to_string());
            }
            let loop_ = (s.pw_thread_loop_get_loop)(thread_loop);

            // Build properties: monitor-capture of the named sink at a
            // tight quantum (256 / 48000 = 5.3 ms) so RT_PROCESS fires
            // often enough that 20 ms frames are assembled promptly.
            let props = (s.pw_properties_new)(ptr::null());
            if props.is_null() {
                (s.pw_thread_loop_destroy)(thread_loop);
                return Err("pw_properties_new failed".to_string());
            }
            let set = |k: &str, v: &str| {
                let ck = CString::new(k).unwrap();
                let cv = CString::new(v).unwrap();
                (s.pw_properties_set)(props, ck.as_ptr(), cv.as_ptr());
            };
            set("media.type", "Audio");
            set("media.category", "Capture");
            set("media.role", "DSP");
            set("stream.capture.sink", "true");
            set("target.object", target_node);
            set("node.name", "blit-capture");
            set("node.latency", "256/48000");

            // Allocate user_data (Box -> raw) for the process callback.
            // Freed in Drop after the thread-loop has stopped, so no
            // callback can observe the freed pointer.
            let state = Box::into_raw(Box::new(CaptureState {
                stream: ptr::null_mut(),
                tx,
                active: AtomicBool::new(true),
            }));

            let stream = (s.pw_stream_new_simple)(
                loop_,
                CString::new("blit-capture").unwrap().as_ptr(),
                props, // ownership transferred to stream
                &STREAM_EVENTS,
                state as *mut c_void,
            );
            if stream.is_null() {
                drop(Box::from_raw(state));
                (s.pw_thread_loop_destroy)(thread_loop);
                return Err("pw_stream_new_simple failed".to_string());
            }
            (*state).stream = stream;

            // Connect with the format POD describing the capture format.
            let pod = build_audio_format_pod();
            let mut params: [*const c_void; 1] = [pod.as_ptr() as *const c_void];
            let flags =
                PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS | PW_STREAM_FLAG_RT_PROCESS;
            let rc = (s.pw_stream_connect)(
                stream,
                PW_DIRECTION_INPUT,
                PW_ID_ANY,
                flags,
                params.as_mut_ptr(),
                params.len() as u32,
            );
            if rc < 0 {
                (s.pw_stream_destroy)(stream);
                drop(Box::from_raw(state));
                (s.pw_thread_loop_destroy)(thread_loop);
                return Err(format!("pw_stream_connect failed: {rc}"));
            }
            // POD is referenced only during the connect call — libpipewire
            // copies what it needs.
            drop(pod);

            if (s.pw_thread_loop_start)(thread_loop) < 0 {
                (s.pw_stream_disconnect)(stream);
                (s.pw_stream_destroy)(stream);
                drop(Box::from_raw(state));
                (s.pw_thread_loop_destroy)(thread_loop);
                return Err("pw_thread_loop_start failed".to_string());
            }

            Ok((
                Self {
                    thread_loop,
                    stream,
                    state,
                },
                rx,
            ))
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Order matters: flip `active` so a racing callback bails early,
        // stop the thread-loop (blocks until the loop thread exits —
        // guarantees no further callbacks), disconnect + destroy the
        // stream, destroy the loop, free the user_data.
        let Some(s) = syms() else {
            return;
        };
        unsafe {
            if !self.state.is_null() {
                (*self.state).active.store(false, Ordering::Release);
            }
            if !self.thread_loop.is_null() {
                (s.pw_thread_loop_stop)(self.thread_loop);
            }
            if !self.stream.is_null() {
                (s.pw_stream_disconnect)(self.stream);
                (s.pw_stream_destroy)(self.stream);
                self.stream = ptr::null_mut();
            }
            if !self.thread_loop.is_null() {
                (s.pw_thread_loop_destroy)(self.thread_loop);
                self.thread_loop = ptr::null_mut();
            }
            if !self.state.is_null() {
                drop(Box::from_raw(self.state));
                self.state = ptr::null_mut();
            }
        }
    }
}

// CStr helper so we can format error strings for logs without pulling
// in a dependency.  Safe because libpipewire guarantees NUL termination
// on the error strings it emits.
#[allow(dead_code)]
unsafe fn cstr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p).to_string_lossy().into_owned() }
}
