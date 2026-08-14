//! Wasmi extension wire foundations (`docs/design/extensions.md`).
//!
//! This module owns bounded decoding for every client request and the private
//! `EXT_INFO(INIT)` bootstrap packet. Lifecycle response codecs can build on
//! the same constants without duplicating request validation in the server.

use std::fmt;

pub const FEATURE_EXTENSION: u32 = 1 << 11;

pub const EXT_RUN: u8 = 0x90;
pub const EXT_PUT: u8 = 0x91;
pub const EXT_CONTROL: u8 = 0x92;
pub const EXT_EVENT: u8 = 0x93;
pub const EXT_COMMAND: u8 = 0x94;

pub const EXT_STATUS: u8 = 0x90;
pub const EXT_PUT_STATUS: u8 = 0x91;
pub const EXT_INFO: u8 = 0x92;
pub const EXT_OUTPUT_EVENT: u8 = 0x93;
pub const EXT_EXIT: u8 = 0x94;

pub const EXT_RUN_DETACH: u8 = 1 << 0;
pub const EXT_RUN_PERSIST: u8 = 1 << 1;
pub const EXT_RUN_UPDATE: u8 = 1 << 2;
pub const EXT_RUN_FLAGS: u8 = EXT_RUN_DETACH | EXT_RUN_PERSIST | EXT_RUN_UPDATE;

pub const EXT_FLAG_DETACH: u8 = 1 << 0;
pub const EXT_FLAG_PERSIST: u8 = 1 << 1;
pub const EXT_FLAG_ENABLED: u8 = 1 << 2;
pub const EXT_FLAG_DESIRED_RUNNING: u8 = 1 << 3;
pub const EXT_FLAGS: u8 =
    EXT_FLAG_DETACH | EXT_FLAG_PERSIST | EXT_FLAG_ENABLED | EXT_FLAG_DESIRED_RUNNING;

pub const EXT_RESTART_NEVER: u8 = 0;
pub const EXT_RESTART_ON_FAILURE: u8 = 1;
pub const EXT_RESTART_ALWAYS: u8 = 2;

pub const EXT_PUT_BEGIN: u8 = 1 << 0;
pub const EXT_PUT_FINAL: u8 = 1 << 1;
pub const EXT_PUT_FLAGS: u8 = EXT_PUT_BEGIN | EXT_PUT_FINAL;

pub const EXT_CONTROL_CANCEL: u8 = 1;
pub const EXT_CONTROL_ATTACH: u8 = 2;
pub const EXT_CONTROL_UNFOLLOW: u8 = 3;
pub const EXT_CONTROL_STATUS: u8 = 4;
pub const EXT_CONTROL_RESTART: u8 = 5;
pub const EXT_CONTROL_ENABLE: u8 = 6;
pub const EXT_CONTROL_DISABLE: u8 = 7;
pub const EXT_CONTROL_REMOVE: u8 = 8;
pub const EXT_CONTROL_LIST: u8 = 9;

pub const EXT_EVENT_STDOUT: u8 = 1;
pub const EXT_EVENT_STDERR: u8 = 2;
pub const EXT_EVENT_LOG: u8 = 3;

pub const EXT_COMMAND_REGISTER: u8 = 1;
pub const EXT_COMMAND_DISCOVER: u8 = 2;

pub const EXT_INFO_INIT: u8 = 1;
pub const EXT_INFO_LIST: u8 = 2;
pub const EXT_INFO_STATUS: u8 = 3;
pub const EXT_INFO_COMMAND_REGISTERED: u8 = 4;
pub const EXT_INFO_COMMANDS: u8 = 5;
pub const EXT_INFO_REPLAY_DONE: u8 = 6;

pub const EXT_PHASE_NONE: u8 = 0;
pub const EXT_PHASE_NEED_OBJECT: u8 = 1;
pub const EXT_PHASE_VALIDATING: u8 = 2;
pub const EXT_PHASE_QUEUED: u8 = 3;
pub const EXT_PHASE_RUNNING: u8 = 4;
pub const EXT_PHASE_BACKOFF: u8 = 5;
pub const EXT_PHASE_STOPPED: u8 = 6;
pub const EXT_PHASE_BLOCKED: u8 = 7;
pub const EXT_PHASE_STOPPING: u8 = 8;

pub const EXT_EXIT_RETURNED: u8 = 0;
pub const EXT_EXIT_TRAPPED: u8 = 1;
pub const EXT_EXIT_CANCELLED: u8 = 2;
pub const EXT_EXIT_UPDATED: u8 = 3;
pub const EXT_EXIT_SLOW_CONSUMER: u8 = 4;
pub const EXT_EXIT_PROTOCOL_VIOLATION: u8 = 5;
pub const EXT_EXIT_HOST_FAILURE: u8 = 6;
pub const EXT_EXIT_SERVER_SHUTDOWN: u8 = 7;
pub const EXT_EXIT_RESOURCE_LIMIT: u8 = 8;

pub const EXT_STATUS_OK: u8 = crate::STATUS_OK;
pub const EXT_STATUS_UNKNOWN_ID: u8 = crate::STATUS_UNKNOWN_ID;
pub const EXT_STATUS_NOT_FOUND: u8 = crate::STATUS_NOT_FOUND;
pub const EXT_STATUS_PERMISSION: u8 = crate::STATUS_PERMISSION;
pub const EXT_STATUS_TOO_LARGE: u8 = crate::STATUS_TOO_LARGE;
pub const EXT_STATUS_BUDGET: u8 = crate::STATUS_BUDGET;
pub const EXT_STATUS_INVALID: u8 = crate::STATUS_INVALID;
pub const EXT_STATUS_CANCELLED: u8 = crate::STATUS_CANCELLED;
pub const EXT_STATUS_OTHER: u8 = crate::STATUS_OTHER;
pub const EXT_STATUS_CONFLICT: u8 = crate::STATUS_CONFLICT;
pub const EXT_PUT_STATUS_ALREADY_HAVE: u8 = 128;
pub const EXT_PUT_ALREADY_HAVE: u8 = EXT_PUT_STATUS_ALREADY_HAVE;

pub const EXT_MAX_NAME: usize = 255;
pub const EXT_MAX_ARGS: usize = 1024;
pub const EXT_MAX_ARG: usize = 64 * 1024;
pub const EXT_MAX_ARGUMENT_BYTES: usize = 1024 * 1024;
pub const EXT_MAX_MODULE: u64 = 64 * 1024 * 1024;
pub const EXT_MAX_EVENT: usize = 1024 * 1024;
pub const EXT_MAX_DESCRIPTOR: usize = 64 * 1024;
pub const EXT_MAX_DETAIL: usize = 4 * 1024;
pub const EXT_MAX_COMMAND_RECORDS: usize = 32;
pub const EXT_MAX_COMMANDS_PACKET: usize = 4 * 1024 * 1024;

