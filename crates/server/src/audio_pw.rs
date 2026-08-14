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
use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::ptr;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::atomic::{AtomicU32, AtomicU64};
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
#[repr(C)]
struct PwCore {
    _priv: [u8; 0],
}

#[repr(C)]
struct SpaList {
    next: *mut SpaList,
    prev: *mut SpaList,
}

#[repr(C)]
struct SpaCallbacks {
    funcs: *const c_void,
    data: *mut c_void,
}

#[repr(C)]
struct SpaHook {
    link: SpaList,
    callbacks: SpaCallbacks,
    removed: Option<unsafe extern "C" fn(*mut SpaHook)>,
    private: *mut c_void,
}

// ── spa_dict (must match C layout exactly) ────────────────────────────

#[repr(C)]
struct SpaDictItem {
    key: *const c_char,
    value: *const c_char,
}

#[repr(C)]
struct SpaDict {
    flags: u32,
    n_items: u32,
    items: *const SpaDictItem,
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
    metas: *mut SpaMeta,
    datas: *mut SpaData,
}

#[repr(C)]
struct SpaMeta {
    type_: u32,
    size: u32,
    data: *mut c_void,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
struct SpaMetaHeader {
    flags: u32,
    offset: u32,
    pts: i64,
    dts_offset: i64,
    seq: u64,
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

#[repr(C)]
struct PwCoreEvents {
    version: u32,
    info: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    done: Option<unsafe extern "C" fn(*mut c_void, u32, i32)>,
    ping: Option<unsafe extern "C" fn(*mut c_void, u32, i32)>,
    error: Option<unsafe extern "C" fn(*mut c_void, u32, i32, i32, *const c_char)>,
    remove_id: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    bound_id: Option<unsafe extern "C" fn(*mut c_void, u32, u32)>,
    add_mem: Option<unsafe extern "C" fn(*mut c_void, u32, u32, i32, u32)>,
    remove_mem: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    bound_props: Option<unsafe extern "C" fn(*mut c_void, u32, u32, *const SpaDict)>,
}

const PW_VERSION_STREAM_EVENTS: u32 = 2;

// ── PipeWire constants ────────────────────────────────────────────────

const PW_DIRECTION_INPUT: i32 = 0;
const PW_DIRECTION_OUTPUT: i32 = 1;
const PW_ID_ANY: u32 = u32::MAX;
const PW_STREAM_FLAG_AUTOCONNECT: u32 = 1 << 0;
const PW_STREAM_FLAG_MAP_BUFFERS: u32 = 1 << 2;
const PW_STREAM_FLAG_RT_PROCESS: u32 = 1 << 4;

// ── SPA POD constants ─────────────────────────────────────────────────

const SPA_TYPE_ID: u32 = 3;
const SPA_TYPE_INT: u32 = 4;
const SPA_TYPE_OBJECT: u32 = 15;
const SPA_TYPE_RECTANGLE: u32 = 10;
const SPA_TYPE_FRACTION: u32 = 11;
const SPA_TYPE_OBJECT_FORMAT: u32 = 0x40003;
const SPA_TYPE_OBJECT_PARAM_BUFFERS: u32 = 0x40004;
const SPA_TYPE_OBJECT_PARAM_META: u32 = 0x40005;
const SPA_PARAM_ENUM_FORMAT: u32 = 3;
const SPA_PARAM_FORMAT: u32 = 4;
const SPA_PARAM_BUFFERS: u32 = 5;
const SPA_PARAM_META: u32 = 6;
const SPA_PARAM_BUFFERS_BUFFERS: u32 = 1;
const SPA_PARAM_BUFFERS_BLOCKS: u32 = 2;
const SPA_PARAM_BUFFERS_SIZE: u32 = 3;
const SPA_PARAM_BUFFERS_STRIDE: u32 = 4;
const SPA_PARAM_META_TYPE: u32 = 1;
const SPA_PARAM_META_SIZE: u32 = 2;
const SPA_META_HEADER: u32 = 1;
const SPA_META_HEADER_FLAG_GAP: u32 = 1 << 4;
const SPA_TIME_INVALID: i64 = i64::MIN;
const WRAPPED_MS_PERIOD: i64 = 1i64 << 32;
const SPA_FORMAT_MEDIA_TYPE: u32 = 1;
const SPA_FORMAT_MEDIA_SUBTYPE: u32 = 2;
const SPA_FORMAT_AUDIO_FORMAT: u32 = 0x10001;
const SPA_FORMAT_AUDIO_RATE: u32 = 0x10003;
const SPA_FORMAT_AUDIO_CHANNELS: u32 = 0x10004;
const SPA_MEDIA_TYPE_AUDIO: u32 = 1;
const SPA_MEDIA_TYPE_VIDEO: u32 = 2;
const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;
const SPA_FORMAT_VIDEO_SIZE: u32 = 131_075;
const SPA_FORMAT_VIDEO_FRAMERATE: u32 = 131_076;
const SPA_FORMAT_VIDEO_FORMAT: u32 = 131_073;
const SPA_VIDEO_FORMAT_RGBA: u32 = 11;
const SPA_AUDIO_FORMAT_F32_LE: u32 = 283;
const SPA_AUDIO_FORMAT_S16_LE: u32 = 259;

// ── Resolved symbols ──────────────────────────────────────────────────

type FnPwInit = unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char);
type FnPwDeinit = unsafe extern "C" fn();
type FnPwThreadLoopNew = unsafe extern "C" fn(*const c_char, *const SpaDict) -> *mut PwThreadLoop;
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
type FnPwStreamUpdateParams = unsafe extern "C" fn(*mut PwStream, *mut *const c_void, u32) -> c_int;
type FnPwStreamGetNodeId = unsafe extern "C" fn(*mut PwStream) -> u32;
type FnPwStreamGetProperties = unsafe extern "C" fn(*mut PwStream) -> *const PwProperties;
type FnPwStreamGetCore = unsafe extern "C" fn(*mut PwStream) -> *mut PwCore;
type FnPwCoreAddListener =
    unsafe extern "C" fn(*mut PwCore, *mut SpaHook, *const PwCoreEvents, *mut c_void) -> c_int;
type FnPwPropertiesNew = unsafe extern "C" fn(*const c_char) -> *mut PwProperties;
type FnPwPropertiesSet =
    unsafe extern "C" fn(*mut PwProperties, *const c_char, *const c_char) -> c_int;
type FnPwPropertiesGet = unsafe extern "C" fn(*const PwProperties, *const c_char) -> *const c_char;

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
    pw_stream_update_params: FnPwStreamUpdateParams,
    pw_stream_get_node_id: FnPwStreamGetNodeId,
    pw_stream_get_properties: FnPwStreamGetProperties,
    pw_stream_get_core: FnPwStreamGetCore,
    pw_core_add_listener: FnPwCoreAddListener,
    pw_properties_new: FnPwPropertiesNew,
    pw_properties_set: FnPwPropertiesSet,
    pw_properties_get: FnPwPropertiesGet,
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
                pw_stream_update_params: sym!("pw_stream_update_params", FnPwStreamUpdateParams),
                pw_stream_get_node_id: sym!("pw_stream_get_node_id", FnPwStreamGetNodeId),
                pw_stream_get_properties: sym!("pw_stream_get_properties", FnPwStreamGetProperties),
                pw_stream_get_core: sym!("pw_stream_get_core", FnPwStreamGetCore),
                pw_core_add_listener: sym!("pw_core_add_listener", FnPwCoreAddListener),
                pw_properties_new: sym!("pw_properties_new", FnPwPropertiesNew),
                pw_properties_set: sym!("pw_properties_set", FnPwPropertiesSet),
                pw_properties_get: sym!("pw_properties_get", FnPwPropertiesGet),
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
    build_audio_format_pod_for(SPA_AUDIO_FORMAT_F32_LE, 2)
}

