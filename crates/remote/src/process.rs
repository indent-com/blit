//! Native non-PTY process wire protocol (docs/design/processes.md).
//!
//! Processes use binary argv, environment, stdin, stdout, and stderr on Unix.
//! Windows host validation is explicit because a client library's compile
//! target need not match its server. All integers are little-endian and every
//! parser rejects trailing bytes on the fixed-shape messages.

use crate::{STATUS_INVALID, STATUS_OK, STATUS_TOO_LARGE};

/// `S2C_HELLO` feature bit: native non-PTY child processes.
pub const FEATURE_PROCESS: u32 = 1 << 13;

// Direction-local process-family opcodes.
pub const C2S_PROCESS_SPAWN: u8 = 0xC0;
pub const C2S_PROCESS_STDIN: u8 = 0xC1;
pub const C2S_PROCESS_OUTPUT_ACK: u8 = 0xC2;
pub const C2S_PROCESS_CONTROL: u8 = 0xC3;
pub const C2S_PROCESS_LIST: u8 = 0xC4;
pub const C2S_PROCESS_WATCH: u8 = 0xC5;

pub const S2C_PROCESS_STARTED: u8 = 0xC0;
pub const S2C_PROCESS_STDOUT: u8 = 0xC1;
pub const S2C_PROCESS_STDERR: u8 = 0xC2;
pub const S2C_PROCESS_STDIN_ACK: u8 = 0xC3;
pub const S2C_PROCESS_EXIT: u8 = 0xC4;
pub const S2C_PROCESS_CONTROLLED: u8 = 0xC5;
pub const S2C_PROCESS_LISTED: u8 = 0xC6;
pub const S2C_PROCESS_WATCHED: u8 = 0xC7;

pub const PROCESS_SPAWN_MERGE_STDERR: u8 = 1 << 0;
pub const PROCESS_SPAWN_DETACHABLE: u8 = 1 << 1;
pub const PROCESS_SPAWN_FLAGS: u8 = PROCESS_SPAWN_MERGE_STDERR | PROCESS_SPAWN_DETACHABLE;

/// Request the process's single vacant stdin-writer role while watching.
pub const PROCESS_WATCH_STDIN: u8 = 1 << 0;
pub const PROCESS_WATCH_FLAGS: u8 = PROCESS_WATCH_STDIN;

pub const PROCESS_CWD_DEFAULT: u8 = 0;
pub const PROCESS_CWD_EXPLICIT: u8 = 1;
pub const PROCESS_CWD_FROM_PTY: u8 = 2;

pub const PROCESS_STREAM_STDOUT: u8 = 1;
pub const PROCESS_STREAM_STDERR: u8 = 2;

pub const PROCESS_CONTROL_CLOSE_STDIN: u8 = 1;
pub const PROCESS_CONTROL_TERMINATE: u8 = 2;
pub const PROCESS_CONTROL_KILL: u8 = 3;
pub const PROCESS_CONTROL_SIGNAL: u8 = 4;
pub const PROCESS_CONTROL_UNWATCH: u8 = 5;

pub const PROCESS_STATE_RUNNING: u8 = 1;
pub const PROCESS_STATE_EXITED: u8 = 2;

pub const PROCESS_STREAM_STDIN_ACCEPTING: u8 = 1 << 0;
pub const PROCESS_STREAM_STDIN_CLOSING: u8 = 1 << 1;
pub const PROCESS_STREAM_STDIN_CLOSED: u8 = 1 << 2;
pub const PROCESS_STREAM_STDOUT_OPEN: u8 = 1 << 3;
pub const PROCESS_STREAM_STDERR_OPEN: u8 = 1 << 4;
pub const PROCESS_STREAM_MERGED_STDERR: u8 = 1 << 5;
pub const PROCESS_STREAM_STDIN_WRITABLE: u8 = 1 << 6;
pub const PROCESS_STREAM_STATE_FLAGS: u8 = PROCESS_STREAM_STDIN_ACCEPTING
    | PROCESS_STREAM_STDIN_CLOSING
    | PROCESS_STREAM_STDIN_CLOSED
    | PROCESS_STREAM_STDOUT_OPEN
    | PROCESS_STREAM_STDERR_OPEN
    | PROCESS_STREAM_MERGED_STDERR
    | PROCESS_STREAM_STDIN_WRITABLE;

pub const PROCESS_STDIN_ACCEPTING: u8 = 1;
pub const PROCESS_STDIN_CLOSING: u8 = 2;
pub const PROCESS_STDIN_CLOSED: u8 = 3;

pub const PROCESS_EXIT_RETURNED: u8 = 0;
pub const PROCESS_EXIT_SIGNALLED: u8 = 1;
pub const PROCESS_EXIT_KILLED: u8 = 2;
pub const PROCESS_EXIT_PROTOCOL_VIOLATION: u8 = 3;
pub const PROCESS_EXIT_HOST_FAILURE: u8 = 4;

pub const PROCESS_KILL_UNSPECIFIED: u8 = 0;
pub const PROCESS_KILL_CLIENT: u8 = 1;
pub const PROCESS_KILL_OWNER_LOST: u8 = 2;
pub const PROCESS_KILL_TERMINATE_TIMEOUT: u8 = 3;
pub const PROCESS_KILL_SERVER_SHUTDOWN: u8 = 4;

pub const PROCESS_MAX_ARGC: usize = 1_024;
pub const PROCESS_MAX_ARG_LEN: usize = 64 * 1024;
pub const PROCESS_MAX_ARG_BYTES: usize = 1024 * 1024;
pub const PROCESS_MAX_ENVC: usize = 256;
pub const PROCESS_MAX_ENV_KEY_LEN: usize = 255;
pub const PROCESS_MAX_ENV_VALUE_LEN: usize = 64 * 1024;
pub const PROCESS_MAX_ENV_BYTES: usize = 1024 * 1024;
pub const PROCESS_MAX_CWD_LEN: usize = 4 * 1024;
pub const PROCESS_MAX_STREAM_PAYLOAD: usize = 256 * 1024;
pub const PROCESS_MAX_DETAIL_LEN: usize = 4 * 1024;
pub const PROCESS_MAX_LIST_ENTRIES: usize = 4_096;
pub const PROCESS_MAX_LIST_BYTES: usize = 8 * 1024 * 1024;
pub const PROCESS_MAX_UNACKED_PACKETS: usize = 1_024;
pub const PROCESS_DEFAULT_STREAM_WINDOW: u64 = 1024 * 1024;

/// Server-boot-scoped identity for a native process generation. Zero is never
/// a valid process reference.
pub type ProcessRef = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessCodecError {
    /// Wrong opcode, a truncated field, or trailing bytes on a fixed message.
    Malformed,
    /// A complete message whose flags or field combination is invalid.
    Invalid,
    /// A complete length prefix or count exceeds a protocol cap.
    TooLarge,
}