const EXTENSION_RECORD_FIXED_BYTES: usize = 89;
const COMMAND_RECORD_FIXED_BYTES: usize = 72;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionRequest<'a> {
    Run {
        nonce: u16,
        flags: u8,
        restart: u8,
        expected_extension_id: u64,
        expected_definition_revision: u64,
        hash: [u8; 32],
        name: &'a str,
        args: Vec<&'a [u8]>,
    },
    Put {
        nonce: u16,
        flags: u8,
        hash: [u8; 32],
        offset: u64,
        total_size: u64,
        data: &'a [u8],
    },
    Control {
        nonce: u16,
        extension_id: u64,
        action: u8,
    },
    Event {
        kind: u8,
        data: &'a [u8],
    },
    CommandRegister {
        nonce: u16,
        listener_id: u32,
        descriptor: &'a str,
    },
    CommandDiscover {
        nonce: u16,
        directory_revision: u64,
        cursor: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionRunRequest<'a> {
    pub nonce: u16,
    pub flags: u8,
    pub restart: u8,
    pub expected_extension_id: u64,
    pub expected_definition_revision: u64,
    pub hash: [u8; 32],
    pub name: &'a str,
    pub args: Vec<&'a [u8]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionPutRequest<'a> {
    pub nonce: u16,
    pub flags: u8,
    pub hash: [u8; 32],
    pub offset: u64,
    pub total_size: u64,
    pub data: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionInit<'a> {
    pub extension_id: u64,
    pub definition_revision: u64,
    pub attempt: u64,
    pub task_id: u32,
    pub flags: u8,
    pub hash: [u8; 32],
    pub name: &'a str,
    pub args: Vec<&'a [u8]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionStatus<'a> {
    pub nonce: u16,
    pub status: u8,
    pub phase: u8,
    pub flags: u8,
    pub restart: u8,
    pub extension_id: u64,
    pub definition_revision: u64,
    pub attempt: u64,
    pub last_running_attempt: u64,
    pub task_id: u32,
    pub replay_from_sequence: u64,
    pub output_sequence: u64,
    pub next_start_unix_ms: u64,
    pub hash: [u8; 32],
    pub detail: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionPutStatus<'a> {
    pub nonce: u16,
    pub status: u8,
    pub hash: [u8; 32],
    pub received: u64,
    pub detail: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionRecord<'a> {
    pub extension_id: u64,
    pub definition_revision: u64,
    pub phase: u8,
    pub flags: u8,
    pub restart: u8,
    pub attempt: u64,
    pub last_running_attempt: u64,
    pub task_id: u32,
    pub output_sequence: u64,
    pub next_start_unix_ms: u64,
    pub hash: [u8; 32],
    pub name: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionInfoStatus<'a> {
    pub extension_id: u64,
    pub definition_revision: u64,
    pub phase: u8,
    pub flags: u8,
    pub restart: u8,
    pub attempt: u64,
    pub last_running_attempt: u64,
    pub task_id: u32,
    pub output_sequence: u64,
    pub next_start_unix_ms: u64,
    pub hash: [u8; 32],
    pub detail: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionCommandRegistered<'a> {
    pub nonce: u16,
    pub status: u8,
    pub extension_id: u64,
    pub definition_revision: u64,
    pub detail: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRecord<'a> {
    pub extension_id: u64,
    pub definition_revision: u64,
    pub hash: [u8; 32],
    pub name: &'a str,
    pub listener_name: &'a str,
    pub listener_token: [u8; 16],
    pub descriptor: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionOutputEvent<'a> {
    pub extension_id: u64,
    pub definition_revision: u64,
    pub attempt: u64,
    pub task_id: u32,
    pub output_sequence: u64,
    pub kind: u8,
    pub data: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionExit<'a> {
    pub extension_id: u64,
    pub definition_revision: u64,
    pub attempt: u64,
    pub task_id: u32,
    pub output_sequence: u64,
    pub reason: u8,
    pub code: i32,
    pub next_start_unix_ms: u64,
    pub detail: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionInfo<'a> {
    Init(ExtensionInit<'a>),
    List {
        nonce: u16,
        status: u8,
        records: Vec<ExtensionRecord<'a>>,
    },
    Status(ExtensionInfoStatus<'a>),
    CommandRegistered(ExtensionCommandRegistered<'a>),
    Commands {
        nonce: u16,
        status: u8,
        directory_revision: u64,
        next_cursor: u64,
        records: Vec<CommandRecord<'a>>,
    },
    ReplayDone {
        extension_id: u64,
        through_sequence: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionMessage<'a> {
    Status(ExtensionStatus<'a>),
    PutStatus(ExtensionPutStatus<'a>),
    Info(ExtensionInfo<'a>),
    Event(ExtensionOutputEvent<'a>),
    Exit(ExtensionExit<'a>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionDecodeError {
    NotExtension,
    Truncated,
    TrailingBytes,
    InvalidFlags,
    InvalidRestart,
    InvalidIdentity,
    InvalidName,
    InvalidUtf8,
    InvalidArguments,
    InvalidUpload,
    InvalidControl,
    InvalidEvent,
    InvalidRecord,
    InvalidCommand,
    InvalidExit,
    TooLarge,
}

impl fmt::Display for ExtensionDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotExtension => "not an extension packet",
            Self::Truncated => "extension packet is truncated",
            Self::TrailingBytes => "extension packet has trailing bytes",
            Self::InvalidFlags => "extension flags are invalid",
            Self::InvalidRestart => "extension restart policy is invalid",
            Self::InvalidIdentity => "extension identity fields are invalid",
            Self::InvalidName => "extension name is invalid",
            Self::InvalidUtf8 => "extension text is not valid UTF-8",
            Self::InvalidArguments => "extension arguments are invalid",
            Self::InvalidUpload => "extension upload fields are invalid",
            Self::InvalidControl => "extension control fields are invalid",
            Self::InvalidEvent => "extension event is invalid",
            Self::InvalidRecord => "extension record is invalid",
            Self::InvalidCommand => "extension command record is invalid",
            Self::InvalidExit => "extension exit is invalid",
            Self::TooLarge => "extension field exceeds its size limit",
        })
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8], offset: usize) -> Self {
        Self { bytes, offset }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ExtensionDecodeError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(ExtensionDecodeError::TooLarge)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(ExtensionDecodeError::Truncated)?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, ExtensionDecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ExtensionDecodeError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("fixed length"),
        ))
    }

    fn u32(&mut self) -> Result<u32, ExtensionDecodeError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("fixed length"),
        ))
    }

    fn u64(&mut self) -> Result<u64, ExtensionDecodeError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("fixed length"),
        ))
    }

    fn i32(&mut self) -> Result<i32, ExtensionDecodeError> {
        Ok(i32::from_le_bytes(
            self.take(4)?.try_into().expect("fixed length"),
        ))
    }

    fn hash(&mut self) -> Result<[u8; 32], ExtensionDecodeError> {
        Ok(self.take(32)?.try_into().expect("fixed length"))
    }

    fn rest(&mut self) -> &'a [u8] {
        let rest = &self.bytes[self.offset..];
        self.offset = self.bytes.len();
        rest
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn finish(&self) -> Result<(), ExtensionDecodeError> {
        (self.offset == self.bytes.len())
            .then_some(())
            .ok_or(ExtensionDecodeError::TrailingBytes)
    }
}

pub fn parse_extension_request(
    packet: &[u8],
) -> Result<Option<ExtensionRequest<'_>>, ExtensionDecodeError> {
    if packet.len() > crate::MAX_FRAME_SIZE {
        return Err(ExtensionDecodeError::TooLarge);
    }
    let Some(&opcode) = packet.first() else {
        return Err(ExtensionDecodeError::Truncated);
    };
    if !(EXT_RUN..=EXT_COMMAND).contains(&opcode) {
        return Err(ExtensionDecodeError::NotExtension);
    }
    let mut decoder = Decoder::new(packet, 1);
    match opcode {
        EXT_RUN => parse_run(&mut decoder).map(Some),
        EXT_PUT => parse_put(&mut decoder).map(Some),
        EXT_CONTROL => {
            let nonce = decoder.u16()?;
            let extension_id = decoder.u64()?;
            let action = decoder.u8()?;
            if !(EXT_CONTROL_CANCEL..=EXT_CONTROL_LIST).contains(&action)
                || action == EXT_CONTROL_LIST && extension_id != 0
                || action != EXT_CONTROL_LIST && extension_id == 0
            {
                return Err(ExtensionDecodeError::InvalidControl);
            }
            let request = ExtensionRequest::Control {
                nonce,
                extension_id,
                action,
            };
            decoder.finish()?;
            Ok(Some(request))
        }
        EXT_EVENT => {
            let kind = decoder.u8()?;
            let data = decoder.rest();
            if data.len() > EXT_MAX_EVENT
                || !matches!(kind, EXT_EVENT_STDOUT | EXT_EVENT_STDERR | EXT_EVENT_LOG)
                || (kind == EXT_EVENT_LOG && std::str::from_utf8(data).is_err())
            {
                return Err(ExtensionDecodeError::InvalidEvent);
            }
            Ok(Some(ExtensionRequest::Event { kind, data }))
        }
        EXT_COMMAND => parse_command(&mut decoder),
        _ => unreachable!("range checked"),
    }
}

fn parse_run<'a>(decoder: &mut Decoder<'a>) -> Result<ExtensionRequest<'a>, ExtensionDecodeError> {
    let nonce = decoder.u16()?;
    let flags = decoder.u8()?;
    let restart = decoder.u8()?;
    let expected_extension_id = decoder.u64()?;
    let expected_definition_revision = decoder.u64()?;
    let hash = decoder.hash()?;
    let name_len = decoder.u16()? as usize;
    let name = decode_name(decoder.take(name_len)?)?;
    let args = decode_args(decoder)?;
    decoder.finish()?;

    validate_run_fields(
        flags,
        restart,
        expected_extension_id,
        expected_definition_revision,
        name,
        &args,
    )?;

    Ok(ExtensionRequest::Run {
        nonce,
        flags,
        restart,
        expected_extension_id,
        expected_definition_revision,
        hash,
        name,
        args,
    })
}

fn parse_put<'a>(decoder: &mut Decoder<'a>) -> Result<ExtensionRequest<'a>, ExtensionDecodeError> {
    let nonce = decoder.u16()?;
    let flags = decoder.u8()?;
    let hash = decoder.hash()?;
    let offset = decoder.u64()?;
    let total_size = decoder.u64()?;
    let data = decoder.rest();
    validate_put_fields(flags, offset, total_size, data.len())?;
    Ok(ExtensionRequest::Put {
        nonce,
        flags,
        hash,
        offset,
        total_size,
        data,
    })
}

fn parse_command<'a>(
    decoder: &mut Decoder<'a>,
) -> Result<Option<ExtensionRequest<'a>>, ExtensionDecodeError> {
    let kind = decoder.u8()?;
    match kind {
        EXT_COMMAND_REGISTER => {
            let nonce = decoder.u16()?;
            let listener_id = decoder.u32()?;
            let descriptor_len = decoder.u32()? as usize;
            if descriptor_len > EXT_MAX_DESCRIPTOR {
                return Err(ExtensionDecodeError::TooLarge);
            }
            let descriptor = std::str::from_utf8(decoder.take(descriptor_len)?)
                .map_err(|_| ExtensionDecodeError::InvalidUtf8)?;
            decoder.finish()?;
            Ok(Some(ExtensionRequest::CommandRegister {
                nonce,
                listener_id,
                descriptor,
            }))
        }
        EXT_COMMAND_DISCOVER => {
            let nonce = decoder.u16()?;
            let request = ExtensionRequest::CommandDiscover {
                nonce,
                directory_revision: decoder.u64()?,
                cursor: decoder.u64()?,
            };
            decoder.finish()?;
            Ok(Some(request))
        }
        _ => Ok(None),
    }
}