fn build_audio_format_pod_for(format: u32, channels: i32) -> Vec<u64> {
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
        &format.to_le_bytes(),
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
        &channels.to_le_bytes(),
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

fn build_rgba_format_pod(width: u16, height: u16, fps: u8) -> Vec<u64> {
    let mut body: Vec<u8> = Vec::with_capacity(160);
    body.extend_from_slice(&SPA_TYPE_OBJECT_FORMAT.to_le_bytes());
    body.extend_from_slice(&SPA_PARAM_ENUM_FORMAT.to_le_bytes());
    let prop = |out: &mut Vec<u8>, key: u32, pod_type: u32, value: &[u8]| {
        out.extend_from_slice(&key.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(&pod_type.to_le_bytes());
        out.extend_from_slice(value);
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
    };
    prop(
        &mut body,
        SPA_FORMAT_MEDIA_TYPE,
        SPA_TYPE_ID,
        &SPA_MEDIA_TYPE_VIDEO.to_le_bytes(),
    );
    prop(
        &mut body,
        SPA_FORMAT_MEDIA_SUBTYPE,
        SPA_TYPE_ID,
        &SPA_MEDIA_SUBTYPE_RAW.to_le_bytes(),
    );
    prop(
        &mut body,
        SPA_FORMAT_VIDEO_FORMAT,
        SPA_TYPE_ID,
        &SPA_VIDEO_FORMAT_RGBA.to_le_bytes(),
    );
    let mut size = Vec::with_capacity(8);
    size.extend_from_slice(&(width as u32).to_le_bytes());
    size.extend_from_slice(&(height as u32).to_le_bytes());
    prop(&mut body, SPA_FORMAT_VIDEO_SIZE, SPA_TYPE_RECTANGLE, &size);
    let mut rate = Vec::with_capacity(8);
    rate.extend_from_slice(&(fps as u32).to_le_bytes());
    rate.extend_from_slice(&1u32.to_le_bytes());
    prop(
        &mut body,
        SPA_FORMAT_VIDEO_FRAMERATE,
        SPA_TYPE_FRACTION,
        &rate,
    );
    let mut bytes = Vec::with_capacity(8 + body.len());
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&SPA_TYPE_OBJECT.to_le_bytes());
    bytes.extend_from_slice(&body);
    assert!(bytes.len().is_multiple_of(8));
    let mut aligned = vec![0u64; bytes.len() / 8];
    unsafe {
        ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            aligned.as_mut_ptr().cast::<u8>(),
            bytes.len(),
        );
    }
    aligned
}

/// Request `SPA_META_Header` on every negotiated raw-video buffer. PipeWire
/// allocates the metadata area; the process callback only fills it.
fn build_header_meta_pod() -> Vec<u64> {
    let mut body = Vec::with_capacity(64);
    body.extend_from_slice(&SPA_TYPE_OBJECT_PARAM_META.to_le_bytes());
    body.extend_from_slice(&SPA_PARAM_META.to_le_bytes());
    let prop = |out: &mut Vec<u8>, key: u32, pod_type: u32, value: &[u8]| {
        out.extend_from_slice(&key.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(&pod_type.to_le_bytes());
        out.extend_from_slice(value);
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
    };
    prop(
        &mut body,
        SPA_PARAM_META_TYPE,
        SPA_TYPE_ID,
        &SPA_META_HEADER.to_le_bytes(),
    );
    prop(
        &mut body,
        SPA_PARAM_META_SIZE,
        SPA_TYPE_INT,
        &(std::mem::size_of::<SpaMetaHeader>() as i32).to_le_bytes(),
    );
    let mut bytes = Vec::with_capacity(8 + body.len());
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&SPA_TYPE_OBJECT.to_le_bytes());
    bytes.extend_from_slice(&body);
    assert!(bytes.len().is_multiple_of(8));
    let mut aligned = vec![0u64; bytes.len() / 8];
    unsafe {
        ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            aligned.as_mut_ptr().cast::<u8>(),
            bytes.len(),
        );
    }
    aligned
}