impl ProcessCodecError {
    /// Common-registry status used by a correlated refusal.
    pub fn status(self) -> u8 {
        match self {
            Self::TooLarge => STATUS_TOO_LARGE,
            Self::Malformed | Self::Invalid => STATUS_INVALID,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSpawnRequest<'a> {
    pub nonce: u16,
    pub process_id: u32,
    pub flags: u8,
    pub cwd_kind: u8,
    pub src_pty_id: u16,
    pub cwd: &'a [u8],
    pub argv: Vec<&'a [u8]>,
    pub env: Vec<(&'a [u8], &'a [u8])>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessStdin<'a> {
    pub process_id: u32,
    pub offset: u64,
    pub data: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessOutputAck {
    pub process_id: u32,
    pub stream: u8,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessControl {
    pub nonce: u16,
    pub process_id: u32,
    pub action: u8,
    pub value: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessList {
    pub nonce: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessWatch {
    pub nonce: u16,
    pub process_id: u32,
    pub process_ref: ProcessRef,
    pub flags: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessStarted<'a> {
    pub nonce: u16,
    pub status: u8,
    pub process_id: u32,
    pub process_ref: ProcessRef,
    pub stdin_window: u64,
    pub stdout_window: u64,
    pub stderr_window: u64,
    pub detail: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessOutput<'a> {
    pub process_id: u32,
    pub offset: u64,
    pub data: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessStdinAck {
    pub process_id: u32,
    pub bytes: u64,
    pub stdin_state: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessExit<'a> {
    pub process_id: u32,
    pub reason: u8,
    pub kill_cause: u8,
    pub code: u32,
    pub detail: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessControlled<'a> {
    pub nonce: u16,
    pub status: u8,
    pub process_id: u32,
    pub detail: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessListEntry<'a> {
    pub process_ref: ProcessRef,
    pub state: u8,
    pub flags: u8,
    pub pid: u32,
    pub argv0: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessListed<'a> {
    pub nonce: u16,
    pub status: u8,
    pub revision: u64,
    pub entries: Vec<ProcessListEntry<'a>>,
    pub detail: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessWatched<'a> {
    pub nonce: u16,
    pub status: u8,
    pub process_id: u32,
    pub process_ref: ProcessRef,
    pub state: u8,
    pub stream_state: u8,
    pub stdin_received: u64,
    pub stdin_acked: u64,
    pub stdout_next: u64,
    pub stderr_next: u64,
    pub stdin_window: u64,
    pub stdout_window: u64,
    pub stderr_window: u64,
    pub exit_reason: u8,
    pub kill_cause: u8,
    pub exit_code: u32,
    pub detail: &'a str,
}

fn body_of(msg: &[u8], opcode: u8) -> Result<&[u8], ProcessCodecError> {
    if msg.first() != Some(&opcode) {
        return Err(ProcessCodecError::Malformed);
    }
    Ok(&msg[1..])
}

fn take<const N: usize>(body: &mut &[u8]) -> Result<[u8; N], ProcessCodecError> {
    let bytes = body
        .get(..N)
        .ok_or(ProcessCodecError::Malformed)?
        .try_into()
        .unwrap();
    *body = &body[N..];
    Ok(bytes)
}

fn take_u8(body: &mut &[u8]) -> Result<u8, ProcessCodecError> {
    Ok(take::<1>(body)?[0])
}

fn take_u16(body: &mut &[u8]) -> Result<u16, ProcessCodecError> {
    Ok(u16::from_le_bytes(take(body)?))
}

fn take_u32(body: &mut &[u8]) -> Result<u32, ProcessCodecError> {
    Ok(u32::from_le_bytes(take(body)?))
}

fn take_u64(body: &mut &[u8]) -> Result<u64, ProcessCodecError> {
    Ok(u64::from_le_bytes(take(body)?))
}

fn take_len<'a>(
    body: &mut &'a [u8],
    len: usize,
    max: usize,
) -> Result<&'a [u8], ProcessCodecError> {
    if len > max {
        return Err(ProcessCodecError::TooLarge);
    }
    let value = body.get(..len).ok_or(ProcessCodecError::Malformed)?;
    *body = &body[len..];
    Ok(value)
}

fn finish(body: &[u8]) -> Result<(), ProcessCodecError> {
    if body.is_empty() {
        Ok(())
    } else {
        Err(ProcessCodecError::Malformed)
    }
}

fn has_nul(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

fn validate_spawn(req: &ProcessSpawnRequest<'_>) -> Result<(), ProcessCodecError> {
    if req.flags & !PROCESS_SPAWN_FLAGS != 0 {
        return Err(ProcessCodecError::Invalid);
    }
    match req.cwd_kind {
        PROCESS_CWD_DEFAULT if req.src_pty_id == 0 && req.cwd.is_empty() => {}
        PROCESS_CWD_EXPLICIT
            if req.src_pty_id == 0
                && !req.cwd.is_empty()
                && req.cwd.len() <= PROCESS_MAX_CWD_LEN
                && !has_nul(req.cwd) => {}
        // PTY ids are client-visible u16 values and zero is a valid id. The
        // field is required for this shape, but has no sentinel value.
        PROCESS_CWD_FROM_PTY if req.cwd.is_empty() => {}
        PROCESS_CWD_EXPLICIT if req.cwd.len() > PROCESS_MAX_CWD_LEN => {
            return Err(ProcessCodecError::TooLarge);
        }
        _ => return Err(ProcessCodecError::Invalid),
    }
    if req.argv.is_empty() {
        return Err(ProcessCodecError::Invalid);
    }
    if req.argv.len() > PROCESS_MAX_ARGC || req.env.len() > PROCESS_MAX_ENVC {
        return Err(ProcessCodecError::TooLarge);
    }
    let mut arg_bytes = 0usize;
    for arg in &req.argv {
        if arg.len() > PROCESS_MAX_ARG_LEN {
            return Err(ProcessCodecError::TooLarge);
        }
        arg_bytes = arg_bytes
            .checked_add(arg.len())
            .ok_or(ProcessCodecError::TooLarge)?;
        if arg_bytes > PROCESS_MAX_ARG_BYTES {
            return Err(ProcessCodecError::TooLarge);
        }
        if has_nul(arg) {
            return Err(ProcessCodecError::Invalid);
        }
    }
    let mut env_bytes = 0usize;
    for (i, (key, value)) in req.env.iter().enumerate() {
        if key.len() > PROCESS_MAX_ENV_KEY_LEN || value.len() > PROCESS_MAX_ENV_VALUE_LEN {
            return Err(ProcessCodecError::TooLarge);
        }
        env_bytes = env_bytes
            .checked_add(key.len())
            .and_then(|n| n.checked_add(value.len()))
            .ok_or(ProcessCodecError::TooLarge)?;
        if env_bytes > PROCESS_MAX_ENV_BYTES {
            return Err(ProcessCodecError::TooLarge);
        }
        if has_nul(key) || key.contains(&b'=') || has_nul(value) {
            return Err(ProcessCodecError::Invalid);
        }
        if req.env[..i].iter().any(|(prior, _)| prior == key) {
            return Err(ProcessCodecError::Invalid);
        }
    }
    Ok(())
}

/// Apply the Windows execution host's native-string restrictions.
///
/// Call this after [`parse_process_spawn`] and before reserving a generation.
/// Unix needs no second pass: it accepts the raw byte form already validated
/// by the wire parser. `env_keys_equal` must implement Windows' native
/// case-insensitive environment-key comparison; keeping it caller-supplied
/// avoids substituting Unicode case folding for the OS rule.
pub fn validate_process_spawn_for_windows(
    req: &ProcessSpawnRequest<'_>,
    mut env_keys_equal: impl FnMut(&str, &str) -> bool,
) -> Result<(), ProcessCodecError> {
    validate_spawn(req)?;
    if req.argv.iter().any(|v| std::str::from_utf8(v).is_err())
        || req
            .env
            .iter()
            .any(|(k, v)| std::str::from_utf8(k).is_err() || std::str::from_utf8(v).is_err())
        || (req.cwd_kind == PROCESS_CWD_EXPLICIT && std::str::from_utf8(req.cwd).is_err())
    {
        return Err(ProcessCodecError::Invalid);
    }
    for (i, (key, _)) in req.env.iter().enumerate() {
        let key = std::str::from_utf8(key).expect("UTF-8 checked above");
        if req.env[..i]
            .iter()
            .map(|(prior, _)| std::str::from_utf8(prior).expect("UTF-8 checked above"))
            .any(|prior| env_keys_equal(prior, key))
        {
            return Err(ProcessCodecError::Invalid);
        }
    }
    Ok(())
}

pub fn msg_process_spawn(req: &ProcessSpawnRequest<'_>) -> Result<Vec<u8>, ProcessCodecError> {
    validate_spawn(req)?;
    let mut msg = Vec::with_capacity(
        32 + req.cwd.len()
            + req.argv.iter().map(|v| 4 + v.len()).sum::<usize>()
            + req
                .env
                .iter()
                .map(|(k, v)| 6 + k.len() + v.len())
                .sum::<usize>(),
    );
    msg.push(C2S_PROCESS_SPAWN);
    msg.extend_from_slice(&req.nonce.to_le_bytes());
    msg.extend_from_slice(&req.process_id.to_le_bytes());
    msg.push(req.flags);
    msg.push(req.cwd_kind);
    msg.extend_from_slice(&req.src_pty_id.to_le_bytes());
    msg.extend_from_slice(&(req.cwd.len() as u32).to_le_bytes());
    msg.extend_from_slice(req.cwd);
    msg.extend_from_slice(&(req.argv.len() as u16).to_le_bytes());
    for arg in &req.argv {
        msg.extend_from_slice(&(arg.len() as u32).to_le_bytes());
        msg.extend_from_slice(arg);
    }
    msg.extend_from_slice(&(req.env.len() as u16).to_le_bytes());
    for (key, value) in &req.env {
        msg.extend_from_slice(&(key.len() as u16).to_le_bytes());
        msg.extend_from_slice(key);
        msg.extend_from_slice(&(value.len() as u32).to_le_bytes());
        msg.extend_from_slice(value);
    }
    Ok(msg)
}

pub fn parse_process_spawn(msg: &[u8]) -> Result<ProcessSpawnRequest<'_>, ProcessCodecError> {
    let mut body = body_of(msg, C2S_PROCESS_SPAWN)?;
    let nonce = take_u16(&mut body)?;
    let process_id = take_u32(&mut body)?;
    let flags = take_u8(&mut body)?;
    let cwd_kind = take_u8(&mut body)?;
    let src_pty_id = take_u16(&mut body)?;
    let cwd_len = take_u32(&mut body)? as usize;
    let cwd = take_len(&mut body, cwd_len, PROCESS_MAX_CWD_LEN)?;
    let argc = take_u16(&mut body)? as usize;
    if argc > PROCESS_MAX_ARGC {
        return Err(ProcessCodecError::TooLarge);
    }
    let mut argv = Vec::with_capacity(argc);
    let mut arg_bytes = 0usize;
    for _ in 0..argc {
        let len = take_u32(&mut body)? as usize;
        arg_bytes = arg_bytes
            .checked_add(len)
            .ok_or(ProcessCodecError::TooLarge)?;
        if arg_bytes > PROCESS_MAX_ARG_BYTES {
            return Err(ProcessCodecError::TooLarge);
        }
        argv.push(take_len(&mut body, len, PROCESS_MAX_ARG_LEN)?);
    }
    let envc = take_u16(&mut body)? as usize;
    if envc > PROCESS_MAX_ENVC {
        return Err(ProcessCodecError::TooLarge);
    }
    let mut env = Vec::with_capacity(envc);
    let mut env_bytes = 0usize;
    for _ in 0..envc {
        let key_len = take_u16(&mut body)? as usize;
        let key = take_len(&mut body, key_len, PROCESS_MAX_ENV_KEY_LEN)?;
        let value_len = take_u32(&mut body)? as usize;
        env_bytes = env_bytes
            .checked_add(key_len)
            .and_then(|n| n.checked_add(value_len))
            .ok_or(ProcessCodecError::TooLarge)?;
        if env_bytes > PROCESS_MAX_ENV_BYTES {
            return Err(ProcessCodecError::TooLarge);
        }
        let value = take_len(&mut body, value_len, PROCESS_MAX_ENV_VALUE_LEN)?;
        env.push((key, value));
    }
    finish(body)?;
    let req = ProcessSpawnRequest {
        nonce,
        process_id,
        flags,
        cwd_kind,
        src_pty_id,
        cwd,
        argv,
        env,
    };
    validate_spawn(&req)?;
    Ok(req)
}

pub fn msg_process_stdin(input: ProcessStdin<'_>) -> Result<Vec<u8>, ProcessCodecError> {
    if input.data.is_empty() {
        return Err(ProcessCodecError::Invalid);
    }
    if input.data.len() > PROCESS_MAX_STREAM_PAYLOAD {
        return Err(ProcessCodecError::TooLarge);
    }
    let mut msg = Vec::with_capacity(13 + input.data.len());
    msg.push(C2S_PROCESS_STDIN);
    msg.extend_from_slice(&input.process_id.to_le_bytes());
    msg.extend_from_slice(&input.offset.to_le_bytes());
    msg.extend_from_slice(input.data);
    Ok(msg)
}

pub fn parse_process_stdin(msg: &[u8]) -> Result<ProcessStdin<'_>, ProcessCodecError> {
    let mut body = body_of(msg, C2S_PROCESS_STDIN)?;
    let process_id = take_u32(&mut body)?;
    let offset = take_u64(&mut body)?;
    if body.is_empty() {
        return Err(ProcessCodecError::Invalid);
    }
    if body.len() > PROCESS_MAX_STREAM_PAYLOAD {
        return Err(ProcessCodecError::TooLarge);
    }
    Ok(ProcessStdin {
        process_id,
        offset,
        data: body,
    })
}

pub fn msg_process_output_ack(ack: ProcessOutputAck) -> Result<Vec<u8>, ProcessCodecError> {
    if !matches!(ack.stream, PROCESS_STREAM_STDOUT | PROCESS_STREAM_STDERR) {
        return Err(ProcessCodecError::Invalid);
    }
    let mut msg = Vec::with_capacity(14);
    msg.push(C2S_PROCESS_OUTPUT_ACK);
    msg.extend_from_slice(&ack.process_id.to_le_bytes());
    msg.push(ack.stream);
    msg.extend_from_slice(&ack.bytes.to_le_bytes());
    Ok(msg)
}

pub fn parse_process_output_ack(msg: &[u8]) -> Result<ProcessOutputAck, ProcessCodecError> {
    let mut body = body_of(msg, C2S_PROCESS_OUTPUT_ACK)?;
    let ack = ProcessOutputAck {
        process_id: take_u32(&mut body)?,
        stream: take_u8(&mut body)?,
        bytes: take_u64(&mut body)?,
    };
    finish(body)?;
    if !matches!(ack.stream, PROCESS_STREAM_STDOUT | PROCESS_STREAM_STDERR) {
        return Err(ProcessCodecError::Invalid);
    }
    Ok(ack)
}

fn validate_control(control: ProcessControl) -> Result<(), ProcessCodecError> {
    match control.action {
        PROCESS_CONTROL_CLOSE_STDIN
        | PROCESS_CONTROL_TERMINATE
        | PROCESS_CONTROL_KILL
        | PROCESS_CONTROL_UNWATCH
            if control.value == 0 =>
        {
            Ok(())
        }
        PROCESS_CONTROL_SIGNAL if control.value != 0 => Ok(()),
        PROCESS_CONTROL_CLOSE_STDIN
        | PROCESS_CONTROL_TERMINATE
        | PROCESS_CONTROL_KILL
        | PROCESS_CONTROL_SIGNAL
        | PROCESS_CONTROL_UNWATCH => Err(ProcessCodecError::Invalid),
        _ => Err(ProcessCodecError::Invalid),
    }
}

pub fn msg_process_control(control: ProcessControl) -> Result<Vec<u8>, ProcessCodecError> {
    validate_control(control)?;
    let mut msg = Vec::with_capacity(12);
    msg.push(C2S_PROCESS_CONTROL);
    msg.extend_from_slice(&control.nonce.to_le_bytes());
    msg.extend_from_slice(&control.process_id.to_le_bytes());
    msg.push(control.action);
    msg.extend_from_slice(&control.value.to_le_bytes());
    Ok(msg)
}

pub fn parse_process_control(msg: &[u8]) -> Result<ProcessControl, ProcessCodecError> {
    let mut body = body_of(msg, C2S_PROCESS_CONTROL)?;
    let control = ProcessControl {
        nonce: take_u16(&mut body)?,
        process_id: take_u32(&mut body)?,
        action: take_u8(&mut body)?,
        value: take_u32(&mut body)?,
    };
    finish(body)?;
    validate_control(control)?;
    Ok(control)
}

pub fn msg_process_list(list: ProcessList) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_PROCESS_LIST);
    msg.extend_from_slice(&list.nonce.to_le_bytes());
    msg
}

pub fn parse_process_list(msg: &[u8]) -> Result<ProcessList, ProcessCodecError> {
    let mut body = body_of(msg, C2S_PROCESS_LIST)?;
    let list = ProcessList {
        nonce: take_u16(&mut body)?,
    };
    finish(body)?;
    Ok(list)
}

pub fn msg_process_watch(watch: ProcessWatch) -> Result<Vec<u8>, ProcessCodecError> {
    if watch.process_ref == 0 || watch.flags & !PROCESS_WATCH_FLAGS != 0 {
        return Err(ProcessCodecError::Invalid);
    }
    let mut msg = Vec::with_capacity(16);
    msg.push(C2S_PROCESS_WATCH);
    msg.extend_from_slice(&watch.nonce.to_le_bytes());
    msg.extend_from_slice(&watch.process_id.to_le_bytes());
    msg.extend_from_slice(&watch.process_ref.to_le_bytes());
    msg.push(watch.flags);
    Ok(msg)
}

pub fn parse_process_watch(msg: &[u8]) -> Result<ProcessWatch, ProcessCodecError> {
    let mut body = body_of(msg, C2S_PROCESS_WATCH)?;
    let watch = ProcessWatch {
        nonce: take_u16(&mut body)?,
        process_id: take_u32(&mut body)?,
        process_ref: take_u64(&mut body)?,
        flags: take_u8(&mut body)?,
    };
    finish(body)?;
    if watch.process_ref == 0 || watch.flags & !PROCESS_WATCH_FLAGS != 0 {
        return Err(ProcessCodecError::Invalid);
    }
    Ok(watch)
}

/// Whether `opcode` belongs to the client-to-server process family.
pub fn is_c2s_process(opcode: u8) -> bool {
    matches!(
        opcode,
        C2S_PROCESS_SPAWN
            | C2S_PROCESS_STDIN
            | C2S_PROCESS_OUTPUT_ACK
            | C2S_PROCESS_CONTROL
            | C2S_PROCESS_LIST
            | C2S_PROCESS_WATCH
    )
}

fn detail_bytes(detail: &str) -> &[u8] {
    if detail.len() <= PROCESS_MAX_DETAIL_LEN {
        return detail.as_bytes();
    }
    let mut end = PROCESS_MAX_DETAIL_LEN;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    &detail.as_bytes()[..end]
}

fn take_detail(body: &[u8]) -> Result<&str, ProcessCodecError> {
    if body.len() > PROCESS_MAX_DETAIL_LEN {
        return Err(ProcessCodecError::TooLarge);
    }
    std::str::from_utf8(body).map_err(|_| ProcessCodecError::Invalid)
}

fn validate_started(started: &ProcessStarted<'_>) -> Result<(), ProcessCodecError> {
    if (started.status == STATUS_OK) == (started.process_ref == 0)
        || (started.status != STATUS_OK
            && (started.stdin_window != 0
                || started.stdout_window != 0
                || started.stderr_window != 0))
    {
        return Err(ProcessCodecError::Invalid);
    }
    Ok(())
}

pub fn msg_process_started(started: ProcessStarted<'_>) -> Result<Vec<u8>, ProcessCodecError> {
    validate_started(&started)?;
    let detail = detail_bytes(started.detail);
    let mut msg = Vec::with_capacity(40 + detail.len());
    msg.push(S2C_PROCESS_STARTED);
    msg.extend_from_slice(&started.nonce.to_le_bytes());
    msg.push(started.status);
    msg.extend_from_slice(&started.process_id.to_le_bytes());
    msg.extend_from_slice(&started.process_ref.to_le_bytes());
    msg.extend_from_slice(&started.stdin_window.to_le_bytes());
    msg.extend_from_slice(&started.stdout_window.to_le_bytes());
    msg.extend_from_slice(&started.stderr_window.to_le_bytes());
    msg.extend_from_slice(detail);
    Ok(msg)
}

pub fn parse_process_started(msg: &[u8]) -> Result<ProcessStarted<'_>, ProcessCodecError> {
    let mut body = body_of(msg, S2C_PROCESS_STARTED)?;
    let nonce = take_u16(&mut body)?;
    let status = take_u8(&mut body)?;
    let process_id = take_u32(&mut body)?;
    let process_ref = take_u64(&mut body)?;
    let stdin_window = take_u64(&mut body)?;
    let stdout_window = take_u64(&mut body)?;
    let stderr_window = take_u64(&mut body)?;
    let started = ProcessStarted {
        nonce,
        status,
        process_id,
        process_ref,
        stdin_window,
        stdout_window,
        stderr_window,
        detail: take_detail(body)?,
    };
    validate_started(&started)?;
    Ok(started)
}

fn msg_process_output(opcode: u8, output: ProcessOutput<'_>) -> Result<Vec<u8>, ProcessCodecError> {
    if output.data.is_empty() {
        return Err(ProcessCodecError::Invalid);
    }
    if output.data.len() > PROCESS_MAX_STREAM_PAYLOAD {
        return Err(ProcessCodecError::TooLarge);
    }
    let mut msg = Vec::with_capacity(13 + output.data.len());
    msg.push(opcode);
    msg.extend_from_slice(&output.process_id.to_le_bytes());
    msg.extend_from_slice(&output.offset.to_le_bytes());
    msg.extend_from_slice(output.data);
    Ok(msg)
}

fn parse_process_output(msg: &[u8], opcode: u8) -> Result<ProcessOutput<'_>, ProcessCodecError> {
    let mut body = body_of(msg, opcode)?;
    let process_id = take_u32(&mut body)?;
    let offset = take_u64(&mut body)?;
    if body.is_empty() {
        return Err(ProcessCodecError::Invalid);
    }
    if body.len() > PROCESS_MAX_STREAM_PAYLOAD {
        return Err(ProcessCodecError::TooLarge);
    }
    Ok(ProcessOutput {
        process_id,
        offset,
        data: body,
    })
}