pub fn msg_extension_run(request: &ExtensionRunRequest<'_>) -> Option<Vec<u8>> {
    validate_run_fields(
        request.flags,
        request.restart,
        request.expected_extension_id,
        request.expected_definition_revision,
        request.name,
        &request.args,
    )
    .ok()?;
    let args_len = encoded_args_len(&request.args)?;
    let len = 1usize
        .checked_add(2 + 1 + 1 + 8 + 8 + 32 + 2)?
        .checked_add(request.name.len())?
        .checked_add(args_len)?;
    if len > crate::MAX_FRAME_SIZE {
        return None;
    }
    let mut message = Vec::with_capacity(len);
    message.push(EXT_RUN);
    message.extend_from_slice(&request.nonce.to_le_bytes());
    message.push(request.flags);
    message.push(request.restart);
    message.extend_from_slice(&request.expected_extension_id.to_le_bytes());
    message.extend_from_slice(&request.expected_definition_revision.to_le_bytes());
    message.extend_from_slice(&request.hash);
    message.extend_from_slice(&(request.name.len() as u16).to_le_bytes());
    message.extend_from_slice(request.name.as_bytes());
    push_args(&mut message, &request.args);
    Some(message)
}

pub fn msg_extension_put(request: &ExtensionPutRequest<'_>) -> Option<Vec<u8>> {
    validate_put_fields(
        request.flags,
        request.offset,
        request.total_size,
        request.data.len(),
    )
    .ok()?;
    let len = 1usize
        .checked_add(2 + 1 + 32 + 8 + 8)?
        .checked_add(request.data.len())?;
    if len > crate::MAX_FRAME_SIZE {
        return None;
    }
    let mut message = Vec::with_capacity(len);
    message.push(EXT_PUT);
    message.extend_from_slice(&request.nonce.to_le_bytes());
    message.push(request.flags);
    message.extend_from_slice(&request.hash);
    message.extend_from_slice(&request.offset.to_le_bytes());
    message.extend_from_slice(&request.total_size.to_le_bytes());
    message.extend_from_slice(request.data);
    Some(message)
}

pub fn msg_extension_control(nonce: u16, extension_id: u64, action: u8) -> Option<Vec<u8>> {
    if !(EXT_CONTROL_CANCEL..=EXT_CONTROL_LIST).contains(&action)
        || action == EXT_CONTROL_LIST && extension_id != 0
        || action != EXT_CONTROL_LIST && extension_id == 0
    {
        return None;
    }
    let mut message = Vec::with_capacity(12);
    message.push(EXT_CONTROL);
    message.extend_from_slice(&nonce.to_le_bytes());
    message.extend_from_slice(&extension_id.to_le_bytes());
    message.push(action);
    Some(message)
}

pub fn msg_extension_event(kind: u8, data: &[u8]) -> Option<Vec<u8>> {
    if data.len() > EXT_MAX_EVENT
        || !matches!(kind, EXT_EVENT_STDOUT | EXT_EVENT_STDERR | EXT_EVENT_LOG)
        || kind == EXT_EVENT_LOG && std::str::from_utf8(data).is_err()
    {
        return None;
    }
    let mut message = Vec::with_capacity(2 + data.len());
    message.extend_from_slice(&[EXT_EVENT, kind]);
    message.extend_from_slice(data);
    Some(message)
}

pub fn msg_extension_command_register(
    nonce: u16,
    listener_id: u32,
    descriptor: &str,
) -> Option<Vec<u8>> {
    if descriptor.len() > EXT_MAX_DESCRIPTOR || (listener_id == 0) != descriptor.is_empty() {
        return None;
    }
    let descriptor_len = u32::try_from(descriptor.len()).ok()?;
    let mut message = Vec::with_capacity(8 + descriptor.len());
    message.extend_from_slice(&[EXT_COMMAND, EXT_COMMAND_REGISTER]);
    message.extend_from_slice(&nonce.to_le_bytes());
    message.extend_from_slice(&listener_id.to_le_bytes());
    message.extend_from_slice(&descriptor_len.to_le_bytes());
    message.extend_from_slice(descriptor.as_bytes());
    Some(message)
}

pub fn msg_extension_command_discover(
    nonce: u16,
    directory_revision: u64,
    cursor: u64,
) -> Option<Vec<u8>> {
    if (directory_revision == 0) != (cursor == 0) {
        return None;
    }
    let mut message = Vec::with_capacity(20);
    message.extend_from_slice(&[EXT_COMMAND, EXT_COMMAND_DISCOVER]);
    message.extend_from_slice(&nonce.to_le_bytes());
    message.extend_from_slice(&directory_revision.to_le_bytes());
    message.extend_from_slice(&cursor.to_le_bytes());
    Some(message)
}

/// Decode one server-to-client extension packet. Unknown `EXT_INFO` kinds use
/// the RFC skip rule and return `Ok(None)`; unknown scalar status, phase, event
/// kind, and exit-reason values remain present in the decoded record.
pub fn parse_extension_message(
    packet: &[u8],
) -> Result<Option<ExtensionMessage<'_>>, ExtensionDecodeError> {
    if packet.len() > crate::MAX_LOGICAL_MESSAGE {
        return Err(ExtensionDecodeError::TooLarge);
    }
    let Some(&opcode) = packet.first() else {
        return Err(ExtensionDecodeError::Truncated);
    };
    let mut decoder = Decoder::new(packet, 1);
    match opcode {
        EXT_STATUS => parse_status(&mut decoder).map(|value| Some(ExtensionMessage::Status(value))),
        EXT_PUT_STATUS => {
            parse_put_status(&mut decoder).map(|value| Some(ExtensionMessage::PutStatus(value)))
        }
        EXT_INFO => parse_info(packet),
        EXT_OUTPUT_EVENT => {
            parse_output_event(&mut decoder).map(|value| Some(ExtensionMessage::Event(value)))
        }
        EXT_EXIT => parse_exit(&mut decoder).map(|value| Some(ExtensionMessage::Exit(value))),
        _ => Err(ExtensionDecodeError::NotExtension),
    }
}

fn parse_status<'a>(
    decoder: &mut Decoder<'a>,
) -> Result<ExtensionStatus<'a>, ExtensionDecodeError> {
    let value = ExtensionStatus {
        nonce: decoder.u16()?,
        status: decoder.u8()?,
        phase: decoder.u8()?,
        flags: decoder.u8()?,
        restart: decoder.u8()?,
        extension_id: decoder.u64()?,
        definition_revision: decoder.u64()?,
        attempt: decoder.u64()?,
        last_running_attempt: decoder.u64()?,
        task_id: decoder.u32()?,
        replay_from_sequence: decoder.u64()?,
        output_sequence: decoder.u64()?,
        next_start_unix_ms: decoder.u64()?,
        hash: decoder.hash()?,
        detail: decode_detail(decoder.rest())?,
    };
    validate_lifecycle_fields(
        value.flags,
        value.restart,
        value.phase,
        value.attempt,
        value.last_running_attempt,
        value.task_id,
        value.next_start_unix_ms,
    )?;
    Ok(value)
}

fn parse_put_status<'a>(
    decoder: &mut Decoder<'a>,
) -> Result<ExtensionPutStatus<'a>, ExtensionDecodeError> {
    let value = ExtensionPutStatus {
        nonce: decoder.u16()?,
        status: decoder.u8()?,
        hash: decoder.hash()?,
        received: decoder.u64()?,
        detail: decode_detail(decoder.rest())?,
    };
    if value.received > EXT_MAX_MODULE
        || valid_extension_status(value.status)
            && !matches!(value.status, EXT_STATUS_OK)
            && value.received != 0
        || value.status == EXT_PUT_ALREADY_HAVE && value.received == 0
    {
        return Err(ExtensionDecodeError::InvalidRecord);
    }
    Ok(value)
}

fn parse_info(packet: &[u8]) -> Result<Option<ExtensionMessage<'_>>, ExtensionDecodeError> {
    let Some(&kind) = packet.get(1) else {
        return Err(ExtensionDecodeError::Truncated);
    };
    if kind == EXT_INFO_INIT {
        return parse_extension_init(packet)
            .map(ExtensionInfo::Init)
            .map(ExtensionMessage::Info)
            .map(Some);
    }
    let mut decoder = Decoder::new(packet, 2);
    let info = match kind {
        EXT_INFO_LIST => {
            let nonce = decoder.u16()?;
            let status = decoder.u8()?;
            let count = decoder.u16()? as usize;
            ensure_record_count(&decoder, count, EXTENSION_RECORD_FIXED_BYTES)?;
            let mut records = Vec::with_capacity(count);
            for _ in 0..count {
                records.push(parse_extension_record(&mut decoder)?);
            }
            decoder.finish()?;
            ExtensionInfo::List {
                nonce,
                status,
                records,
            }
        }
        EXT_INFO_STATUS => ExtensionInfo::Status(parse_info_status(&mut decoder)?),
        EXT_INFO_COMMAND_REGISTERED => {
            ExtensionInfo::CommandRegistered(ExtensionCommandRegistered {
                nonce: decoder.u16()?,
                status: decoder.u8()?,
                extension_id: decoder.u64()?,
                definition_revision: decoder.u64()?,
                detail: decode_detail(decoder.rest())?,
            })
        }
        EXT_INFO_COMMANDS => {
            if packet.len() > EXT_MAX_COMMANDS_PACKET {
                return Err(ExtensionDecodeError::TooLarge);
            }
            let nonce = decoder.u16()?;
            let status = decoder.u8()?;
            let directory_revision = decoder.u64()?;
            let next_cursor = decoder.u64()?;
            let count = decoder.u16()? as usize;
            if count > EXT_MAX_COMMAND_RECORDS {
                return Err(ExtensionDecodeError::TooLarge);
            }
            ensure_record_count(&decoder, count, COMMAND_RECORD_FIXED_BYTES)?;
            let mut records = Vec::with_capacity(count);
            for _ in 0..count {
                records.push(parse_command_record(&mut decoder)?);
            }
            decoder.finish()?;
            ExtensionInfo::Commands {
                nonce,
                status,
                directory_revision,
                next_cursor,
                records,
            }
        }
        EXT_INFO_REPLAY_DONE => {
            let extension_id = decoder.u64()?;
            if extension_id == 0 {
                return Err(ExtensionDecodeError::InvalidIdentity);
            }
            let info = ExtensionInfo::ReplayDone {
                extension_id,
                through_sequence: decoder.u64()?,
            };
            decoder.finish()?;
            info
        }
        _ => return Ok(None),
    };
    Ok(Some(ExtensionMessage::Info(info)))
}