/// Fix the raw-video pool to the RFC's three buffers. Each buffer is one
/// tightly packed RGBA block; bounding the producer queue alone would not
/// bound buffers retained by a PipeWire consumer.
fn build_raw_video_buffers_pod(frame_size: i32, stride: i32) -> Vec<u64> {
    let mut body = Vec::with_capacity(112);
    body.extend_from_slice(&SPA_TYPE_OBJECT_PARAM_BUFFERS.to_le_bytes());
    body.extend_from_slice(&SPA_PARAM_BUFFERS.to_le_bytes());
    let prop = |out: &mut Vec<u8>, key: u32, value: i32| {
        out.extend_from_slice(&key.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&SPA_TYPE_INT.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
    };
    prop(&mut body, SPA_PARAM_BUFFERS_BUFFERS, 3);
    prop(&mut body, SPA_PARAM_BUFFERS_BLOCKS, 1);
    prop(&mut body, SPA_PARAM_BUFFERS_SIZE, frame_size);
    prop(&mut body, SPA_PARAM_BUFFERS_STRIDE, stride);
    let mut bytes = Vec::with_capacity(8 + body.len());
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&SPA_TYPE_OBJECT.to_le_bytes());
    bytes.extend_from_slice(&body);
    assert!(bytes.len().is_multiple_of(8));
    let mut aligned = vec![0u64; bytes.len() / 8];
    unsafe {
        ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            aligned.as_mut_ptr().cast::<u8>(),
            bytes.len(),
        );
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
            // Ask for RT priority on the loop thread (best effort: newer
            // libpipewire honours `loop.rt-prio`, older versions ignore
            // unknown keys).  Without it the capture loop runs SCHED_OTHER
            // and competes with video-encode threads for CPU.  The dict
            // and its strings only need to outlive the _new call —
            // libpipewire copies what it keeps.
            let rt_key = c"loop.rt-prio";
            let rt_val = c"88";
            let loop_props_items = [SpaDictItem {
                key: rt_key.as_ptr(),
                value: rt_val.as_ptr(),
            }];
            let loop_props = SpaDict {
                flags: 0,
                n_items: loop_props_items.len() as u32,
                items: loop_props_items.as_ptr(),
            };
            let thread_loop = (s.pw_thread_loop_new)(name.as_ptr(), &loop_props);
            if thread_loop.is_null() {
                return Err("pw_thread_loop_new failed".to_string());
            }
            let loop_ = (s.pw_thread_loop_get_loop)(thread_loop);

            // Build properties: monitor-capture of the named sink.  The
            // 1024/48000 latency (~21 ms, one Opus frame) matches the
            // daemon's forced quantum: fewer, larger cycles give the
            // (possibly non-RT) graph threads 4x more scheduling slack
            // under encode-saturated CPU than the old 5.3 ms quantum,
            // and the client's >= 60 ms jitter buffer hides the extra
            // batching entirely.
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
            set("node.latency", "1024/48000");

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

// ── Viewer microphone source ─────────────────────────────────────────

const SOURCE_QUEUE_FRAMES: usize = 3;
const PCM_FRAME_BYTES: usize = 960 * 2;

struct SourceQueue {
    frames: VecDeque<Vec<u8>>,
    offset: usize,
}

struct SourceState {
    stream: *mut PwStream,
    queue: Mutex<SourceQueue>,
    active: AtomicBool,
}

// SAFETY: the PipeWire callback owns stream access. Producers only touch the
// mutex-protected byte queue, and Drop stops the loop before freeing state.
unsafe impl Send for SourceState {}
unsafe impl Sync for SourceState {}

/// Convert PipeWire's suggested mono-S16 frame count into a writable byte
/// count. A zero request means "no suggestion", so use the whole buffer.
/// The result is always sample-aligned and never exceeds the mapped plane.
fn pcm_requested_bytes(requested_frames: u64, capacity: usize) -> usize {
    let aligned_capacity = capacity & !1;
    if requested_frames == 0 {
        return aligned_capacity;
    }
    requested_frames
        .saturating_mul(2)
        .min(aligned_capacity as u64) as usize
        & !1
}

unsafe extern "C" fn on_source_process(data: *mut c_void) {
    unsafe {
        let state = &*(data as *const SourceState);
        if !state.active.load(Ordering::Acquire) {
            return;
        }
        let Some(s) = syms() else {
            return;
        };
        let buffer = (s.pw_stream_dequeue_buffer)(state.stream);
        if buffer.is_null() {
            return;
        }
        let pw_buffer = &mut *buffer;
        let spa_buffer = pw_buffer.buffer;
        if !spa_buffer.is_null() {
            let spa = &mut *spa_buffer;
            if spa.n_datas >= 1 && !spa.datas.is_null() {
                let plane = &mut *spa.datas;
                if !plane.data.is_null() && !plane.chunk.is_null() {
                    let capacity = plane.maxsize as usize;
                    let target = pcm_requested_bytes(pw_buffer.requested, capacity);
                    let output = std::slice::from_raw_parts_mut(plane.data as *mut u8, target);
                    output.fill(0);
                    let mut written = 0usize;
                    if let Ok(mut queue) = state.queue.try_lock() {
                        while written < target {
                            let Some(frame_len) = queue.frames.front().map(Vec::len) else {
                                break;
                            };
                            let offset = queue.offset;
                            let available = frame_len.saturating_sub(offset);
                            let copy = available.min(target - written);
                            {
                                let frame = queue.frames.front().expect("checked above");
                                output[written..written + copy]
                                    .copy_from_slice(&frame[offset..offset + copy]);
                            }
                            written += copy;
                            queue.offset += copy;
                            if queue.offset == frame_len {
                                queue.frames.pop_front();
                                queue.offset = 0;
                            }
                        }
                    }
                    let chunk = &mut *plane.chunk;
                    chunk.offset = 0;
                    chunk.size = target as u32;
                    chunk.stride = 2;
                    chunk.flags = 0;
                    pw_buffer.size = (target / 2) as u64;
                }
            }
        }
        (s.pw_stream_queue_buffer)(state.stream, buffer);
    }
}

const SOURCE_STREAM_EVENTS: PwStreamEvents = PwStreamEvents {
    version: PW_VERSION_STREAM_EVENTS,
    destroy: None,
    state_changed: None,
    control_info: None,
    io_changed: None,
    param_changed: None,
    add_buffer: None,
    remove_buffer: None,
    process: Some(on_source_process),
    drained: None,
    command: None,
    trigger_done: None,
};

/// Short-lived 48 kHz mono S16 virtual source for a viewer microphone lease.
/// The node is owned by the stream and disappears when this handle is dropped.
pub struct PcmSource {
    thread_loop: *mut PwThreadLoop,
    stream: *mut PwStream,
    state: *mut SourceState,
}

// SAFETY: all PipeWire mutation is confined to the library thread loop.
unsafe impl Send for PcmSource {}

impl PcmSource {
    pub fn start(runtime_dir: &Path) -> Result<Self, String> {
        let s = syms().ok_or_else(|| "libpipewire-0.3.so.0 not available".to_string())?;
        // See Capture::start: the private daemon is selected before the
        // simple stream creates its core connection.
        unsafe {
            std::env::set_var(
                "PIPEWIRE_REMOTE",
                runtime_dir.join("pipewire-0").as_os_str(),
            );
            std::env::set_var("XDG_RUNTIME_DIR", runtime_dir.as_os_str());
        }
        unsafe {
            let thread_loop = (s.pw_thread_loop_new)(c"blit-microphone".as_ptr(), ptr::null());
            if thread_loop.is_null() {
                return Err("pw_thread_loop_new failed".into());
            }
            let props = (s.pw_properties_new)(ptr::null());
            if props.is_null() {
                (s.pw_thread_loop_destroy)(thread_loop);
                return Err("pw_properties_new failed".into());
            }
            let set = |key: &str, value: &str| {
                let key = CString::new(key).unwrap();
                let value = CString::new(value).unwrap();
                (s.pw_properties_set)(props, key.as_ptr(), value.as_ptr());
            };
            set("media.type", "Audio");
            set("media.category", "Playback");
            set("media.role", "Communication");
            set("media.class", "Audio/Source");
            set("node.name", "blit-microphone");
            set("node.nick", "Blit Microphone");
            set("node.description", "Blit Microphone");
            set("node.virtual", "true");
            set("node.latency", "960/48000");

            let state = Box::into_raw(Box::new(SourceState {
                stream: ptr::null_mut(),
                queue: Mutex::new(SourceQueue {
                    frames: VecDeque::with_capacity(SOURCE_QUEUE_FRAMES),
                    offset: 0,
                }),
                active: AtomicBool::new(true),
            }));
            let stream = (s.pw_stream_new_simple)(
                (s.pw_thread_loop_get_loop)(thread_loop),
                c"blit-microphone".as_ptr(),
                props,
                &SOURCE_STREAM_EVENTS,
                state.cast(),
            );
            if stream.is_null() {
                drop(Box::from_raw(state));
                (s.pw_thread_loop_destroy)(thread_loop);
                return Err("pw_stream_new_simple failed".into());
            }
            (*state).stream = stream;
            let pod = build_audio_format_pod_for(SPA_AUDIO_FORMAT_S16_LE, 1);
            let mut params = [pod.as_ptr() as *const c_void];
            let flags = PW_STREAM_FLAG_MAP_BUFFERS | PW_STREAM_FLAG_RT_PROCESS;
            let rc = (s.pw_stream_connect)(
                stream,
                PW_DIRECTION_OUTPUT,
                PW_ID_ANY,
                flags,
                params.as_mut_ptr(),
                1,
            );
            if rc < 0 {
                (s.pw_stream_destroy)(stream);
                drop(Box::from_raw(state));
                (s.pw_thread_loop_destroy)(thread_loop);
                return Err(format!("pw_stream_connect failed: {rc}"));
            }
            if (s.pw_thread_loop_start)(thread_loop) < 0 {
                (s.pw_stream_disconnect)(stream);
                (s.pw_stream_destroy)(stream);
                drop(Box::from_raw(state));
                (s.pw_thread_loop_destroy)(thread_loop);
                return Err("pw_thread_loop_start failed".into());
            }
            Ok(Self {
                thread_loop,
                stream,
                state,
            })
        }
    }

    /// Enqueue exactly one 20 ms PCM frame. The bounded jitter queue keeps
    /// newest input under congestion and marks the old audio as lost.
    pub fn push(&self, pcm: Vec<u8>) -> Result<(), &'static str> {
        if pcm.len() != PCM_FRAME_BYTES {
            return Err("PCM frame must contain 960 mono S16 samples");
        }
        // SAFETY: state remains live until Drop, and this method borrows self.
        let state = unsafe { &*self.state };
        let mut queue = state.queue.lock().map_err(|_| "source queue poisoned")?;
        if queue.frames.len() >= SOURCE_QUEUE_FRAMES {
            queue.frames.pop_front();
            queue.offset = 0;
        }
        queue.frames.push_back(pcm);
        Ok(())
    }
}