pub fn msg_process_stdout(output: ProcessOutput<'_>) -> Result<Vec<u8>, ProcessCodecError> {
    msg_process_output(S2C_PROCESS_STDOUT, output)
}

pub fn parse_process_stdout(msg: &[u8]) -> Result<ProcessOutput<'_>, ProcessCodecError> {
    parse_process_output(msg, S2C_PROCESS_STDOUT)
}

pub fn msg_process_stderr(output: ProcessOutput<'_>) -> Result<Vec<u8>, ProcessCodecError> {
    msg_process_output(S2C_PROCESS_STDERR, output)
}

pub fn parse_process_stderr(msg: &[u8]) -> Result<ProcessOutput<'_>, ProcessCodecError> {
    parse_process_output(msg, S2C_PROCESS_STDERR)
}

pub fn msg_process_stdin_ack(ack: ProcessStdinAck) -> Result<Vec<u8>, ProcessCodecError> {
    if !matches!(
        ack.stdin_state,
        PROCESS_STDIN_ACCEPTING | PROCESS_STDIN_CLOSING | PROCESS_STDIN_CLOSED
    ) {
        return Err(ProcessCodecError::Invalid);
    }
    let mut msg = Vec::with_capacity(14);
    msg.push(S2C_PROCESS_STDIN_ACK);
    msg.extend_from_slice(&ack.process_id.to_le_bytes());
    msg.extend_from_slice(&ack.bytes.to_le_bytes());
    msg.push(ack.stdin_state);
    Ok(msg)
}