fn parse_info_status<'a>(
    decoder: &mut Decoder<'a>,
) -> Result<ExtensionInfoStatus<'a>, ExtensionDecodeError> {
    let value = ExtensionInfoStatus {
        extension_id: decoder.u64()?,
        definition_revision: decoder.u64()?,
        phase: decoder.u8()?,
        flags: decoder.u8()?,
        restart: decoder.u8()?,
        attempt: decoder.u64()?,
        last_running_attempt: decoder.u64()?,
        task_id: decoder.u32()?,
        output_sequence: decoder.u64()?,
        next_start_unix_ms: decoder.u64()?,
        hash: decoder.hash()?,
        detail: decode_detail(decoder.rest())?,
    };
    if value.extension_id == 0 || value.definition_revision == 0 || value.output_sequence == 0 {
        return Err(ExtensionDecodeError::InvalidIdentity);
    }
    validate_lifecycle_fields(
        value.flags,
        value.restart,
        value.phase,
        value.attempt,
        value.last_running_attempt,
        value.task_id,
        value.next_start_unix_ms,
    )?;
    Ok(value)
}

fn parse_extension_record<'a>(
    decoder: &mut Decoder<'a>,
) -> Result<ExtensionRecord<'a>, ExtensionDecodeError> {
    let value = ExtensionRecord {
        extension_id: decoder.u64()?,
        definition_revision: decoder.u64()?,
        phase: decoder.u8()?,
        flags: decoder.u8()?,
        restart: decoder.u8()?,
        attempt: decoder.u64()?,
        last_running_attempt: decoder.u64()?,
        task_id: decoder.u32()?,
        output_sequence: decoder.u64()?,
        next_start_unix_ms: decoder.u64()?,
        hash: decoder.hash()?,
        name: decode_len_name(decoder)?,
    };
    validate_extension_record(&value)?;
    Ok(value)
}

fn parse_command_record<'a>(
    decoder: &mut Decoder<'a>,
) -> Result<CommandRecord<'a>, ExtensionDecodeError> {
    let extension_id = decoder.u64()?;
    let definition_revision = decoder.u64()?;
    let hash = decoder.hash()?;
    let name = decode_len_name(decoder)?;
    let listener_name = decode_len_nonempty_name(decoder)?;
    let listener_token = decoder.take(16)?.try_into().expect("fixed length");
    let descriptor_len = decoder.u32()? as usize;
    if descriptor_len > EXT_MAX_DESCRIPTOR {
        return Err(ExtensionDecodeError::TooLarge);
    }
    let descriptor = std::str::from_utf8(decoder.take(descriptor_len)?)
        .map_err(|_| ExtensionDecodeError::InvalidUtf8)?;
    let value = CommandRecord {
        extension_id,
        definition_revision,
        hash,
        name,
        listener_name,
        listener_token,
        descriptor,
    };
    validate_command_record(&value)?;
    Ok(value)
}

fn parse_output_event<'a>(
    decoder: &mut Decoder<'a>,
) -> Result<ExtensionOutputEvent<'a>, ExtensionDecodeError> {
    let extension_id = decoder.u64()?;
    let definition_revision = decoder.u64()?;
    let attempt = decoder.u64()?;
    let task_id = decoder.u32()?;
    let output_sequence = decoder.u64()?;
    let kind = decoder.u8()?;
    let data = decoder.rest();
    if extension_id == 0
        || definition_revision == 0
        || attempt == 0
        || task_id == 0
        || output_sequence == 0
        || data.len() > EXT_MAX_EVENT
        || kind == EXT_EVENT_LOG && std::str::from_utf8(data).is_err()
    {
        return Err(ExtensionDecodeError::InvalidEvent);
    }
    Ok(ExtensionOutputEvent {
        extension_id,
        definition_revision,
        attempt,
        task_id,
        output_sequence,
        kind,
        data,
    })
}

fn parse_exit<'a>(decoder: &mut Decoder<'a>) -> Result<ExtensionExit<'a>, ExtensionDecodeError> {
    let value = ExtensionExit {
        extension_id: decoder.u64()?,
        definition_revision: decoder.u64()?,
        attempt: decoder.u64()?,
        task_id: decoder.u32()?,
        output_sequence: decoder.u64()?,
        reason: decoder.u8()?,
        code: decoder.i32()?,
        next_start_unix_ms: decoder.u64()?,
        detail: decode_detail(decoder.rest())?,
    };
    if value.extension_id == 0
        || value.definition_revision == 0
        || value.attempt == 0
        || value.task_id == 0
        || value.output_sequence == 0
        || value.reason <= EXT_EXIT_RESOURCE_LIMIT
            && value.reason != EXT_EXIT_RETURNED
            && value.code != 0
    {
        return Err(ExtensionDecodeError::InvalidExit);
    }
    Ok(value)
}

pub fn msg_extension_status(status: &ExtensionStatus<'_>) -> Option<Vec<u8>> {
    if !valid_extension_status(status.status)
        || status.phase > EXT_PHASE_STOPPING
        || validate_lifecycle_fields(
            status.flags,
            status.restart,
            status.phase,
            status.attempt,
            status.last_running_attempt,
            status.task_id,
            status.next_start_unix_ms,
        )
        .is_err()
        || validate_detail(status.detail).is_err()
    {
        return None;
    }
    let mut message = Vec::with_capacity(99 + status.detail.len());
    message.push(EXT_STATUS);
    message.extend_from_slice(&status.nonce.to_le_bytes());
    message.push(status.status);
    message.push(status.phase);
    message.push(status.flags);
    message.push(status.restart);
    message.extend_from_slice(&status.extension_id.to_le_bytes());
    message.extend_from_slice(&status.definition_revision.to_le_bytes());
    message.extend_from_slice(&status.attempt.to_le_bytes());
    message.extend_from_slice(&status.last_running_attempt.to_le_bytes());
    message.extend_from_slice(&status.task_id.to_le_bytes());
    message.extend_from_slice(&status.replay_from_sequence.to_le_bytes());
    message.extend_from_slice(&status.output_sequence.to_le_bytes());
    message.extend_from_slice(&status.next_start_unix_ms.to_le_bytes());
    message.extend_from_slice(&status.hash);
    message.extend_from_slice(status.detail.as_bytes());
    Some(message)
}

pub fn msg_extension_put_status(status: &ExtensionPutStatus<'_>) -> Option<Vec<u8>> {
    if !valid_put_status(status.status, status.received) || validate_detail(status.detail).is_err()
    {
        return None;
    }
    let mut message = Vec::with_capacity(44 + status.detail.len());
    message.push(EXT_PUT_STATUS);
    message.extend_from_slice(&status.nonce.to_le_bytes());
    message.push(status.status);
    message.extend_from_slice(&status.hash);
    message.extend_from_slice(&status.received.to_le_bytes());
    message.extend_from_slice(status.detail.as_bytes());
    Some(message)
}