impl Drop for PcmSource {
    fn drop(&mut self) {
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
            }
            if !self.thread_loop.is_null() {
                (s.pw_thread_loop_destroy)(self.thread_loop);
            }
            if !self.state.is_null() {
                drop(Box::from_raw(self.state));
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RawVideoFrame {
    rgba: Vec<u8>,
    pts_ns: i64,
    sequence: u64,
}

struct RawVideoState {
    stream: *mut PwStream,
    frames: Mutex<VecDeque<RawVideoFrame>>,
    active: AtomicBool,
    node_id: AtomicU32,
    serial: AtomicU64,
    next_sequence: AtomicU64,
    frame_size: i32,
    stride: u32,
    node_name: CString,
    core_hook: SpaHook,
}

unsafe impl Send for RawVideoState {}
unsafe impl Sync for RawVideoState {}

unsafe extern "C" fn on_raw_core_bound_props(
    data: *mut c_void,
    _local_id: u32,
    global_id: u32,
    props: *const SpaDict,
) {
    unsafe {
        if props.is_null() {
            return;
        }
        let state = &*(data as *const RawVideoState);
        let props = &*props;
        if props.items.is_null() {
            return;
        }
        let items = std::slice::from_raw_parts(props.items, props.n_items as usize);
        let mut name_matches = false;
        let mut serial = None;
        for item in items {
            if item.key.is_null() || item.value.is_null() {
                continue;
            }
            let key = CStr::from_ptr(item.key).to_bytes();
            if key == b"node.name" {
                name_matches = CStr::from_ptr(item.value).to_bytes() == state.node_name.as_bytes();
            } else if key == b"object.serial" {
                serial = CStr::from_ptr(item.value)
                    .to_str()
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok());
            }
        }
        if name_matches {
            state.node_id.store(global_id, Ordering::Release);
            if let Some(serial) = serial {
                state.serial.store(serial, Ordering::Release);
            }
        }
    }
}

const RAW_CORE_EVENTS: PwCoreEvents = PwCoreEvents {
    version: 1,
    info: None,
    done: None,
    ping: None,
    error: None,
    remove_id: None,
    bound_id: None,
    add_mem: None,
    remove_mem: None,
    bound_props: Some(on_raw_core_bound_props),
};

unsafe extern "C" fn on_raw_video_state_changed(
    data: *mut c_void,
    _old: i32,
    new: i32,
    _error: *const c_char,
) {
    unsafe {
        // PAUSED (2) is the first state in which the server has assigned a
        // node. STREAMING (3) may follow before the creator observes it.
        if new < 2 {
            return;
        }
        let state = &*(data as *const RawVideoState);
        let Some(s) = syms() else {
            return;
        };
        let node_id = (s.pw_stream_get_node_id)(state.stream);
        if node_id != PW_ID_ANY {
            state.node_id.store(node_id, Ordering::Release);
        }
        let properties = (s.pw_stream_get_properties)(state.stream);
        if !properties.is_null() {
            let value = (s.pw_properties_get)(properties, c"object.serial".as_ptr());
            if !value.is_null()
                && let Ok(value) = CStr::from_ptr(value).to_str()
                && let Ok(serial) = value.parse::<u64>()
            {
                state.serial.store(serial, Ordering::Release);
            }
        }
    }
}

unsafe extern "C" fn on_raw_video_param_changed(data: *mut c_void, id: u32, param: *const c_void) {
    unsafe {
        if id != SPA_PARAM_FORMAT || param.is_null() {
            return;
        }
        let state = &*(data as *const RawVideoState);
        let Some(s) = syms() else {
            return;
        };
        let buffers = build_raw_video_buffers_pod(state.frame_size, state.stride as i32);
        let header_meta = build_header_meta_pod();
        let mut params = [
            buffers.as_ptr() as *const c_void,
            header_meta.as_ptr() as *const c_void,
        ];
        let _ = (s.pw_stream_update_params)(state.stream, params.as_mut_ptr(), params.len() as u32);
    }
}

unsafe extern "C" fn on_raw_video_process(data: *mut c_void) {
    unsafe {
        let state = &*(data as *const RawVideoState);
        if !state.active.load(Ordering::Acquire) {
            return;
        }
        let Some(s) = syms() else {
            return;
        };
        let buffer = (s.pw_stream_dequeue_buffer)(state.stream);
        if buffer.is_null() {
            return;
        }
        let pw_buffer = &*buffer;
        let spa_buffer = pw_buffer.buffer;
        if !spa_buffer.is_null() {
            let spa = &mut *spa_buffer;
            if spa.n_datas >= 1 && !spa.datas.is_null() {
                let plane = &mut *spa.datas;
                if !plane.data.is_null() && !plane.chunk.is_null() {
                    let capacity = plane.maxsize as usize;
                    let frame = take_latest_frame(&state.frames)
                        .filter(|frame| frame.rgba.len() <= capacity);
                    let size = frame.as_ref().map_or(0, |frame| frame.rgba.len());
                    if let Some(frame) = frame.as_ref() {
                        ptr::copy_nonoverlapping(
                            frame.rgba.as_ptr(),
                            plane.data.cast::<u8>(),
                            size,
                        );
                    }
                    let chunk = &mut *plane.chunk;
                    chunk.offset = 0;
                    chunk.size = size as u32;
                    chunk.stride = state.stride as i32;
                    chunk.flags = 0;
                    write_raw_video_header(spa, frame.as_ref());
                }
            }
        }
        (s.pw_stream_queue_buffer)(state.stream, buffer);
    }
}

/// Take the newest pending video frame and discard every older one. PipeWire
/// can call the process hook less often than the compositor produces frames;
/// replaying the remainder later would turn queue pressure into time travel.
fn take_latest_frame(frames: &Mutex<VecDeque<RawVideoFrame>>) -> Option<RawVideoFrame> {
    let mut frames = frames.try_lock().ok()?;
    let latest = frames.pop_back();
    frames.clear();
    latest
}

fn populate_video_header(header: &mut SpaMetaHeader, frame: Option<&RawVideoFrame>) {
    header.offset = 0;
    header.dts_offset = 0;
    if let Some(frame) = frame {
        header.flags = 0;
        header.pts = frame.pts_ns;
        header.seq = frame.sequence;
    } else {
        header.flags = SPA_META_HEADER_FLAG_GAP;
        header.pts = SPA_TIME_INVALID;
        header.seq = 0;
    }
}

/// Expand the compositor's wrapping u32 millisecond timestamp into the latest
/// matching CLOCK_MONOTONIC epoch. A frame timestamp is captured before this
/// function runs, so choosing the latest candidate not later than `now`
/// resolves the 49.7-day wrap without keeping mutable epoch state.
fn wrapped_timestamp_to_pts_ns(
    timestamp_ms: u32,
    timestamp_sub_us: u16,
    now_monotonic_ns: i64,
) -> i64 {
    let now_ms = now_monotonic_ns.div_euclid(1_000_000);
    let epoch = now_ms.div_euclid(WRAPPED_MS_PERIOD) * WRAPPED_MS_PERIOD;
    let mut expanded_ms = epoch + i64::from(timestamp_ms);
    if expanded_ms > now_ms {
        expanded_ms -= WRAPPED_MS_PERIOD;
    }
    expanded_ms
        .saturating_mul(1_000_000)
        .saturating_add(i64::from(timestamp_sub_us.min(999)) * 1_000)
}

fn monotonic_now_ns() -> Option<i64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return None;
    }
    Some(
        ts.tv_sec
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec),
    )
}