pub fn parse_process_stdin_ack(msg: &[u8]) -> Result<ProcessStdinAck, ProcessCodecError> {
    let mut body = body_of(msg, S2C_PROCESS_STDIN_ACK)?;
    let ack = ProcessStdinAck {
        process_id: take_u32(&mut body)?,
        bytes: take_u64(&mut body)?,
        stdin_state: take_u8(&mut body)?,
    };
    finish(body)?;
    if !matches!(
        ack.stdin_state,
        PROCESS_STDIN_ACCEPTING | PROCESS_STDIN_CLOSING | PROCESS_STDIN_CLOSED
    ) {
        return Err(ProcessCodecError::Invalid);
    }
    Ok(ack)
}

fn validate_exit(reason: u8, kill_cause: u8, code: u32) -> Result<(), ProcessCodecError> {
    match reason {
        PROCESS_EXIT_RETURNED if kill_cause == 0 => Ok(()),
        PROCESS_EXIT_SIGNALLED if kill_cause == 0 && code != 0 => Ok(()),
        PROCESS_EXIT_KILLED if code == 0 => Ok(()),
        PROCESS_EXIT_PROTOCOL_VIOLATION | PROCESS_EXIT_HOST_FAILURE
            if kill_cause == 0 && code == 0 =>
        {
            Ok(())
        }
        // Unknown reasons are terminal and retained verbatim for diagnostics.
        5..=u8::MAX => Ok(()),
        _ => Err(ProcessCodecError::Invalid),
    }
}

pub fn msg_process_exit(exit: ProcessExit<'_>) -> Result<Vec<u8>, ProcessCodecError> {
    validate_exit(exit.reason, exit.kill_cause, exit.code)?;
    let detail = detail_bytes(exit.detail);
    let mut msg = Vec::with_capacity(11 + detail.len());
    msg.push(S2C_PROCESS_EXIT);
    msg.extend_from_slice(&exit.process_id.to_le_bytes());
    msg.push(exit.reason);
    msg.push(exit.kill_cause);
    msg.extend_from_slice(&exit.code.to_le_bytes());
    msg.extend_from_slice(detail);
    Ok(msg)
}

pub fn parse_process_exit(msg: &[u8]) -> Result<ProcessExit<'_>, ProcessCodecError> {
    let mut body = body_of(msg, S2C_PROCESS_EXIT)?;
    let process_id = take_u32(&mut body)?;
    let reason = take_u8(&mut body)?;
    let kill_cause = take_u8(&mut body)?;
    let code = take_u32(&mut body)?;
    validate_exit(reason, kill_cause, code)?;
    Ok(ProcessExit {
        process_id,
        reason,
        kill_cause,
        code,
        detail: take_detail(body)?,
    })
}

pub fn msg_process_controlled(controlled: ProcessControlled<'_>) -> Vec<u8> {
    let detail = detail_bytes(controlled.detail);
    let mut msg = Vec::with_capacity(8 + detail.len());
    msg.push(S2C_PROCESS_CONTROLLED);
    msg.extend_from_slice(&controlled.nonce.to_le_bytes());
    msg.push(controlled.status);
    msg.extend_from_slice(&controlled.process_id.to_le_bytes());
    msg.extend_from_slice(detail);
    msg
}

pub fn parse_process_controlled(msg: &[u8]) -> Result<ProcessControlled<'_>, ProcessCodecError> {
    let mut body = body_of(msg, S2C_PROCESS_CONTROLLED)?;
    let nonce = take_u16(&mut body)?;
    let status = take_u8(&mut body)?;
    let process_id = take_u32(&mut body)?;
    Ok(ProcessControlled {
        nonce,
        status,
        process_id,
        detail: take_detail(body)?,
    })
}

fn validate_list_entry(entry: ProcessListEntry<'_>) -> Result<(), ProcessCodecError> {
    if entry.process_ref == 0
        || !matches!(entry.state, PROCESS_STATE_RUNNING | PROCESS_STATE_EXITED)
        || entry.flags & !PROCESS_SPAWN_FLAGS != 0
        || (entry.state == PROCESS_STATE_EXITED && entry.flags & PROCESS_SPAWN_DETACHABLE == 0)
        || has_nul(entry.argv0)
    {
        return Err(ProcessCodecError::Invalid);
    }
    if entry.argv0.len() > PROCESS_MAX_ARG_LEN {
        return Err(ProcessCodecError::TooLarge);
    }
    Ok(())
}

fn validate_listed(listed: &ProcessListed<'_>) -> Result<usize, ProcessCodecError> {
    if listed.status != STATUS_OK {
        if listed.revision != 0 || !listed.entries.is_empty() {
            return Err(ProcessCodecError::Invalid);
        }
        return Ok(14 + detail_bytes(listed.detail).len());
    }
    if !listed.detail.is_empty() {
        return Err(ProcessCodecError::Invalid);
    }
    if listed.entries.len() > PROCESS_MAX_LIST_ENTRIES {
        return Err(ProcessCodecError::TooLarge);
    }
    let mut len = 14usize;
    let mut previous_ref = 0;
    for entry in &listed.entries {
        validate_list_entry(*entry)?;
        if entry.process_ref <= previous_ref {
            return Err(ProcessCodecError::Invalid);
        }
        previous_ref = entry.process_ref;
        len = len
            .checked_add(18)
            .and_then(|n| n.checked_add(entry.argv0.len()))
            .ok_or(ProcessCodecError::TooLarge)?;
        if len > PROCESS_MAX_LIST_BYTES {
            return Err(ProcessCodecError::TooLarge);
        }
    }
    Ok(len)
}

pub fn msg_process_listed(listed: ProcessListed<'_>) -> Result<Vec<u8>, ProcessCodecError> {
    let len = validate_listed(&listed)?;
    let detail = detail_bytes(listed.detail);
    let mut msg = Vec::with_capacity(len);
    msg.push(S2C_PROCESS_LISTED);
    msg.extend_from_slice(&listed.nonce.to_le_bytes());
    msg.push(listed.status);
    msg.extend_from_slice(&listed.revision.to_le_bytes());
    msg.extend_from_slice(&(listed.entries.len() as u16).to_le_bytes());
    for entry in listed.entries {
        msg.extend_from_slice(&entry.process_ref.to_le_bytes());
        msg.push(entry.state);
        msg.push(entry.flags);
        msg.extend_from_slice(&entry.pid.to_le_bytes());
        msg.extend_from_slice(&(entry.argv0.len() as u32).to_le_bytes());
        msg.extend_from_slice(entry.argv0);
    }
    msg.extend_from_slice(detail);
    Ok(msg)
}

pub fn parse_process_listed(msg: &[u8]) -> Result<ProcessListed<'_>, ProcessCodecError> {
    if msg.len() > PROCESS_MAX_LIST_BYTES {
        return Err(ProcessCodecError::TooLarge);
    }
    let mut body = body_of(msg, S2C_PROCESS_LISTED)?;
    let nonce = take_u16(&mut body)?;
    let status = take_u8(&mut body)?;
    let revision = take_u64(&mut body)?;
    let count = take_u16(&mut body)? as usize;
    if count > PROCESS_MAX_LIST_ENTRIES {
        return Err(ProcessCodecError::TooLarge);
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let entry = ProcessListEntry {
            process_ref: take_u64(&mut body)?,
            state: take_u8(&mut body)?,
            flags: take_u8(&mut body)?,
            pid: take_u32(&mut body)?,
            argv0: {
                let len = take_u32(&mut body)? as usize;
                take_len(&mut body, len, PROCESS_MAX_ARG_LEN)?
            },
        };
        validate_list_entry(entry)?;
        entries.push(entry);
    }
    let listed = ProcessListed {
        nonce,
        status,
        revision,
        entries,
        detail: take_detail(body)?,
    };
    validate_listed(&listed)?;
    Ok(listed)
}

fn validate_watched(watched: &ProcessWatched<'_>) -> Result<(), ProcessCodecError> {
    if (watched.status == STATUS_OK && watched.process_ref == 0)
        || watched.stdin_acked > watched.stdin_received
    {
        return Err(ProcessCodecError::Invalid);
    }
    if watched.status != STATUS_OK {
        if watched.state != 0
            || watched.stream_state != 0
            || watched.stdin_received != 0
            || watched.stdin_acked != 0
            || watched.stdout_next != 0
            || watched.stderr_next != 0
            || watched.stdin_window != 0
            || watched.stdout_window != 0
            || watched.stderr_window != 0
            || watched.exit_reason != 0
            || watched.kill_cause != 0
            || watched.exit_code != 0
        {
            return Err(ProcessCodecError::Invalid);
        }
        return Ok(());
    }
    if !matches!(watched.state, PROCESS_STATE_RUNNING | PROCESS_STATE_EXITED)
        || watched.stream_state & !PROCESS_STREAM_STATE_FLAGS != 0
    {
        return Err(ProcessCodecError::Invalid);
    }
    let stdin_bits = watched.stream_state
        & (PROCESS_STREAM_STDIN_ACCEPTING
            | PROCESS_STREAM_STDIN_CLOSING
            | PROCESS_STREAM_STDIN_CLOSED);
    if stdin_bits.count_ones() != 1 {
        return Err(ProcessCodecError::Invalid);
    }
    let accepting = stdin_bits == PROCESS_STREAM_STDIN_ACCEPTING;
    let writable = watched.stream_state & PROCESS_STREAM_STDIN_WRITABLE != 0;
    let stdout_open = watched.stream_state & PROCESS_STREAM_STDOUT_OPEN != 0;
    let stderr_open = watched.stream_state & PROCESS_STREAM_STDERR_OPEN != 0;
    let merged = watched.stream_state & PROCESS_STREAM_MERGED_STDERR != 0;
    if (writable && (!accepting || watched.stdin_window == 0))
        || (!writable && watched.stdin_window != 0)
        || (writable
            && watched
                .stdin_acked
                .checked_add(watched.stdin_window)
                .is_none_or(|limit| watched.stdin_received > limit))
        || stdout_open != (watched.stdout_window != 0)
        || stderr_open != (watched.stderr_window != 0)
        || (merged && (stderr_open || watched.stderr_next != 0 || watched.stderr_window != 0))
    {
        return Err(ProcessCodecError::Invalid);
    }
    match watched.state {
        PROCESS_STATE_RUNNING
            if watched.exit_reason == 0 && watched.kill_cause == 0 && watched.exit_code == 0 =>
        {
            Ok(())
        }
        PROCESS_STATE_EXITED
            if stdin_bits == PROCESS_STREAM_STDIN_CLOSED
                && !stdout_open
                && !stderr_open
                && watched.stdin_window == 0
                && watched.stdout_window == 0
                && watched.stderr_window == 0 =>
        {
            validate_exit(watched.exit_reason, watched.kill_cause, watched.exit_code)
        }
        _ => Err(ProcessCodecError::Invalid),
    }
}