pub fn msg_extension_list(
    nonce: u16,
    status: u8,
    records: &[ExtensionRecord<'_>],
) -> Option<Vec<u8>> {
    if !valid_extension_status(status) || records.len() > u16::MAX as usize {
        return None;
    }
    let records_len = records.iter().try_fold(0usize, |total, record| {
        validate_server_extension_record(record).ok()?;
        total.checked_add(extension_record_len(record)?)
    })?;
    let len = 7usize.checked_add(records_len)?;
    if len > crate::MAX_LOGICAL_MESSAGE {
        return None;
    }
    let mut message = Vec::with_capacity(len);
    message.extend_from_slice(&[EXT_INFO, EXT_INFO_LIST]);
    message.extend_from_slice(&nonce.to_le_bytes());
    message.push(status);
    message.extend_from_slice(&(records.len() as u16).to_le_bytes());
    for record in records {
        push_extension_record(&mut message, record);
    }
    Some(message)
}

pub fn msg_extension_info_status(status: &ExtensionInfoStatus<'_>) -> Option<Vec<u8>> {
    if status.extension_id == 0
        || status.definition_revision == 0
        || status.output_sequence == 0
        || status.phase == EXT_PHASE_NONE
        || status.phase > EXT_PHASE_STOPPING
        || validate_lifecycle_fields(
            status.flags,
            status.restart,
            status.phase,
            status.attempt,
            status.last_running_attempt,
            status.task_id,
            status.next_start_unix_ms,
        )
        .is_err()
        || validate_detail(status.detail).is_err()
    {
        return None;
    }
    let mut message = Vec::with_capacity(91 + status.detail.len());
    message.extend_from_slice(&[EXT_INFO, EXT_INFO_STATUS]);
    message.extend_from_slice(&status.extension_id.to_le_bytes());
    message.extend_from_slice(&status.definition_revision.to_le_bytes());
    message.push(status.phase);
    message.push(status.flags);
    message.push(status.restart);
    message.extend_from_slice(&status.attempt.to_le_bytes());
    message.extend_from_slice(&status.last_running_attempt.to_le_bytes());
    message.extend_from_slice(&status.task_id.to_le_bytes());
    message.extend_from_slice(&status.output_sequence.to_le_bytes());
    message.extend_from_slice(&status.next_start_unix_ms.to_le_bytes());
    message.extend_from_slice(&status.hash);
    message.extend_from_slice(status.detail.as_bytes());
    Some(message)
}

pub fn msg_extension_command_registered(
    registered: &ExtensionCommandRegistered<'_>,
) -> Option<Vec<u8>> {
    if !valid_extension_status(registered.status) || validate_detail(registered.detail).is_err() {
        return None;
    }
    let mut message = Vec::with_capacity(21 + registered.detail.len());
    message.extend_from_slice(&[EXT_INFO, EXT_INFO_COMMAND_REGISTERED]);
    message.extend_from_slice(&registered.nonce.to_le_bytes());
    message.push(registered.status);
    message.extend_from_slice(&registered.extension_id.to_le_bytes());
    message.extend_from_slice(&registered.definition_revision.to_le_bytes());
    message.extend_from_slice(registered.detail.as_bytes());
    Some(message)
}

pub fn msg_extension_commands(
    nonce: u16,
    status: u8,
    directory_revision: u64,
    next_cursor: u64,
    records: &[CommandRecord<'_>],
) -> Option<Vec<u8>> {
    if !valid_extension_status(status) || records.len() > EXT_MAX_COMMAND_RECORDS {
        return None;
    }
    let records_len = records.iter().try_fold(0usize, |total, record| {
        validate_command_record(record).ok()?;
        total.checked_add(command_record_len(record)?)
    })?;
    let len = 23usize.checked_add(records_len)?;
    if len > EXT_MAX_COMMANDS_PACKET {
        return None;
    }
    let mut message = Vec::with_capacity(len);
    message.extend_from_slice(&[EXT_INFO, EXT_INFO_COMMANDS]);
    message.extend_from_slice(&nonce.to_le_bytes());
    message.push(status);
    message.extend_from_slice(&directory_revision.to_le_bytes());
    message.extend_from_slice(&next_cursor.to_le_bytes());
    message.extend_from_slice(&(records.len() as u16).to_le_bytes());
    for record in records {
        push_command_record(&mut message, record);
    }
    Some(message)
}

pub fn msg_extension_replay_done(extension_id: u64, through_sequence: u64) -> Option<Vec<u8>> {
    if extension_id == 0 {
        return None;
    }
    let mut message = Vec::with_capacity(18);
    message.extend_from_slice(&[EXT_INFO, EXT_INFO_REPLAY_DONE]);
    message.extend_from_slice(&extension_id.to_le_bytes());
    message.extend_from_slice(&through_sequence.to_le_bytes());
    Some(message)
}

pub fn msg_extension_output_event(event: &ExtensionOutputEvent<'_>) -> Option<Vec<u8>> {
    if event.extension_id == 0
        || event.definition_revision == 0
        || event.attempt == 0
        || event.task_id == 0
        || event.output_sequence == 0
        || event.data.len() > EXT_MAX_EVENT
        || !matches!(
            event.kind,
            EXT_EVENT_STDOUT | EXT_EVENT_STDERR | EXT_EVENT_LOG
        )
        || event.kind == EXT_EVENT_LOG && std::str::from_utf8(event.data).is_err()
    {
        return None;
    }
    let mut message = Vec::with_capacity(38 + event.data.len());
    message.push(EXT_OUTPUT_EVENT);
    message.extend_from_slice(&event.extension_id.to_le_bytes());
    message.extend_from_slice(&event.definition_revision.to_le_bytes());
    message.extend_from_slice(&event.attempt.to_le_bytes());
    message.extend_from_slice(&event.task_id.to_le_bytes());
    message.extend_from_slice(&event.output_sequence.to_le_bytes());
    message.push(event.kind);
    message.extend_from_slice(event.data);
    Some(message)
}

pub fn msg_extension_exit(exit: &ExtensionExit<'_>) -> Option<Vec<u8>> {
    if exit.extension_id == 0
        || exit.definition_revision == 0
        || exit.attempt == 0
        || exit.task_id == 0
        || exit.output_sequence == 0
        || exit.reason > EXT_EXIT_RESOURCE_LIMIT
        || exit.reason != EXT_EXIT_RETURNED && exit.code != 0
        || validate_detail(exit.detail).is_err()
    {
        return None;
    }
    let mut message = Vec::with_capacity(50 + exit.detail.len());
    message.push(EXT_EXIT);
    message.extend_from_slice(&exit.extension_id.to_le_bytes());
    message.extend_from_slice(&exit.definition_revision.to_le_bytes());
    message.extend_from_slice(&exit.attempt.to_le_bytes());
    message.extend_from_slice(&exit.task_id.to_le_bytes());
    message.extend_from_slice(&exit.output_sequence.to_le_bytes());
    message.push(exit.reason);
    message.extend_from_slice(&exit.code.to_le_bytes());
    message.extend_from_slice(&exit.next_start_unix_ms.to_le_bytes());
    message.extend_from_slice(exit.detail.as_bytes());
    Some(message)
}

pub fn msg_extension_init(init: &ExtensionInit<'_>) -> Option<Vec<u8>> {
    if init.extension_id == 0
        || init.definition_revision == 0
        || init.attempt == 0
        || init.task_id == 0
        || init.flags & !EXT_FLAGS != 0
        || validate_name(init.name).is_err()
        || validate_args(&init.args).is_err()
    {
        return None;
    }
    let body_len = 8usize
        .checked_add(8)?
        .checked_add(8)?
        .checked_add(4)?
        .checked_add(1)?
        .checked_add(32)?
        .checked_add(2)?
        .checked_add(init.name.len())?
        .checked_add(2)?
        .checked_add(
            init.args
                .iter()
                .try_fold(0usize, |total, arg| total.checked_add(4 + arg.len()))?,
        )?;
    let mut message = Vec::with_capacity(2 + body_len);
    message.extend_from_slice(&[EXT_INFO, EXT_INFO_INIT]);
    message.extend_from_slice(&init.extension_id.to_le_bytes());
    message.extend_from_slice(&init.definition_revision.to_le_bytes());
    message.extend_from_slice(&init.attempt.to_le_bytes());
    message.extend_from_slice(&init.task_id.to_le_bytes());
    message.push(init.flags);
    message.extend_from_slice(&init.hash);
    message.extend_from_slice(&(init.name.len() as u16).to_le_bytes());
    message.extend_from_slice(init.name.as_bytes());
    message.extend_from_slice(&(init.args.len() as u16).to_le_bytes());
    for arg in &init.args {
        message.extend_from_slice(&(arg.len() as u32).to_le_bytes());
        message.extend_from_slice(arg);
    }
    Some(message)
}

pub fn parse_extension_init(packet: &[u8]) -> Result<ExtensionInit<'_>, ExtensionDecodeError> {
    if packet.first() != Some(&EXT_INFO) || packet.get(1) != Some(&EXT_INFO_INIT) {
        return Err(ExtensionDecodeError::NotExtension);
    }
    let mut decoder = Decoder::new(packet, 2);
    let extension_id = decoder.u64()?;
    let definition_revision = decoder.u64()?;
    let attempt = decoder.u64()?;
    let task_id = decoder.u32()?;
    let flags = decoder.u8()?;
    let hash = decoder.hash()?;
    let name_len = decoder.u16()? as usize;
    let name = decode_name(decoder.take(name_len)?)?;
    let args = decode_args(&mut decoder)?;
    decoder.finish()?;
    if extension_id == 0
        || definition_revision == 0
        || attempt == 0
        || task_id == 0
        || flags & !EXT_FLAGS != 0
    {
        return Err(ExtensionDecodeError::InvalidIdentity);
    }
    Ok(ExtensionInit {
        extension_id,
        definition_revision,
        attempt,
        task_id,
        flags,
        hash,
        name,
        args,
    })
}

fn decode_name(bytes: &[u8]) -> Result<&str, ExtensionDecodeError> {
    if bytes.len() > EXT_MAX_NAME {
        return Err(ExtensionDecodeError::InvalidName);
    }
    let name = std::str::from_utf8(bytes).map_err(|_| ExtensionDecodeError::InvalidUtf8)?;
    validate_name(name)?;
    Ok(name)
}

fn validate_name(name: &str) -> Result<(), ExtensionDecodeError> {
    if name.len() > EXT_MAX_NAME || name.chars().any(char::is_control) {
        return Err(ExtensionDecodeError::InvalidName);
    }
    Ok(())
}

fn decode_args<'a>(decoder: &mut Decoder<'a>) -> Result<Vec<&'a [u8]>, ExtensionDecodeError> {
    let count = decoder.u16()? as usize;
    if count > EXT_MAX_ARGS {
        return Err(ExtensionDecodeError::InvalidArguments);
    }
    let mut args = Vec::with_capacity(count);
    let mut total = 0usize;
    for _ in 0..count {
        let len = decoder.u32()? as usize;
        total = total
            .checked_add(len)
            .ok_or(ExtensionDecodeError::InvalidArguments)?;
        if len > EXT_MAX_ARG || total > EXT_MAX_ARGUMENT_BYTES {
            return Err(ExtensionDecodeError::InvalidArguments);
        }
        let arg = decoder.take(len)?;
        std::str::from_utf8(arg).map_err(|_| ExtensionDecodeError::InvalidUtf8)?;
        args.push(arg);
    }
    Ok(args)
}

fn validate_args(args: &[&[u8]]) -> Result<(), ExtensionDecodeError> {
    if args.len() > EXT_MAX_ARGS {
        return Err(ExtensionDecodeError::InvalidArguments);
    }
    let mut total = 0usize;
    for arg in args {
        total = total
            .checked_add(arg.len())
            .ok_or(ExtensionDecodeError::InvalidArguments)?;
        if arg.len() > EXT_MAX_ARG || total > EXT_MAX_ARGUMENT_BYTES {
            return Err(ExtensionDecodeError::InvalidArguments);
        }
        std::str::from_utf8(arg).map_err(|_| ExtensionDecodeError::InvalidUtf8)?;
    }
    Ok(())
}