/// Fill the negotiated header metadata when it is present. A peer may decline
/// optional metadata, so absence is valid and must not suppress the frame.
unsafe fn write_raw_video_header(spa: &mut SpaBuffer, frame: Option<&RawVideoFrame>) {
    unsafe {
        if spa.n_metas == 0 || spa.metas.is_null() {
            return;
        }
        let metas = std::slice::from_raw_parts_mut(spa.metas, spa.n_metas as usize);
        let Some(meta) = metas.iter_mut().find(|meta| meta.type_ == SPA_META_HEADER) else {
            return;
        };
        if meta.data.is_null() || (meta.size as usize) < std::mem::size_of::<SpaMetaHeader>() {
            return;
        }
        populate_video_header(&mut *meta.data.cast::<SpaMetaHeader>(), frame);
    }
}

const RAW_VIDEO_STREAM_EVENTS: PwStreamEvents = PwStreamEvents {
    state_changed: Some(on_raw_video_state_changed),
    param_changed: Some(on_raw_video_param_changed),
    process: Some(on_raw_video_process),
    ..SOURCE_STREAM_EVENTS
};

/// Three-buffer, newest-frame-wins raw RGBA source used by a portal window
/// ScreenCast. The node disappears with the portal session.
pub struct RawVideoSource {
    thread_loop: *mut PwThreadLoop,
    stream: *mut PwStream,
    state: *mut RawVideoState,
    width: u16,
    height: u16,
}