pub fn msg_process_watched(watched: ProcessWatched<'_>) -> Result<Vec<u8>, ProcessCodecError> {
    validate_watched(&watched)?;
    let detail = detail_bytes(watched.detail);
    let mut msg = Vec::with_capacity(90 + detail.len());
    msg.push(S2C_PROCESS_WATCHED);
    msg.extend_from_slice(&watched.nonce.to_le_bytes());
    msg.push(watched.status);
    msg.extend_from_slice(&watched.process_id.to_le_bytes());
    msg.extend_from_slice(&watched.process_ref.to_le_bytes());
    msg.push(watched.state);
    msg.push(watched.stream_state);
    msg.extend_from_slice(&watched.stdin_received.to_le_bytes());
    msg.extend_from_slice(&watched.stdin_acked.to_le_bytes());
    msg.extend_from_slice(&watched.stdout_next.to_le_bytes());
    msg.extend_from_slice(&watched.stderr_next.to_le_bytes());
    msg.extend_from_slice(&watched.stdin_window.to_le_bytes());
    msg.extend_from_slice(&watched.stdout_window.to_le_bytes());
    msg.extend_from_slice(&watched.stderr_window.to_le_bytes());
    msg.push(watched.exit_reason);
    msg.push(watched.kill_cause);
    msg.extend_from_slice(&watched.exit_code.to_le_bytes());
    msg.extend_from_slice(detail);
    Ok(msg)
}

pub fn parse_process_watched(msg: &[u8]) -> Result<ProcessWatched<'_>, ProcessCodecError> {
    let mut body = body_of(msg, S2C_PROCESS_WATCHED)?;
    let nonce = take_u16(&mut body)?;
    let status = take_u8(&mut body)?;
    let process_id = take_u32(&mut body)?;
    let process_ref = take_u64(&mut body)?;
    let state = take_u8(&mut body)?;
    let stream_state = take_u8(&mut body)?;
    let stdin_received = take_u64(&mut body)?;
    let stdin_acked = take_u64(&mut body)?;
    let stdout_next = take_u64(&mut body)?;
    let stderr_next = take_u64(&mut body)?;
    let stdin_window = take_u64(&mut body)?;
    let stdout_window = take_u64(&mut body)?;
    let stderr_window = take_u64(&mut body)?;
    let exit_reason = take_u8(&mut body)?;
    let kill_cause = take_u8(&mut body)?;
    let exit_code = take_u32(&mut body)?;
    let watched = ProcessWatched {
        nonce,
        status,
        process_id,
        process_ref,
        state,
        stream_state,
        stdin_received,
        stdin_acked,
        stdout_next,
        stderr_next,
        stdin_window,
        stdout_window,
        stderr_window,
        exit_reason,
        kill_cause,
        exit_code,
        detail: take_detail(body)?,
    };
    validate_watched(&watched)?;
    Ok(watched)
}

/// Owned, transport-neutral command builder used by native clients and Wasm
/// extension guests. It only produces ordinary process-family packets; the
/// caller continues to multiplex those packets over its existing Blit link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessCommand {
    argv: Vec<Vec<u8>>,
    env: Vec<(Vec<u8>, Vec<u8>)>,
    cwd_kind: u8,
    src_pty_id: u16,
    cwd: Vec<u8>,
    flags: u8,
}

impl ProcessCommand {
    /// Build a direct-exec command.
    ///
    /// The child inherits the server process environment. Use
    /// [`env`](Self::env) to add or replace entries.
    pub fn new(program: impl Into<Vec<u8>>) -> Self {
        Self {
            argv: vec![program.into()],
            env: Vec::new(),
            cwd_kind: PROCESS_CWD_DEFAULT,
            src_pty_id: 0,
            cwd: Vec::new(),
            flags: 0,
        }
    }

    pub fn arg(mut self, arg: impl Into<Vec<u8>>) -> Self {
        self.argv.push(arg.into());
        self
    }