fn validate_run_fields(
    flags: u8,
    restart: u8,
    expected_extension_id: u64,
    expected_definition_revision: u64,
    name: &str,
    args: &[&[u8]],
) -> Result<(), ExtensionDecodeError> {
    if flags & !EXT_RUN_FLAGS != 0 || flags & EXT_RUN_PERSIST != 0 && flags & EXT_RUN_DETACH == 0 {
        return Err(ExtensionDecodeError::InvalidFlags);
    }
    if restart > EXT_RESTART_ALWAYS {
        return Err(ExtensionDecodeError::InvalidRestart);
    }
    validate_name(name)?;
    validate_args(args)?;
    let update = flags & EXT_RUN_UPDATE != 0;
    if update
        && (flags & (EXT_RUN_DETACH | EXT_RUN_PERSIST) != EXT_RUN_DETACH | EXT_RUN_PERSIST
            || name.is_empty()
            || expected_extension_id == 0
            || expected_definition_revision == 0)
        || !update && (expected_extension_id != 0 || expected_definition_revision != 0)
        || flags & EXT_RUN_PERSIST != 0 && name.is_empty()
    {
        return Err(ExtensionDecodeError::InvalidIdentity);
    }
    Ok(())
}

fn validate_put_fields(
    flags: u8,
    offset: u64,
    total_size: u64,
    data_len: usize,
) -> Result<(), ExtensionDecodeError> {
    let end = offset.checked_add(data_len as u64);
    if flags & !EXT_PUT_FLAGS != 0
        || flags & EXT_PUT_BEGIN != 0 && offset != 0
        || total_size == 0
        || total_size > EXT_MAX_MODULE
        || end.is_none_or(|end| end > total_size)
        || flags & EXT_PUT_FINAL != 0 && end != Some(total_size)
        || flags & EXT_PUT_FINAL == 0 && end == Some(total_size)
    {
        return Err(ExtensionDecodeError::InvalidUpload);
    }
    Ok(())
}

fn encoded_args_len(args: &[&[u8]]) -> Option<usize> {
    args.iter().try_fold(2usize, |total, arg| {
        total.checked_add(4)?.checked_add(arg.len())
    })
}

fn push_args(message: &mut Vec<u8>, args: &[&[u8]]) {
    message.extend_from_slice(&(args.len() as u16).to_le_bytes());
    for arg in args {
        message.extend_from_slice(&(arg.len() as u32).to_le_bytes());
        message.extend_from_slice(arg);
    }
}

fn ensure_record_count(
    decoder: &Decoder<'_>,
    count: usize,
    minimum_record_bytes: usize,
) -> Result<(), ExtensionDecodeError> {
    let minimum = count
        .checked_mul(minimum_record_bytes)
        .ok_or(ExtensionDecodeError::TooLarge)?;
    if minimum > decoder.remaining() {
        return Err(ExtensionDecodeError::Truncated);
    }
    Ok(())
}

fn decode_len_name<'a>(decoder: &mut Decoder<'a>) -> Result<&'a str, ExtensionDecodeError> {
    let len = decoder.u16()? as usize;
    decode_name(decoder.take(len)?)
}

fn decode_len_nonempty_name<'a>(
    decoder: &mut Decoder<'a>,
) -> Result<&'a str, ExtensionDecodeError> {
    let name = decode_len_name(decoder)?;
    if name.is_empty() {
        return Err(ExtensionDecodeError::InvalidName);
    }
    Ok(name)
}

fn decode_detail(bytes: &[u8]) -> Result<&str, ExtensionDecodeError> {
    if bytes.len() > EXT_MAX_DETAIL {
        return Err(ExtensionDecodeError::TooLarge);
    }
    std::str::from_utf8(bytes).map_err(|_| ExtensionDecodeError::InvalidUtf8)
}

fn validate_detail(detail: &str) -> Result<(), ExtensionDecodeError> {
    if detail.len() > EXT_MAX_DETAIL {
        return Err(ExtensionDecodeError::TooLarge);
    }
    Ok(())
}

fn validate_lifecycle_fields(
    flags: u8,
    restart: u8,
    phase: u8,
    attempt: u64,
    last_running_attempt: u64,
    task_id: u32,
    next_start_unix_ms: u64,
) -> Result<(), ExtensionDecodeError> {
    if flags & !EXT_FLAGS != 0 {
        return Err(ExtensionDecodeError::InvalidFlags);
    }
    if restart > EXT_RESTART_ALWAYS {
        return Err(ExtensionDecodeError::InvalidRestart);
    }
    if last_running_attempt > attempt {
        return Err(ExtensionDecodeError::InvalidRecord);
    }
    if phase <= EXT_PHASE_STOPPING
        && ((phase == EXT_PHASE_RUNNING) != (task_id != 0)
            || phase != EXT_PHASE_BACKOFF && next_start_unix_ms != 0)
    {
        return Err(ExtensionDecodeError::InvalidRecord);
    }
    Ok(())
}

fn validate_extension_record(record: &ExtensionRecord<'_>) -> Result<(), ExtensionDecodeError> {
    if record.extension_id == 0 || record.definition_revision == 0 {
        return Err(ExtensionDecodeError::InvalidIdentity);
    }
    validate_name(record.name)?;
    validate_lifecycle_fields(
        record.flags,
        record.restart,
        record.phase,
        record.attempt,
        record.last_running_attempt,
        record.task_id,
        record.next_start_unix_ms,
    )
}

fn validate_server_extension_record(
    record: &ExtensionRecord<'_>,
) -> Result<(), ExtensionDecodeError> {
    validate_extension_record(record)?;
    if record.phase == EXT_PHASE_NONE || record.phase > EXT_PHASE_STOPPING {
        return Err(ExtensionDecodeError::InvalidRecord);
    }
    Ok(())
}

fn extension_record_len(record: &ExtensionRecord<'_>) -> Option<usize> {
    EXTENSION_RECORD_FIXED_BYTES.checked_add(record.name.len())
}

fn push_extension_record(message: &mut Vec<u8>, record: &ExtensionRecord<'_>) {
    message.extend_from_slice(&record.extension_id.to_le_bytes());
    message.extend_from_slice(&record.definition_revision.to_le_bytes());
    message.push(record.phase);
    message.push(record.flags);
    message.push(record.restart);
    message.extend_from_slice(&record.attempt.to_le_bytes());
    message.extend_from_slice(&record.last_running_attempt.to_le_bytes());
    message.extend_from_slice(&record.task_id.to_le_bytes());
    message.extend_from_slice(&record.output_sequence.to_le_bytes());
    message.extend_from_slice(&record.next_start_unix_ms.to_le_bytes());
    message.extend_from_slice(&record.hash);
    message.extend_from_slice(&(record.name.len() as u16).to_le_bytes());
    message.extend_from_slice(record.name.as_bytes());
}

fn validate_command_record(record: &CommandRecord<'_>) -> Result<(), ExtensionDecodeError> {
    if record.extension_id == 0
        || record.definition_revision == 0
        || record.name.is_empty()
        || record.listener_name.is_empty()
        || validate_name(record.name).is_err()
        || validate_name(record.listener_name).is_err()
        || record.descriptor.is_empty()
        || record.descriptor.len() > EXT_MAX_DESCRIPTOR
    {
        return Err(ExtensionDecodeError::InvalidCommand);
    }
    Ok(())
}

fn command_record_len(record: &CommandRecord<'_>) -> Option<usize> {
    COMMAND_RECORD_FIXED_BYTES
        .checked_add(record.name.len())?
        .checked_add(record.listener_name.len())?
        .checked_add(record.descriptor.len())
}

fn push_command_record(message: &mut Vec<u8>, record: &CommandRecord<'_>) {
    message.extend_from_slice(&record.extension_id.to_le_bytes());
    message.extend_from_slice(&record.definition_revision.to_le_bytes());
    message.extend_from_slice(&record.hash);
    message.extend_from_slice(&(record.name.len() as u16).to_le_bytes());
    message.extend_from_slice(record.name.as_bytes());
    message.extend_from_slice(&(record.listener_name.len() as u16).to_le_bytes());
    message.extend_from_slice(record.listener_name.as_bytes());
    message.extend_from_slice(&record.listener_token);
    message.extend_from_slice(&(record.descriptor.len() as u32).to_le_bytes());
    message.extend_from_slice(record.descriptor.as_bytes());
}

fn valid_extension_status(status: u8) -> bool {
    matches!(
        status,
        EXT_STATUS_OK
            | EXT_STATUS_UNKNOWN_ID
            | EXT_STATUS_NOT_FOUND
            | EXT_STATUS_PERMISSION
            | EXT_STATUS_TOO_LARGE
            | EXT_STATUS_BUDGET
            | EXT_STATUS_INVALID
            | EXT_STATUS_CANCELLED
            | EXT_STATUS_OTHER
            | EXT_STATUS_CONFLICT
    )
}