unsafe impl Send for RawVideoSource {}

impl RawVideoSource {
    pub fn start(
        runtime_dir: &Path,
        session_id: u32,
        surface_id: u16,
        width: u16,
        height: u16,
        fps: u8,
    ) -> Result<Self, String> {
        Self::start_named(
            runtime_dir,
            &format!("blit-screencast-{session_id}-{surface_id}"),
            &format!("Blit Window {surface_id}"),
            "Screen",
            width,
            height,
            fps,
        )
    }

    pub fn start_camera(
        runtime_dir: &Path,
        width: u16,
        height: u16,
        fps: u8,
    ) -> Result<Self, String> {
        Self::start_named(
            runtime_dir,
            "blit-camera",
            "Blit Camera",
            "Camera",
            width,
            height,
            fps,
        )
    }

    fn start_named(
        runtime_dir: &Path,
        node_name: &str,
        description: &str,
        role: &str,
        width: u16,
        height: u16,
        fps: u8,
    ) -> Result<Self, String> {
        if width == 0 || height == 0 || fps == 0 {
            return Err("invalid ScreenCast format".into());
        }
        let stride = i32::from(width)
            .checked_mul(4)
            .ok_or_else(|| "raw-video stride overflow".to_string())?;
        let frame_size = stride
            .checked_mul(i32::from(height))
            .ok_or_else(|| "raw-video frame exceeds PipeWire buffer limits".to_string())?;
        let s = syms().ok_or_else(|| "libpipewire-0.3.so.0 not available".to_string())?;
        unsafe {
            std::env::set_var(
                "PIPEWIRE_REMOTE",
                runtime_dir.join("pipewire-0").as_os_str(),
            );
            std::env::set_var("XDG_RUNTIME_DIR", runtime_dir.as_os_str());
            let thread_loop = (s.pw_thread_loop_new)(c"blit-screencast".as_ptr(), ptr::null());
            if thread_loop.is_null() {
                return Err("pw_thread_loop_new failed".into());
            }
            let props = (s.pw_properties_new)(ptr::null());
            if props.is_null() {
                (s.pw_thread_loop_destroy)(thread_loop);
                return Err("pw_properties_new failed".into());
            }
            let set = |key: &str, value: &str| {
                let key = CString::new(key).unwrap();
                let value = CString::new(value).unwrap();
                (s.pw_properties_set)(props, key.as_ptr(), value.as_ptr());
            };
            set("media.type", "Video");
            set("media.category", "Capture");
            set("media.role", role);
            set("media.class", "Video/Source");
            set("node.name", node_name);
            set("node.nick", description);
            set("node.description", description);
            set("node.virtual", "true");
            set("node.pause-on-idle", "false");
            let node_name = CString::new(node_name).map_err(|_| "invalid PipeWire node name")?;
            let state = Box::into_raw(Box::new(RawVideoState {
                stream: ptr::null_mut(),
                frames: Mutex::new(VecDeque::with_capacity(3)),
                active: AtomicBool::new(true),
                node_id: AtomicU32::new(PW_ID_ANY),
                serial: AtomicU64::new(0),
                next_sequence: AtomicU64::new(0),
                frame_size,
                stride: stride as u32,
                node_name,
                core_hook: SpaHook {
                    link: SpaList {
                        next: ptr::null_mut(),
                        prev: ptr::null_mut(),
                    },
                    callbacks: SpaCallbacks {
                        funcs: ptr::null(),
                        data: ptr::null_mut(),
                    },
                    removed: None,
                    private: ptr::null_mut(),
                },
            }));
            let stream = (s.pw_stream_new_simple)(
                (s.pw_thread_loop_get_loop)(thread_loop),
                c"blit-screencast".as_ptr(),
                props,
                &RAW_VIDEO_STREAM_EVENTS,
                state.cast(),
            );
            if stream.is_null() {
                drop(Box::from_raw(state));
                (s.pw_thread_loop_destroy)(thread_loop);
                return Err("pw_stream_new_simple failed".into());
            }
            (*state).stream = stream;
            let pod = build_rgba_format_pod(width, height, fps);
            let mut params = [pod.as_ptr() as *const c_void];
            let rc = (s.pw_stream_connect)(
                stream,
                PW_DIRECTION_OUTPUT,
                PW_ID_ANY,
                PW_STREAM_FLAG_MAP_BUFFERS | PW_STREAM_FLAG_RT_PROCESS,
                params.as_mut_ptr(),
                params.len() as u32,
            );
            if rc < 0 {
                (s.pw_stream_destroy)(stream);
                drop(Box::from_raw(state));
                (s.pw_thread_loop_destroy)(thread_loop);
                return Err(format!("pw_stream_connect failed: {rc}"));
            }
            let core = (s.pw_stream_get_core)(stream);
            let listener_rc = if core.is_null() {
                -1
            } else {
                (s.pw_core_add_listener)(
                    core,
                    &mut (*state).core_hook,
                    &RAW_CORE_EVENTS,
                    state.cast(),
                )
            };
            if listener_rc < 0 {
                (s.pw_stream_disconnect)(stream);
                (s.pw_stream_destroy)(stream);
                drop(Box::from_raw(state));
                (s.pw_thread_loop_destroy)(thread_loop);
                return Err("pw_core_add_listener failed".into());
            }
            if (s.pw_thread_loop_start)(thread_loop) < 0 {
                (s.pw_stream_disconnect)(stream);
                (s.pw_stream_destroy)(stream);
                drop(Box::from_raw(state));
                (s.pw_thread_loop_destroy)(thread_loop);
                return Err("pw_thread_loop_start failed".into());
            }
            let source = Self {
                thread_loop,
                stream,
                state,
                width,
                height,
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while (source.node_id() == PW_ID_ANY || source.serial() == 0)
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            if source.node_id() == PW_ID_ANY {
                return Err("PipeWire did not publish the ScreenCast node".into());
            }
            if source.serial() == 0 {
                return Err("PipeWire did not report the ScreenCast node serial".into());
            }
            Ok(source)
        }
    }

    pub fn node_id(&self) -> u32 {
        unsafe { (*self.state).node_id.load(Ordering::Acquire) }
    }

    pub fn serial(&self) -> u64 {
        unsafe { (*self.state).serial.load(Ordering::Acquire) }
    }

    pub fn push(&self, rgba: Vec<u8>) -> Result<(), &'static str> {
        self.enqueue(rgba, SPA_TIME_INVALID)
    }