    /// Add a child environment entry. An explicit key replaces the same key
    /// inherited from the server.
    pub fn env(mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn cwd(mut self, cwd: impl Into<Vec<u8>>) -> Self {
        self.cwd_kind = PROCESS_CWD_EXPLICIT;
        self.src_pty_id = 0;
        self.cwd = cwd.into();
        self
    }

    pub fn cwd_from_pty(mut self, pty_id: u16) -> Self {
        self.cwd_kind = PROCESS_CWD_FROM_PTY;
        self.src_pty_id = pty_id;
        self.cwd.clear();
        self
    }

    pub fn merge_stderr(mut self, merge: bool) -> Self {
        self.flags = if merge {
            self.flags | PROCESS_SPAWN_MERGE_STDERR
        } else {
            self.flags & !PROCESS_SPAWN_MERGE_STDERR
        };
        self
    }

    pub fn detachable(mut self, detachable: bool) -> Self {
        self.flags = if detachable {
            self.flags | PROCESS_SPAWN_DETACHABLE
        } else {
            self.flags & !PROCESS_SPAWN_DETACHABLE
        };
        self
    }

    pub fn spawn_packet(&self, nonce: u16, process_id: u32) -> Result<Vec<u8>, ProcessCodecError> {
        msg_process_spawn(&ProcessSpawnRequest {
            nonce,
            process_id,
            flags: self.flags,
            cwd_kind: self.cwd_kind,
            src_pty_id: self.src_pty_id,
            cwd: &self.cwd,
            argv: self.argv.iter().map(Vec::as_slice).collect(),
            env: self
                .env
                .iter()
                .map(|(key, value)| (key.as_slice(), value.as_slice()))
                .collect(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessExitStatus {
    pub reason: u8,
    pub kill_cause: u8,
    pub code: u32,
    pub detail: String,
}

impl ProcessExitStatus {
    pub fn success(&self) -> bool {
        self.reason == PROCESS_EXIT_RETURNED && self.code == 0
    }
}

#[derive(Clone, Debug)]
pub enum ProcessWatchResult {
    Running(ProcessChild),
    Exited(ProcessExitStatus),
}

impl ProcessWatchResult {
    pub fn from_reply(watched: ProcessWatched<'_>) -> Result<Self, ProcessClientError> {
        validate_watched(&watched)?;
        if watched.status != STATUS_OK {
            return Err(ProcessClientError::Refused {
                status: watched.status,
                detail: watched.detail.to_owned(),
            });
        }
        if watched.state == PROCESS_STATE_EXITED {
            return Ok(Self::Exited(ProcessExitStatus {
                reason: watched.exit_reason,
                kill_cause: watched.kill_cause,
                code: watched.exit_code,
                detail: watched.detail.to_owned(),
            }));
        }
        Ok(Self::Running(ProcessChild::from_watched(watched)?))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessEvent {
    Stdout {
        offset: u64,
        data: Vec<u8>,
    },
    Stderr {
        offset: u64,
        data: Vec<u8>,
    },
    StdinAck {
        bytes: u64,
        state: u8,
    },
    Controlled {
        nonce: u16,
        status: u8,
        detail: String,
    },
    Exit(ProcessExitStatus),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessClientError {
    Codec(ProcessCodecError),
    Refused { status: u8, detail: String },
    WrongProcess,
    UnexpectedPacket,
    Offset,
    Window,
    StdinClosed,
    StdinNotWritable,
    Exited,
}

impl From<ProcessCodecError> for ProcessClientError {
    fn from(value: ProcessCodecError) -> Self {
        Self::Codec(value)
    }
}

/// Per-generation client-side accounting. The embedding client calls
/// [`decode_event`](Self::decode_event) from its normal receive multiplexer and
/// emits an ACK from [`acknowledge`](Self::acknowledge) only after consuming an
/// output event.
#[derive(Clone, Debug)]
pub struct ProcessChild {
    process_id: u32,
    process_ref: ProcessRef,
    stdin_next: u64,
    stdin_acked: u64,
    stdin_window: u64,
    stdin_state: u8,
    stdin_writable: bool,
    stdout_next: u64,
    stderr_next: u64,
    stdout_acked: u64,
    stderr_acked: u64,
    stdout_window: u64,
    stderr_window: u64,
    exited: bool,
}

impl ProcessChild {
    pub fn from_started(started: ProcessStarted<'_>) -> Result<Self, ProcessClientError> {
        validate_started(&started)?;
        if started.status != STATUS_OK {
            return Err(ProcessClientError::Refused {
                status: started.status,
                detail: started.detail.to_owned(),
            });
        }
        Ok(Self {
            process_id: started.process_id,
            process_ref: started.process_ref,
            stdin_next: 0,
            stdin_acked: 0,
            stdin_window: started.stdin_window,
            stdin_state: if started.stdin_window == 0 {
                PROCESS_STDIN_CLOSED
            } else {
                PROCESS_STDIN_ACCEPTING
            },
            stdin_writable: started.stdin_window != 0,
            stdout_next: 0,
            stderr_next: 0,
            stdout_acked: 0,
            stderr_acked: 0,
            stdout_window: started.stdout_window,
            stderr_window: started.stderr_window,
            exited: false,
        })
    }

    pub fn from_watched(watched: ProcessWatched<'_>) -> Result<Self, ProcessClientError> {
        validate_watched(&watched)?;
        if watched.status != STATUS_OK {
            return Err(ProcessClientError::Refused {
                status: watched.status,
                detail: watched.detail.to_owned(),
            });
        }
        if watched.state == PROCESS_STATE_EXITED {
            return Err(ProcessClientError::Exited);
        }
        Ok(Self {
            process_id: watched.process_id,
            process_ref: watched.process_ref,
            stdin_next: watched.stdin_received,
            stdin_acked: watched.stdin_acked,
            stdin_window: watched.stdin_window,
            stdin_state: if watched.stream_state & PROCESS_STREAM_STDIN_ACCEPTING != 0 {
                PROCESS_STDIN_ACCEPTING
            } else if watched.stream_state & PROCESS_STREAM_STDIN_CLOSING != 0 {
                PROCESS_STDIN_CLOSING
            } else {
                PROCESS_STDIN_CLOSED
            },
            stdin_writable: watched.stream_state & PROCESS_STREAM_STDIN_WRITABLE != 0,
            stdout_next: watched.stdout_next,
            stderr_next: watched.stderr_next,
            stdout_acked: watched.stdout_next,
            stderr_acked: watched.stderr_next,
            stdout_window: watched.stdout_window,
            stderr_window: watched.stderr_window,
            exited: false,
        })
    }

    pub fn process_id(&self) -> u32 {
        self.process_id
    }

    pub fn process_ref(&self) -> ProcessRef {
        self.process_ref
    }

    pub fn stdin_writable(&self) -> bool {
        self.stdin_writable
    }

    pub fn stdin_packet(&mut self, data: &[u8]) -> Result<Vec<u8>, ProcessClientError> {
        if self.exited {
            return Err(ProcessClientError::Exited);
        }
        if !self.stdin_writable {
            return Err(ProcessClientError::StdinNotWritable);
        }
        if self.stdin_state != PROCESS_STDIN_ACCEPTING {
            return Err(ProcessClientError::StdinClosed);
        }
        let end = self
            .stdin_next
            .checked_add(data.len() as u64)
            .ok_or(ProcessClientError::Window)?;
        let limit = self
            .stdin_acked
            .checked_add(self.stdin_window)
            .ok_or(ProcessClientError::Window)?;
        if end > limit {
            return Err(ProcessClientError::Window);
        }
        let packet = msg_process_stdin(ProcessStdin {
            process_id: self.process_id,
            offset: self.stdin_next,
            data,
        })?;
        self.stdin_next = end;
        Ok(packet)
    }

    pub fn close_stdin_packet(&self, nonce: u16) -> Result<Vec<u8>, ProcessCodecError> {
        msg_process_control(ProcessControl {
            nonce,
            process_id: self.process_id,
            action: PROCESS_CONTROL_CLOSE_STDIN,
            value: 0,
        })
    }

    pub fn terminate_packet(&self, nonce: u16) -> Result<Vec<u8>, ProcessCodecError> {
        msg_process_control(ProcessControl {
            nonce,
            process_id: self.process_id,
            action: PROCESS_CONTROL_TERMINATE,
            value: 0,
        })
    }

    pub fn kill_packet(&self, nonce: u16) -> Result<Vec<u8>, ProcessCodecError> {
        msg_process_control(ProcessControl {
            nonce,
            process_id: self.process_id,
            action: PROCESS_CONTROL_KILL,
            value: 0,
        })
    }

    pub fn signal_packet(&self, nonce: u16, signal: u32) -> Result<Vec<u8>, ProcessCodecError> {
        msg_process_control(ProcessControl {
            nonce,
            process_id: self.process_id,
            action: PROCESS_CONTROL_SIGNAL,
            value: signal,
        })
    }

    pub fn unwatch_packet(&self, nonce: u16) -> Result<Vec<u8>, ProcessCodecError> {
        msg_process_control(ProcessControl {
            nonce,
            process_id: self.process_id,
            action: PROCESS_CONTROL_UNWATCH,
            value: 0,
        })
    }

    pub fn decode_event(&mut self, packet: &[u8]) -> Result<ProcessEvent, ProcessClientError> {
        if self.exited {
            return Err(ProcessClientError::Exited);
        }
        match packet.first().copied() {
            Some(S2C_PROCESS_STDOUT) => {
                let output = parse_process_stdout(packet)?;
                self.check_output(output.process_id, output.offset, output.data.len(), true)?;
                Ok(ProcessEvent::Stdout {
                    offset: output.offset,
                    data: output.data.to_vec(),
                })
            }
            Some(S2C_PROCESS_STDERR) => {
                let output = parse_process_stderr(packet)?;
                self.check_output(output.process_id, output.offset, output.data.len(), false)?;
                Ok(ProcessEvent::Stderr {
                    offset: output.offset,
                    data: output.data.to_vec(),
                })
            }
            Some(S2C_PROCESS_STDIN_ACK) => {
                let ack = parse_process_stdin_ack(packet)?;
                if ack.process_id != self.process_id {
                    return Err(ProcessClientError::WrongProcess);
                }
                if ack.bytes < self.stdin_acked
                    || (self.stdin_writable && ack.bytes > self.stdin_next)
                {
                    return Err(ProcessClientError::Offset);
                }
                if !self.stdin_writable {
                    self.stdin_next = self.stdin_next.max(ack.bytes);
                }
                self.stdin_acked = ack.bytes;
                self.stdin_state = ack.stdin_state;
                Ok(ProcessEvent::StdinAck {
                    bytes: ack.bytes,
                    state: ack.stdin_state,
                })
            }
            Some(S2C_PROCESS_CONTROLLED) => {
                let controlled = parse_process_controlled(packet)?;
                if controlled.process_id != self.process_id {
                    return Err(ProcessClientError::WrongProcess);
                }
                Ok(ProcessEvent::Controlled {
                    nonce: controlled.nonce,
                    status: controlled.status,
                    detail: controlled.detail.to_owned(),
                })
            }
            Some(S2C_PROCESS_EXIT) => {
                let exit = parse_process_exit(packet)?;
                if exit.process_id != self.process_id {
                    return Err(ProcessClientError::WrongProcess);
                }
                self.exited = true;
                Ok(ProcessEvent::Exit(ProcessExitStatus {
                    reason: exit.reason,
                    kill_cause: exit.kill_cause,
                    code: exit.code,
                    detail: exit.detail.to_owned(),
                }))
            }
            _ => Err(ProcessClientError::UnexpectedPacket),
        }
    }

    fn check_output(
        &mut self,
        process_id: u32,
        offset: u64,
        len: usize,
        stdout: bool,
    ) -> Result<(), ProcessClientError> {
        if process_id != self.process_id {
            return Err(ProcessClientError::WrongProcess);
        }
        let (next, acked) = if stdout {
            (&mut self.stdout_next, self.stdout_acked)
        } else {
            (&mut self.stderr_next, self.stderr_acked)
        };
        let window = if stdout {
            self.stdout_window
        } else {
            self.stderr_window
        };
        let end = offset
            .checked_add(len as u64)
            .ok_or(ProcessClientError::Offset)?;
        let limit = acked
            .checked_add(window)
            .ok_or(ProcessClientError::Window)?;
        if window == 0 || offset != *next {
            return Err(ProcessClientError::Offset);
        }
        if end > limit {
            return Err(ProcessClientError::Window);
        }
        *next = end;
        Ok(())
    }

    pub fn acknowledge(&mut self, event: &ProcessEvent) -> Result<Vec<u8>, ProcessClientError> {
        let (stream, offset, len, acked) = match event {
            ProcessEvent::Stdout { offset, data } => (
                PROCESS_STREAM_STDOUT,
                *offset,
                data.len(),
                &mut self.stdout_acked,
            ),
            ProcessEvent::Stderr { offset, data } => (
                PROCESS_STREAM_STDERR,
                *offset,
                data.len(),
                &mut self.stderr_acked,
            ),
            _ => return Err(ProcessClientError::UnexpectedPacket),
        };
        if offset != *acked {
            return Err(ProcessClientError::Offset);
        }
        let bytes = offset
            .checked_add(len as u64)
            .ok_or(ProcessClientError::Offset)?;
        *acked = bytes;
        Ok(msg_process_output_ack(ProcessOutputAck {
            process_id: self.process_id,
            stream,
            bytes,
        })?)
    }
}

pub fn process_list_packet(nonce: u16) -> Vec<u8> {
    msg_process_list(ProcessList { nonce })
}

/// Build a read-only process watch request.
pub fn process_watch_packet(
    nonce: u16,
    process_id: u32,
    process_ref: ProcessRef,
) -> Result<Vec<u8>, ProcessCodecError> {
    msg_process_watch(ProcessWatch {
        nonce,
        process_id,
        process_ref,
        flags: 0,
    })
}

/// Build a process watch request which also requests the stdin-writer role.
pub fn process_watch_stdin_packet(
    nonce: u16,
    process_id: u32,
    process_ref: ProcessRef,
) -> Result<Vec<u8>, ProcessCodecError> {
    msg_process_watch(ProcessWatch {
        nonce,
        process_id,
        process_ref,
        flags: PROCESS_WATCH_STDIN,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{STATUS_NOT_FOUND, STATUS_PERMISSION};

    fn spawn_request<'a>() -> ProcessSpawnRequest<'a> {
        ProcessSpawnRequest {
            nonce: 0x0201,
            process_id: 0x0605_0403,
            flags: 0,
            cwd_kind: PROCESS_CWD_EXPLICIT,
            src_pty_id: 0,
            cwd: b"/tmp",
            argv: vec![b"printf", b"a\0b"],
            env: vec![(b"LANG", b"C")],
        }
    }

    #[test]
    fn allocation_is_locked() {
        assert_eq!(FEATURE_PROCESS, 1 << 13);
        assert_eq!(
            [
                C2S_PROCESS_SPAWN,
                C2S_PROCESS_STDIN,
                C2S_PROCESS_OUTPUT_ACK,
                C2S_PROCESS_CONTROL,
                C2S_PROCESS_LIST,
                C2S_PROCESS_WATCH,
            ],
            [0xC0, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5]
        );
        assert_eq!(
            [
                S2C_PROCESS_STARTED,
                S2C_PROCESS_STDOUT,
                S2C_PROCESS_STDERR,
                S2C_PROCESS_STDIN_ACK,
                S2C_PROCESS_EXIT,
                S2C_PROCESS_CONTROLLED,
                S2C_PROCESS_LISTED,
                S2C_PROCESS_WATCHED,
            ],
            [0xC0, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7]
        );
        assert_eq!(PROCESS_SPAWN_DETACHABLE, 1 << 1);
        assert_eq!(PROCESS_WATCH_STDIN, 1 << 0);
    }

    #[test]
    fn command_builder_and_child_accounting_are_transport_neutral() {
        let command = ProcessCommand::new(b"rg".to_vec())
            .arg(b"needle".to_vec())
            .cwd(b"/workspace".to_vec())
            .env(b"LANG".to_vec(), b"C".to_vec())
            .merge_stderr(true)
            .detachable(true);
        let packet = command.spawn_packet(3, 7).unwrap();
        let spawn = parse_process_spawn(&packet).unwrap();
        assert_eq!(spawn.argv, [b"rg".as_slice(), b"needle".as_slice()]);
        assert_eq!(spawn.cwd, b"/workspace");
        assert_ne!(spawn.flags & PROCESS_SPAWN_MERGE_STDERR, 0);
        assert_ne!(spawn.flags & PROCESS_SPAWN_DETACHABLE, 0);

        let mut child = ProcessChild::from_started(ProcessStarted {
            nonce: 3,
            status: STATUS_OK,
            process_id: 7,
            process_ref: 11,
            stdin_window: 16,
            stdout_window: 16,
            stderr_window: 0,
            detail: "",
        })
        .unwrap();
        assert_eq!(child.process_ref(), 11);
        assert!(child.stdin_writable());
        let stdin = child.stdin_packet(b"hello").unwrap();
        assert_eq!(parse_process_stdin(&stdin).unwrap().offset, 0);
        let beyond_sent = msg_process_stdin_ack(ProcessStdinAck {
            process_id: 7,
            bytes: 6,
            stdin_state: PROCESS_STDIN_ACCEPTING,
        })
        .unwrap();
        assert_eq!(
            child.decode_event(&beyond_sent),
            Err(ProcessClientError::Offset)
        );
        let closed = msg_process_stdin_ack(ProcessStdinAck {
            process_id: 7,
            bytes: 5,
            stdin_state: PROCESS_STDIN_CLOSED,
        })
        .unwrap();
        assert!(matches!(
            child.decode_event(&closed),
            Ok(ProcessEvent::StdinAck {
                bytes: 5,
                state: PROCESS_STDIN_CLOSED
            })
        ));
        assert_eq!(
            child.stdin_packet(b"after eof"),
            Err(ProcessClientError::StdinClosed)
        );
        let output = msg_process_stdout(ProcessOutput {
            process_id: 7,
            offset: 0,
            data: b"binary\0data",
        })
        .unwrap();
        let event = child.decode_event(&output).unwrap();
        assert_eq!(
            event,
            ProcessEvent::Stdout {
                offset: 0,
                data: b"binary\0data".to_vec()
            }
        );
        let past_window = msg_process_stdout(ProcessOutput {
            process_id: 7,
            offset: 11,
            data: b"123456",
        })
        .unwrap();
        assert_eq!(
            child.decode_event(&past_window),
            Err(ProcessClientError::Window)
        );
        let ack = child.acknowledge(&event).unwrap();
        assert_eq!(parse_process_output_ack(&ack).unwrap().bytes, 11);
    }

    #[test]
    fn spawn_roundtrip_preserves_binary_fields() {
        let mut req = spawn_request();
        req.argv[1] = b"a\xffb";
        req.env[0].1 = b"C\xff";
        let msg = msg_process_spawn(&req).unwrap();
        let got = parse_process_spawn(&msg).unwrap();
        assert_eq!(got, req);
        assert_eq!(&msg[..8], &[0xC0, 1, 2, 3, 4, 5, 6, 0]);
    }

    #[test]
    fn spawn_rejects_nul_duplicate_and_bad_field_combinations() {
        assert_eq!(
            msg_process_spawn(&spawn_request()),
            Err(ProcessCodecError::Invalid)
        );
        let mut req = spawn_request();
        req.argv[1] = b"ab";
        req.env.push((b"LANG", b"other"));
        assert_eq!(msg_process_spawn(&req), Err(ProcessCodecError::Invalid));
        req.env.pop();
        req.flags = 1 << 2;
        assert_eq!(msg_process_spawn(&req), Err(ProcessCodecError::Invalid));
        req.flags = PROCESS_SPAWN_DETACHABLE;
        req.cwd_kind = PROCESS_CWD_FROM_PTY;
        req.cwd = b"";
        req.src_pty_id = 0;
        assert!(msg_process_spawn(&req).is_ok(), "pty id zero is valid");
        req.cwd = b"still set";
        assert_eq!(msg_process_spawn(&req), Err(ProcessCodecError::Invalid));
    }

    #[test]
    fn windows_host_validation_is_explicit_not_the_client_compile_target() {
        let mut req = spawn_request();
        req.argv[1] = b"a\xffb";
        req.env[0].1 = b"C\xff";
        assert!(
            msg_process_spawn(&req).is_ok(),
            "Unix wire form stays valid"
        );
        assert_eq!(
            validate_process_spawn_for_windows(&req, str::eq_ignore_ascii_case),
            Err(ProcessCodecError::Invalid)
        );

        req.argv[1] = b"ab";
        req.env[0].1 = b"C";
        req.env.push((b"lang", b"other"));
        assert!(
            msg_process_spawn(&req).is_ok(),
            "distinct Unix keys stay valid"
        );
        assert_eq!(
            validate_process_spawn_for_windows(&req, str::eq_ignore_ascii_case),
            Err(ProcessCodecError::Invalid)
        );
    }

    #[test]
    fn spawn_rejects_caps_before_allocating_claimed_payloads() {
        let mut msg = vec![C2S_PROCESS_SPAWN];
        msg.extend_from_slice(&1u16.to_le_bytes());
        msg.extend_from_slice(&2u32.to_le_bytes());
        msg.push(0);
        msg.push(PROCESS_CWD_DEFAULT);
        msg.extend_from_slice(&0u16.to_le_bytes());
        msg.extend_from_slice(&0u32.to_le_bytes());
        msg.extend_from_slice(&((PROCESS_MAX_ARGC + 1) as u16).to_le_bytes());
        assert_eq!(parse_process_spawn(&msg), Err(ProcessCodecError::TooLarge));

        let mut req = spawn_request();
        req.argv[1] = b"ab";
        req.argv = vec![b"x"; PROCESS_MAX_ARGC + 1];
        assert_eq!(msg_process_spawn(&req), Err(ProcessCodecError::TooLarge));
    }

    #[test]
    fn fixed_c2s_messages_roundtrip_and_reject_trailing_bytes() {
        let stdin = ProcessStdin {
            process_id: 7,
            offset: 9,
            data: b"\0binary",
        };
        assert_eq!(
            parse_process_stdin(&msg_process_stdin(stdin).unwrap()),
            Ok(stdin)
        );

        let ack = ProcessOutputAck {
            process_id: 7,
            stream: PROCESS_STREAM_STDERR,
            bytes: 99,
        };
        let mut ack_msg = msg_process_output_ack(ack).unwrap();
        assert_eq!(parse_process_output_ack(&ack_msg), Ok(ack));
        ack_msg.push(0);
        assert_eq!(
            parse_process_output_ack(&ack_msg),
            Err(ProcessCodecError::Malformed)
        );

        let control = ProcessControl {
            nonce: 4,
            process_id: 7,
            action: PROCESS_CONTROL_SIGNAL,
            value: 15,
        };
        assert_eq!(
            parse_process_control(&msg_process_control(control).unwrap()),
            Ok(control)
        );
        let list = ProcessList { nonce: 8 };
        assert_eq!(parse_process_list(&msg_process_list(list)), Ok(list));
        let watch = ProcessWatch {
            nonce: 8,
            process_id: 9,
            process_ref: 10,
            flags: PROCESS_WATCH_STDIN,
        };
        assert_eq!(
            parse_process_watch(&msg_process_watch(watch).unwrap()),
            Ok(watch)
        );
        assert_eq!(
            msg_process_watch(ProcessWatch {
                process_ref: 0,
                ..watch
            }),
            Err(ProcessCodecError::Invalid)
        );
        assert_eq!(
            msg_process_watch(ProcessWatch { flags: 2, ..watch }),
            Err(ProcessCodecError::Invalid)
        );
        assert_eq!(
            parse_process_watch(&process_watch_packet(8, 9, 10).unwrap())
                .unwrap()
                .flags,
            0
        );
        assert_eq!(
            parse_process_watch(&process_watch_stdin_packet(8, 9, 10).unwrap())
                .unwrap()
                .flags,
            PROCESS_WATCH_STDIN
        );
    }

    #[test]
    fn responses_roundtrip_and_enforce_failure_shapes() {
        let started = ProcessStarted {
            nonce: 1,
            status: STATUS_OK,
            process_id: 2,
            process_ref: 7,
            stdin_window: 3,
            stdout_window: 4,
            stderr_window: 0,
            detail: "",
        };
        assert_eq!(
            parse_process_started(&msg_process_started(started).unwrap()),
            Ok(started)
        );
        assert_eq!(
            msg_process_started(ProcessStarted {
                status: STATUS_PERMISSION,
                ..started
            }),
            Err(ProcessCodecError::Invalid)
        );
        let refused = ProcessStarted {
            status: STATUS_PERMISSION,
            process_ref: 0,
            stdin_window: 0,
            stdout_window: 0,
            detail: "disabled",
            ..started
        };
        assert_eq!(
            parse_process_started(&msg_process_started(refused).unwrap()),
            Ok(refused)
        );

        let output = ProcessOutput {
            process_id: 2,
            offset: 4,
            data: b"bytes",
        };
        assert_eq!(
            parse_process_stdout(&msg_process_stdout(output).unwrap()),
            Ok(output)
        );
        assert_eq!(
            parse_process_stderr(&msg_process_stderr(output).unwrap()),
            Ok(output)
        );

        let ack = ProcessStdinAck {
            process_id: 2,
            bytes: 5,
            stdin_state: PROCESS_STDIN_CLOSING,
        };
        assert_eq!(
            parse_process_stdin_ack(&msg_process_stdin_ack(ack).unwrap()),
            Ok(ack)
        );

        let controlled = ProcessControlled {
            nonce: 9,
            status: STATUS_NOT_FOUND,
            process_id: 2,
            detail: "gone",
        };
        assert_eq!(
            parse_process_controlled(&msg_process_controlled(controlled)),
            Ok(controlled)
        );
    }

    #[test]
    fn exit_validation_preserves_unknown_reasons() {
        let returned = ProcessExit {
            process_id: 5,
            reason: PROCESS_EXIT_RETURNED,
            kill_cause: 0,
            code: 42,
            detail: "",
        };
        assert_eq!(
            parse_process_exit(&msg_process_exit(returned).unwrap()),
            Ok(returned)
        );
        assert_eq!(
            msg_process_exit(ProcessExit {
                kill_cause: PROCESS_KILL_CLIENT,
                ..returned
            }),
            Err(ProcessCodecError::Invalid)
        );
        let future = ProcessExit {
            reason: 200,
            kill_cause: 199,
            code: 123,
            ..returned
        };
        assert_eq!(
            parse_process_exit(&msg_process_exit(future).unwrap()),
            Ok(future)
        );
    }

    #[test]
    fn listed_shapes_roundtrip_and_enforce_caps() {
        let listed = ProcessListed {
            nonce: 1,
            status: STATUS_OK,
            revision: 9,
            entries: vec![
                ProcessListEntry {
                    process_ref: 7,
                    state: PROCESS_STATE_RUNNING,
                    flags: PROCESS_SPAWN_DETACHABLE,
                    pid: 123,
                    argv0: b"rg\xff",
                },
                ProcessListEntry {
                    process_ref: 8,
                    state: PROCESS_STATE_EXITED,
                    flags: PROCESS_SPAWN_MERGE_STDERR | PROCESS_SPAWN_DETACHABLE,
                    pid: 124,
                    argv0: b"true",
                },
            ],
            detail: "",
        };
        assert_eq!(
            parse_process_listed(&msg_process_listed(listed.clone()).unwrap()),
            Ok(listed.clone())
        );
        let mut unsorted = listed.clone();
        unsorted.entries.swap(0, 1);
        assert_eq!(
            msg_process_listed(unsorted),
            Err(ProcessCodecError::Invalid)
        );
        let mut duplicate = listed.clone();
        duplicate.entries[1].process_ref = duplicate.entries[0].process_ref;
        assert_eq!(
            msg_process_listed(duplicate),
            Err(ProcessCodecError::Invalid)
        );
        let mut ordinary_final = listed.clone();
        ordinary_final.entries[1].flags &= !PROCESS_SPAWN_DETACHABLE;
        assert_eq!(
            msg_process_listed(ordinary_final),
            Err(ProcessCodecError::Invalid)
        );

        let refused = ProcessListed {
            nonce: 2,
            status: STATUS_PERMISSION,
            revision: 0,
            entries: Vec::new(),
            detail: "disabled",
        };
        assert_eq!(
            parse_process_listed(&msg_process_listed(refused.clone()).unwrap()),
            Ok(refused)
        );
        assert_eq!(
            msg_process_listed(ProcessListed {
                status: STATUS_PERMISSION,
                detail: "failed",
                ..listed.clone()
            }),
            Err(ProcessCodecError::Invalid)
        );

        let argv0 = vec![b'x'; PROCESS_MAX_ARG_LEN];
        let entry = ProcessListEntry {
            process_ref: 1,
            state: PROCESS_STATE_RUNNING,
            flags: 0,
            pid: 1,
            argv0: &argv0,
        };
        assert_eq!(
            msg_process_listed(ProcessListed {
                nonce: 3,
                status: STATUS_OK,
                revision: 1,
                entries: (1..=129)
                    .map(|process_ref| ProcessListEntry {
                        process_ref,
                        ..entry
                    })
                    .collect(),
                detail: "",
            }),
            Err(ProcessCodecError::TooLarge)
        );
        assert_eq!(
            msg_process_listed(ProcessListed {
                nonce: 3,
                status: STATUS_OK,
                revision: 1,
                entries: (1..=PROCESS_MAX_LIST_ENTRIES + 1)
                    .map(|process_ref| ProcessListEntry {
                        process_ref: process_ref as u64,
                        ..entry
                    })
                    .collect(),
                detail: "",
            }),
            Err(ProcessCodecError::TooLarge)
        );
    }

    #[test]
    fn watched_running_and_exited_shapes_roundtrip() {
        let running = ProcessWatched {
            nonce: 1,
            status: STATUS_OK,
            process_id: 2,
            process_ref: 7,
            state: PROCESS_STATE_RUNNING,
            stream_state: PROCESS_STREAM_STDIN_ACCEPTING
                | PROCESS_STREAM_STDOUT_OPEN
                | PROCESS_STREAM_MERGED_STDERR
                | PROCESS_STREAM_STDIN_WRITABLE,
            stdin_received: 10,
            stdin_acked: 8,
            stdout_next: 20,
            stderr_next: 0,
            stdin_window: 1024,
            stdout_window: 1024,
            stderr_window: 0,
            exit_reason: 0,
            kill_cause: 0,
            exit_code: 0,
            detail: "",
        };
        assert_eq!(
            parse_process_watched(&msg_process_watched(running).unwrap()),
            Ok(running)
        );
        assert_eq!(
            msg_process_watched(ProcessWatched {
                stdin_received: running.stdin_acked + running.stdin_window + 1,
                ..running
            }),
            Err(ProcessCodecError::Invalid)
        );
        assert_eq!(
            msg_process_watched(ProcessWatched {
                stdin_received: u64::MAX,
                stdin_acked: u64::MAX,
                stdin_window: 1,
                ..running
            }),
            Err(ProcessCodecError::Invalid)
        );

        let read_only = ProcessWatched {
            stream_state: running.stream_state & !PROCESS_STREAM_STDIN_WRITABLE,
            stdin_window: 0,
            ..running
        };
        assert_eq!(
            parse_process_watched(&msg_process_watched(read_only).unwrap()),
            Ok(read_only)
        );
        let mut peer = ProcessChild::from_watched(read_only).unwrap();
        assert!(!peer.stdin_writable());
        assert_eq!(
            peer.stdin_packet(b"not mine"),
            Err(ProcessClientError::StdinNotWritable)
        );
        let peer_ack = msg_process_stdin_ack(ProcessStdinAck {
            process_id: read_only.process_id,
            bytes: 12,
            stdin_state: PROCESS_STDIN_ACCEPTING,
        })
        .unwrap();
        assert_eq!(
            peer.decode_event(&peer_ack),
            Ok(ProcessEvent::StdinAck {
                bytes: 12,
                state: PROCESS_STDIN_ACCEPTING,
            })
        );
        assert_eq!(peer.stdin_acked, 12);
        assert_eq!(peer.stdin_next, 12);
        assert_eq!(
            peer.decode_event(
                &msg_process_stdin_ack(ProcessStdinAck {
                    process_id: read_only.process_id,
                    bytes: 11,
                    stdin_state: PROCESS_STDIN_ACCEPTING,
                })
                .unwrap()
            ),
            Err(ProcessClientError::Offset)
        );

        let exited = ProcessWatched {
            state: PROCESS_STATE_EXITED,
            stream_state: PROCESS_STREAM_STDIN_CLOSED | PROCESS_STREAM_MERGED_STDERR,
            stdin_window: 0,
            stdout_window: 0,
            exit_reason: PROCESS_EXIT_RETURNED,
            exit_code: 3,
            ..running
        };
        assert_eq!(
            parse_process_watched(&msg_process_watched(exited).unwrap()),
            Ok(exited)
        );

        assert_eq!(
            msg_process_watched(ProcessWatched {
                stderr_next: 1,
                ..running
            }),
            Err(ProcessCodecError::Invalid)
        );

        let refused = ProcessWatched {
            status: STATUS_NOT_FOUND,
            state: 0,
            stream_state: 0,
            stdin_received: 0,
            stdin_acked: 0,
            stdout_next: 0,
            stderr_next: 0,
            stdin_window: 0,
            stdout_window: 0,
            stderr_window: 0,
            exit_reason: 0,
            kill_cause: 0,
            exit_code: 0,
            detail: "gone",
            ..running
        };
        assert_eq!(
            parse_process_watched(&msg_process_watched(refused).unwrap()),
            Ok(refused)
        );
        let malformed_request = ProcessWatched {
            process_ref: 0,
            ..refused
        };
        assert_eq!(
            parse_process_watched(&msg_process_watched(malformed_request).unwrap()),
            Ok(malformed_request)
        );
        assert_eq!(
            msg_process_watched(ProcessWatched {
                process_ref: 0,
                ..running
            }),
            Err(ProcessCodecError::Invalid)
        );
    }

    #[test]
    fn detail_is_utf8_clipped_and_invalid_wire_utf8_is_rejected() {
        let detail = "é".repeat(PROCESS_MAX_DETAIL_LEN);
        let controlled = ProcessControlled {
            nonce: 1,
            status: STATUS_INVALID,
            process_id: 2,
            detail: &detail,
        };
        let msg = msg_process_controlled(controlled);
        let parsed = parse_process_controlled(&msg).unwrap();
        assert!(parsed.detail.len() <= PROCESS_MAX_DETAIL_LEN);
        assert!(parsed.detail.is_char_boundary(parsed.detail.len()));

        let mut invalid = msg_process_controlled(ProcessControlled {
            detail: "x",
            ..controlled
        });
        *invalid.last_mut().unwrap() = 0xff;
        assert_eq!(
            parse_process_controlled(&invalid),
            Err(ProcessCodecError::Invalid)
        );
    }
}