fn valid_put_status(status: u8, received: u64) -> bool {
    (valid_extension_status(status) || status == EXT_PUT_ALREADY_HAVE)
        && received <= EXT_MAX_MODULE
        && (status != EXT_PUT_ALREADY_HAVE || received != 0)
        && (matches!(status, EXT_STATUS_OK | EXT_PUT_ALREADY_HAVE) || received == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status<'a>(detail: &'a str) -> ExtensionStatus<'a> {
        ExtensionStatus {
            nonce: 17,
            status: EXT_STATUS_OK,
            phase: EXT_PHASE_RUNNING,
            flags: EXT_FLAG_DETACH | EXT_FLAG_ENABLED | EXT_FLAG_DESIRED_RUNNING,
            restart: EXT_RESTART_ON_FAILURE,
            extension_id: 41,
            definition_revision: 3,
            attempt: 5,
            last_running_attempt: 5,
            task_id: 22,
            replay_from_sequence: 7,
            output_sequence: 11,
            next_start_unix_ms: 0,
            hash: [0xa1; 32],
            detail,
        }
    }

    fn extension_record<'a>(name: &'a str) -> ExtensionRecord<'a> {
        ExtensionRecord {
            extension_id: 41,
            definition_revision: 3,
            phase: EXT_PHASE_BACKOFF,
            flags: EXT_FLAG_DETACH | EXT_FLAG_ENABLED | EXT_FLAG_DESIRED_RUNNING,
            restart: EXT_RESTART_ALWAYS,
            attempt: 5,
            last_running_attempt: 4,
            task_id: 0,
            output_sequence: 19,
            next_start_unix_ms: 1_900_000_000_000,
            hash: [0xb2; 32],
            name,
        }
    }

    fn command_record<'a>(
        name: &'a str,
        listener_name: &'a str,
        descriptor: &'a str,
    ) -> CommandRecord<'a> {
        CommandRecord {
            extension_id: 41,
            definition_revision: 3,
            hash: [0xc3; 32],
            name,
            listener_name,
            listener_token: [0xd4; 16],
            descriptor,
        }
    }

    fn run_packet(flags: u8, id: u64, revision: u64, name: &str) -> Vec<u8> {
        let mut packet = vec![EXT_RUN];
        packet.extend_from_slice(&7u16.to_le_bytes());
        packet.push(flags);
        packet.push(EXT_RESTART_ON_FAILURE);
        packet.extend_from_slice(&id.to_le_bytes());
        packet.extend_from_slice(&revision.to_le_bytes());
        packet.extend_from_slice(&[4; 32]);
        packet.extend_from_slice(&(name.len() as u16).to_le_bytes());
        packet.extend_from_slice(name.as_bytes());
        packet.extend_from_slice(&2u16.to_le_bytes());
        packet.extend_from_slice(&3u32.to_le_bytes());
        packet.extend_from_slice(b"one");
        packet.extend_from_slice(&3u32.to_le_bytes());
        packet.extend_from_slice(b"two");
        packet
    }

    #[test]
    fn run_create_and_update_decode_with_bounded_arguments() {
        let create = run_packet(EXT_RUN_DETACH, 0, 0, "transient");
        assert!(matches!(
            parse_extension_request(&create).unwrap(),
            Some(ExtensionRequest::Run {
                nonce: 7,
                name: "transient",
                ref args,
                ..
            }) if args == &[b"one".as_slice(), b"two".as_slice()]
        ));

        let update = run_packet(
            EXT_RUN_DETACH | EXT_RUN_PERSIST | EXT_RUN_UPDATE,
            9,
            3,
            "durable",
        );
        assert!(parse_extension_request(&update).is_ok());
    }

    #[test]
    fn contradictory_run_identity_is_rejected() {
        assert_eq!(
            parse_extension_request(&run_packet(0, 1, 0, "")),
            Err(ExtensionDecodeError::InvalidIdentity)
        );
        assert_eq!(
            parse_extension_request(&run_packet(EXT_RUN_PERSIST, 0, 0, "durable")),
            Err(ExtensionDecodeError::InvalidFlags)
        );
    }

    #[test]
    fn put_requires_exact_final_boundaries() {
        let mut packet = vec![EXT_PUT];
        packet.extend_from_slice(&1u16.to_le_bytes());
        packet.push(EXT_PUT_BEGIN | EXT_PUT_FINAL);
        packet.extend_from_slice(&[2; 32]);
        packet.extend_from_slice(&0u64.to_le_bytes());
        packet.extend_from_slice(&3u64.to_le_bytes());
        packet.extend_from_slice(b"was");
        assert!(matches!(
            parse_extension_request(&packet).unwrap(),
            Some(ExtensionRequest::Put {
                offset: 0,
                total_size: 3,
                data: b"was",
                ..
            })
        ));
        packet.pop();
        assert_eq!(
            parse_extension_request(&packet),
            Err(ExtensionDecodeError::InvalidUpload)
        );
    }

    #[test]
    fn control_event_and_commands_decode() {
        let mut control = vec![EXT_CONTROL];
        control.extend_from_slice(&2u16.to_le_bytes());
        control.extend_from_slice(&11u64.to_le_bytes());
        control.push(EXT_CONTROL_ATTACH);
        assert!(matches!(
            parse_extension_request(&control).unwrap(),
            Some(ExtensionRequest::Control {
                extension_id: 11,
                action: EXT_CONTROL_ATTACH,
                ..
            })
        ));

        assert_eq!(
            parse_extension_request(&[EXT_EVENT, EXT_EVENT_LOG, 0xff]),
            Err(ExtensionDecodeError::InvalidEvent)
        );

        let mut discover = vec![EXT_COMMAND, EXT_COMMAND_DISCOVER];
        discover.extend_from_slice(&3u16.to_le_bytes());
        discover.extend_from_slice(&5u64.to_le_bytes());
        discover.extend_from_slice(&8u64.to_le_bytes());
        assert!(matches!(
            parse_extension_request(&discover).unwrap(),
            Some(ExtensionRequest::CommandDiscover {
                directory_revision: 5,
                cursor: 8,
                ..
            })
        ));
    }

    #[test]
    fn init_round_trip_preserves_utf8_arguments() {
        let init = ExtensionInit {
            extension_id: 10,
            definition_revision: 2,
            attempt: 4,
            task_id: 7,
            flags: EXT_FLAG_DETACH | EXT_FLAG_ENABLED | EXT_FLAG_DESIRED_RUNNING,
            hash: [9; 32],
            name: "collector",
            args: vec![b"utf8".as_slice(), "caf\u{e9}".as_bytes()],
        };
        let packet = msg_extension_init(&init).unwrap();
        assert_eq!(parse_extension_init(&packet).unwrap(), init);
    }

    #[test]
    fn init_rejects_zero_identity_and_argument_overflow() {
        let init = ExtensionInit {
            extension_id: 0,
            definition_revision: 1,
            attempt: 1,
            task_id: 1,
            flags: 0,
            hash: [0; 32],
            name: "",
            args: Vec::new(),
        };
        assert!(msg_extension_init(&init).is_none());

        let invalid_utf8 = ExtensionInit {
            extension_id: 1,
            definition_revision: 1,
            attempt: 1,
            task_id: 1,
            flags: 0,
            hash: [0; 32],
            name: "",
            args: vec![&[0xff]],
        };
        assert!(msg_extension_init(&invalid_utf8).is_none());
    }

    #[test]
    fn every_c2s_builder_round_trips() {
        let run = ExtensionRunRequest {
            nonce: 8,
            flags: EXT_RUN_DETACH | EXT_RUN_PERSIST | EXT_RUN_UPDATE,
            restart: EXT_RESTART_ALWAYS,
            expected_extension_id: 91,
            expected_definition_revision: 6,
            hash: [0x11; 32],
            name: "builder",
            args: vec![b"build", "caf\u{e9}".as_bytes()],
        };
        assert_eq!(
            parse_extension_request(&msg_extension_run(&run).unwrap()).unwrap(),
            Some(ExtensionRequest::Run {
                nonce: run.nonce,
                flags: run.flags,
                restart: run.restart,
                expected_extension_id: run.expected_extension_id,
                expected_definition_revision: run.expected_definition_revision,
                hash: run.hash,
                name: run.name,
                args: run.args.clone(),
            })
        );

        let put = ExtensionPutRequest {
            nonce: 9,
            flags: EXT_PUT_BEGIN | EXT_PUT_FINAL,
            hash: [0x22; 32],
            offset: 0,
            total_size: 4,
            data: b"wasm",
        };
        assert_eq!(
            parse_extension_request(&msg_extension_put(&put).unwrap()).unwrap(),
            Some(ExtensionRequest::Put {
                nonce: put.nonce,
                flags: put.flags,
                hash: put.hash,
                offset: put.offset,
                total_size: put.total_size,
                data: put.data,
            })
        );

        assert_eq!(
            parse_extension_request(&msg_extension_control(10, 91, EXT_CONTROL_RESTART).unwrap())
                .unwrap(),
            Some(ExtensionRequest::Control {
                nonce: 10,
                extension_id: 91,
                action: EXT_CONTROL_RESTART,
            })
        );
        assert_eq!(
            parse_extension_request(&msg_extension_event(EXT_EVENT_LOG, b"ready").unwrap())
                .unwrap(),
            Some(ExtensionRequest::Event {
                kind: EXT_EVENT_LOG,
                data: b"ready",
            })
        );
        assert_eq!(
            parse_extension_request(
                &msg_extension_command_register(11, 24, r#"{"protocol":"blit.cli.v1"}"#).unwrap()
            )
            .unwrap(),
            Some(ExtensionRequest::CommandRegister {
                nonce: 11,
                listener_id: 24,
                descriptor: r#"{"protocol":"blit.cli.v1"}"#,
            })
        );
        assert_eq!(
            parse_extension_request(&msg_extension_command_discover(12, 50, 9).unwrap()).unwrap(),
            Some(ExtensionRequest::CommandDiscover {
                nonce: 12,
                directory_revision: 50,
                cursor: 9,
            })
        );
    }

    #[test]
    fn status_and_put_status_round_trip() {
        let status = status("running");
        assert_eq!(
            parse_extension_message(&msg_extension_status(&status).unwrap()).unwrap(),
            Some(ExtensionMessage::Status(status))
        );

        let put = ExtensionPutStatus {
            nonce: 18,
            status: EXT_PUT_ALREADY_HAVE,
            hash: [0xe5; 32],
            received: 1234,
            detail: "cached",
        };
        assert_eq!(
            parse_extension_message(&msg_extension_put_status(&put).unwrap()).unwrap(),
            Some(ExtensionMessage::PutStatus(put))
        );
    }

    #[test]
    fn every_info_kind_round_trips() {
        let init = ExtensionInit {
            extension_id: 41,
            definition_revision: 3,
            attempt: 5,
            task_id: 22,
            flags: EXT_FLAG_DETACH | EXT_FLAG_PERSIST | EXT_FLAG_ENABLED,
            hash: [0x91; 32],
            name: "builder",
            args: vec![b"serve"],
        };
        assert_eq!(
            parse_extension_message(&msg_extension_init(&init).unwrap()).unwrap(),
            Some(ExtensionMessage::Info(ExtensionInfo::Init(init)))
        );

        let record = extension_record("builder");
        assert_eq!(
            parse_extension_message(
                &msg_extension_list(19, EXT_STATUS_OK, std::slice::from_ref(&record)).unwrap()
            )
            .unwrap(),
            Some(ExtensionMessage::Info(ExtensionInfo::List {
                nonce: 19,
                status: EXT_STATUS_OK,
                records: vec![record],
            }))
        );

        let info_status = ExtensionInfoStatus {
            extension_id: 41,
            definition_revision: 3,
            phase: EXT_PHASE_STOPPED,
            flags: EXT_FLAG_DETACH | EXT_FLAG_PERSIST | EXT_FLAG_ENABLED,
            restart: EXT_RESTART_NEVER,
            attempt: 5,
            last_running_attempt: 5,
            task_id: 0,
            output_sequence: 20,
            next_start_unix_ms: 0,
            hash: [0x92; 32],
            detail: "complete",
        };
        assert_eq!(
            parse_extension_message(&msg_extension_info_status(&info_status).unwrap()).unwrap(),
            Some(ExtensionMessage::Info(ExtensionInfo::Status(info_status)))
        );

        let registered = ExtensionCommandRegistered {
            nonce: 20,
            status: EXT_STATUS_OK,
            extension_id: 41,
            definition_revision: 3,
            detail: "",
        };
        assert_eq!(
            parse_extension_message(&msg_extension_command_registered(&registered).unwrap())
                .unwrap(),
            Some(ExtensionMessage::Info(ExtensionInfo::CommandRegistered(
                registered
            )))
        );

        let command = command_record(
            "builder",
            "blit.cli.41.5",
            r#"{"protocol":"blit.cli.v1","summary":"Build","commands":[]}"#,
        );
        assert_eq!(
            parse_extension_message(
                &msg_extension_commands(21, EXT_STATUS_OK, 72, 4, std::slice::from_ref(&command),)
                    .unwrap()
            )
            .unwrap(),
            Some(ExtensionMessage::Info(ExtensionInfo::Commands {
                nonce: 21,
                status: EXT_STATUS_OK,
                directory_revision: 72,
                next_cursor: 4,
                records: vec![command],
            }))
        );

        assert_eq!(
            parse_extension_message(&msg_extension_replay_done(41, 20).unwrap()).unwrap(),
            Some(ExtensionMessage::Info(ExtensionInfo::ReplayDone {
                extension_id: 41,
                through_sequence: 20,
            }))
        );
    }

    #[test]
    fn output_event_and_exit_round_trip() {
        let event = ExtensionOutputEvent {
            extension_id: 41,
            definition_revision: 3,
            attempt: 5,
            task_id: 22,
            output_sequence: 30,
            kind: EXT_EVENT_STDOUT,
            data: b"bytes\0stay binary",
        };
        assert_eq!(
            parse_extension_message(&msg_extension_output_event(&event).unwrap()).unwrap(),
            Some(ExtensionMessage::Event(event))
        );

        let exit = ExtensionExit {
            extension_id: 41,
            definition_revision: 3,
            attempt: 5,
            task_id: 22,
            output_sequence: 31,
            reason: EXT_EXIT_RETURNED,
            code: -9001,
            next_start_unix_ms: 1_900_000_000_000,
            detail: "returned",
        };
        assert_eq!(
            parse_extension_message(&msg_extension_exit(&exit).unwrap()).unwrap(),
            Some(ExtensionMessage::Exit(exit))
        );
    }

    #[test]
    fn unknown_s2c_scalars_are_preserved_and_unknown_info_is_skipped() {
        let mut status_wire = msg_extension_status(&status("")).unwrap();
        status_wire[4] = 200;
        assert!(matches!(
            parse_extension_message(&status_wire).unwrap(),
            Some(ExtensionMessage::Status(ExtensionStatus { phase: 200, .. }))
        ));

        let event = ExtensionOutputEvent {
            extension_id: 1,
            definition_revision: 1,
            attempt: 1,
            task_id: 1,
            output_sequence: 1,
            kind: EXT_EVENT_STDERR,
            data: &[0xff],
        };
        let mut event_wire = msg_extension_output_event(&event).unwrap();
        event_wire[37] = 200;
        assert!(matches!(
            parse_extension_message(&event_wire).unwrap(),
            Some(ExtensionMessage::Event(ExtensionOutputEvent {
                kind: 200,
                data: &[0xff],
                ..
            }))
        ));

        let exit = ExtensionExit {
            extension_id: 1,
            definition_revision: 1,
            attempt: 1,
            task_id: 1,
            output_sequence: 2,
            reason: EXT_EXIT_RETURNED,
            code: 99,
            next_start_unix_ms: 0,
            detail: "",
        };
        let mut exit_wire = msg_extension_exit(&exit).unwrap();
        exit_wire[37] = 200;
        assert!(matches!(
            parse_extension_message(&exit_wire).unwrap(),
            Some(ExtensionMessage::Exit(ExtensionExit {
                reason: 200,
                code: 99,
                ..
            }))
        ));

        assert_eq!(parse_extension_message(&[EXT_INFO, 200, 1, 2, 3]), Ok(None));
    }

    #[test]
    fn builders_reject_reserved_values_and_oversized_fields() {
        assert!(msg_extension_control(1, 4, 0).is_none());
        assert!(msg_extension_control(1, 4, EXT_CONTROL_LIST).is_none());
        assert!(msg_extension_control(1, 0, EXT_CONTROL_STATUS).is_none());
        assert!(msg_extension_command_discover(1, 0, 1).is_none());
        assert!(msg_extension_command_register(1, 0, "not unregister").is_none());
        assert!(msg_extension_event(0, b"").is_none());
        assert!(msg_extension_event(EXT_EVENT_LOG, &[0xff]).is_none());

        let too_long = "x".repeat(EXT_MAX_DETAIL + 1);
        assert!(msg_extension_status(&status(&too_long)).is_none());

        let mut commands = Vec::new();
        for _ in 0..=EXT_MAX_COMMAND_RECORDS {
            commands.push(command_record("x", "listener", "{}"));
        }
        assert!(msg_extension_commands(1, EXT_STATUS_OK, 1, 0, &commands).is_none());

        let invalid_put = ExtensionPutStatus {
            nonce: 1,
            status: EXT_STATUS_CONFLICT,
            hash: [0; 32],
            received: 1,
            detail: "",
        };
        assert!(msg_extension_put_status(&invalid_put).is_none());
    }

    #[test]
    fn parser_rejects_noncanonical_control_identity() {
        let raw = |extension_id: u64, action: u8| {
            let mut packet = vec![EXT_CONTROL];
            packet.extend_from_slice(&7_u16.to_le_bytes());
            packet.extend_from_slice(&extension_id.to_le_bytes());
            packet.push(action);
            packet
        };
        assert_eq!(
            parse_extension_request(&raw(4, EXT_CONTROL_LIST)),
            Err(ExtensionDecodeError::InvalidControl)
        );
        assert_eq!(
            parse_extension_request(&raw(0, EXT_CONTROL_STATUS)),
            Err(ExtensionDecodeError::InvalidControl)
        );
        assert_eq!(
            parse_extension_request(&raw(4, EXT_CONTROL_LIST + 1)),
            Err(ExtensionDecodeError::InvalidControl)
        );
    }

    #[test]
    fn parsers_bound_counts_and_text_before_allocation_or_use() {
        let mut huge_list = vec![EXT_INFO, EXT_INFO_LIST];
        huge_list.extend_from_slice(&1u16.to_le_bytes());
        huge_list.push(EXT_STATUS_OK);
        huge_list.extend_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(
            parse_extension_message(&huge_list),
            Err(ExtensionDecodeError::Truncated)
        );

        let mut too_many_commands = vec![EXT_INFO, EXT_INFO_COMMANDS];
        too_many_commands.extend_from_slice(&1u16.to_le_bytes());
        too_many_commands.push(EXT_STATUS_OK);
        too_many_commands.extend_from_slice(&1u64.to_le_bytes());
        too_many_commands.extend_from_slice(&0u64.to_le_bytes());
        too_many_commands.extend_from_slice(&33u16.to_le_bytes());
        assert_eq!(
            parse_extension_message(&too_many_commands),
            Err(ExtensionDecodeError::TooLarge)
        );

        let mut invalid_detail = msg_extension_status(&status("")).unwrap();
        invalid_detail.push(0xff);
        assert_eq!(
            parse_extension_message(&invalid_detail),
            Err(ExtensionDecodeError::InvalidUtf8)
        );

        let mut trailing_replay = msg_extension_replay_done(1, 2).unwrap();
        trailing_replay.push(0);
        assert_eq!(
            parse_extension_message(&trailing_replay),
            Err(ExtensionDecodeError::TrailingBytes)
        );
    }
}