    /// Enqueue one ScreenCast frame with the compositor's wrapping monotonic
    /// timestamp. It is expanded into PipeWire's nanosecond clock domain;
    /// sequence numbers are assigned here so dropped queue entries remain
    /// visible as gaps to consumers.
    pub fn push_timed(
        &self,
        rgba: Vec<u8>,
        timestamp_ms: u32,
        timestamp_sub_us: u16,
    ) -> Result<(), &'static str> {
        let pts_ns = monotonic_now_ns().map_or(SPA_TIME_INVALID, |now| {
            wrapped_timestamp_to_pts_ns(timestamp_ms, timestamp_sub_us, now)
        });
        self.enqueue(rgba, pts_ns)
    }

    fn enqueue(&self, rgba: Vec<u8>, pts_ns: i64) -> Result<(), &'static str> {
        let expected = usize::from(self.width) * usize::from(self.height) * 4;
        if rgba.len() != expected {
            return Err("RGBA frame dimensions changed");
        }
        let state = unsafe { &*self.state };
        let sequence = state.next_sequence.fetch_add(1, Ordering::Relaxed);
        let mut frames = state.frames.lock().map_err(|_| "source queue poisoned")?;
        if frames.len() == 3 {
            frames.pop_front();
        }
        frames.push_back(RawVideoFrame {
            rgba,
            pts_ns,
            sequence,
        });
        Ok(())
    }
}

impl Drop for RawVideoSource {
    fn drop(&mut self) {
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
            }
            if !self.thread_loop.is_null() {
                (s.pw_thread_loop_destroy)(self.thread_loop);
            }
            if !self.state.is_null() {
                drop(Box::from_raw(self.state));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Child, Command, Stdio};

    struct TestPipeWire {
        child: Child,
        root: std::path::PathBuf,
    }

    impl TestPipeWire {
        fn spawn() -> Option<Self> {
            if Command::new("pipewire")
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_err()
            {
                return None;
            }
            let root = std::path::Path::new("/tmp").join(format!(
                "blit-pipewire-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock before epoch")
                    .as_nanos()
            ));
            std::fs::create_dir(&root).expect("create PipeWire test runtime");
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
                .expect("chmod PipeWire test runtime");
            let child = Command::new("pipewire")
                .env("XDG_RUNTIME_DIR", &root)
                .env("PIPEWIRE_RUNTIME_DIR", &root)
                .env_remove("PIPEWIRE_REMOTE")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("start test PipeWire daemon");
            let mut daemon = Self { child, root };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !daemon.root.join("pipewire-0").exists() && std::time::Instant::now() < deadline {
                if let Some(status) = daemon.child.try_wait().expect("poll PipeWire daemon") {
                    panic!("test PipeWire daemon exited early: {status}");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(
                daemon.root.join("pipewire-0").exists(),
                "test PipeWire socket was not created"
            );
            Some(daemon)
        }
    }

    impl Drop for TestPipeWire {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn pcm_request_is_sample_aligned_and_capacity_bounded() {
        assert_eq!(pcm_requested_bytes(0, 4_095), 4_094);
        assert_eq!(pcm_requested_bytes(960, 4_096), 1_920);
        assert_eq!(pcm_requested_bytes(8_000, 4_095), 4_094);
        assert_eq!(pcm_requested_bytes(u64::MAX, 4_096), 4_096);
        assert_eq!(pcm_requested_bytes(1, 1), 0);
    }

    #[test]
    fn header_meta_pod_requests_spa_header() {
        let pod = build_header_meta_pod();
        let bytes = unsafe { std::slice::from_raw_parts(pod.as_ptr().cast::<u8>(), pod.len() * 8) };
        let word =
            |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        assert_eq!(word(4), SPA_TYPE_OBJECT);
        assert_eq!(word(8), SPA_TYPE_OBJECT_PARAM_META);
        assert_eq!(word(12), SPA_PARAM_META);
        assert_eq!(word(16), SPA_PARAM_META_TYPE);
        assert_eq!(word(28), SPA_TYPE_ID);
        assert_eq!(word(32), SPA_META_HEADER);
        assert_eq!(word(40), SPA_PARAM_META_SIZE);
        assert_eq!(word(52), SPA_TYPE_INT);
        assert_eq!(word(56), std::mem::size_of::<SpaMetaHeader>() as u32);
    }

    #[test]
    fn raw_video_buffers_pod_fixes_three_tightly_packed_buffers() {
        let pod = build_raw_video_buffers_pod(64 * 48 * 4, 64 * 4);
        let bytes = unsafe { std::slice::from_raw_parts(pod.as_ptr().cast::<u8>(), pod.len() * 8) };
        let word =
            |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        assert_eq!(word(4), SPA_TYPE_OBJECT);
        assert_eq!(word(8), SPA_TYPE_OBJECT_PARAM_BUFFERS);
        assert_eq!(word(12), SPA_PARAM_BUFFERS);
        assert_eq!(word(16), SPA_PARAM_BUFFERS_BUFFERS);
        assert_eq!(word(32), 3);
        assert_eq!(word(40), SPA_PARAM_BUFFERS_BLOCKS);
        assert_eq!(word(56), 1);
        assert_eq!(word(64), SPA_PARAM_BUFFERS_SIZE);
        assert_eq!(word(80), 64 * 48 * 4);
        assert_eq!(word(88), SPA_PARAM_BUFFERS_STRIDE);
        assert_eq!(word(104), 64 * 4);
    }

    #[test]
    fn raw_video_queue_is_newest_frame_wins() {
        let frame = |value, pts_ns, sequence| RawVideoFrame {
            rgba: vec![value],
            pts_ns,
            sequence,
        };
        let frames = Mutex::new(VecDeque::from([
            frame(1, 10, 0),
            frame(2, 20, 1),
            frame(3, 30, 2),
        ]));
        assert_eq!(take_latest_frame(&frames), Some(frame(3, 30, 2)));
        assert!(frames.lock().unwrap().is_empty());
    }

    #[test]
    fn raw_video_header_carries_pts_and_sequence_or_gap() {
        let frame = RawVideoFrame {
            rgba: vec![0; 4],
            pts_ns: 12_345_678,
            sequence: 7,
        };
        let mut header = SpaMetaHeader {
            flags: u32::MAX,
            offset: u32::MAX,
            pts: 0,
            dts_offset: -1,
            seq: 0,
        };
        populate_video_header(&mut header, Some(&frame));
        assert_eq!(header.flags, 0);
        assert_eq!(header.offset, 0);
        assert_eq!(header.pts, frame.pts_ns);
        assert_eq!(header.dts_offset, 0);
        assert_eq!(header.seq, frame.sequence);

        populate_video_header(&mut header, None);
        assert_eq!(header.flags, SPA_META_HEADER_FLAG_GAP);
        assert_eq!(header.pts, SPA_TIME_INVALID);
        assert_eq!(header.seq, 0);
    }

    #[test]
    fn wrapped_compositor_timestamp_expands_into_current_monotonic_epoch() {
        let after_wrap_ms = WRAPPED_MS_PERIOD + 123;
        let now_ns = after_wrap_ms * 1_000_000 + 900_000;
        assert_eq!(
            wrapped_timestamp_to_pts_ns(100, 456, now_ns),
            (WRAPPED_MS_PERIOD + 100) * 1_000_000 + 456_000
        );
        assert_eq!(
            wrapped_timestamp_to_pts_ns(u32::MAX, 999, now_ns),
            (WRAPPED_MS_PERIOD - 1) * 1_000_000 + 999_000
        );
        assert_eq!(
            wrapped_timestamp_to_pts_ns(123, u16::MAX, now_ns),
            (WRAPPED_MS_PERIOD + 123) * 1_000_000 + 999_000
        );
    }

    #[test]
    #[ignore = "requires a local PipeWire daemon binary"]
    fn raw_source_publishes_id_serial_and_rgba_format() {
        let Some(daemon) = TestPipeWire::spawn() else {
            eprintln!("skipped: PipeWire daemon could not start");
            return;
        };
        let source = RawVideoSource::start_camera(&daemon.root, 64, 48, 15).unwrap();
        assert_ne!(source.node_id(), PW_ID_ANY);
        assert_ne!(source.serial(), 0);
        source.push(vec![0x80; 64 * 48 * 4]).unwrap();
    }
}
