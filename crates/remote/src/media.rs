//! Viewer media input, compositor portal, and MPRIS wire protocol.
//!
//! The family intentionally carries normalized values only. D-Bus names,
//! object paths, variants, browser device labels, and PipeWire file
//! descriptors never cross this boundary.

use std::collections::BTreeMap;

use lz4_flex::{compress_prepend_size, decompress_size_prepended};

/// `S2C_HELLO` feature bit: both endpoints understand this wire family.
pub const FEATURE_DESKTOP_MEDIA: u32 = 1 << 22;

pub const C2S_MEDIA_CONTROL: u8 = 0x3e;
pub const C2S_MEDIA_DATA: u8 = 0x3f;
pub const S2C_MEDIA_CONTROL: u8 = 0x35;

pub const MEDIA_FRAGMENT_MAX: usize = 256 * 1024;
pub const MICROPHONE_FRAME_MAX: usize = 64 * 1024;
pub const CAMERA_FRAME_MAX: usize = 4 * 1024 * 1024;
pub const MEDIA_FRAGMENT_COUNT_MAX: u16 = 16;
pub const PORTAL_MESSAGE_MAX: usize = 4 * 1024 * 1024;
pub const PORTAL_PROMPT_MAX: usize = 16 * 1024;
pub const PORTAL_CHOICE_MAX: usize = 16;
pub const PORTAL_CHOICE_OPTION_MAX: usize = 32;
pub const SCREENCAST_CANDIDATE_MAX: usize = 64;
pub const SCREENCAST_STREAM_MAX: usize = 4;
pub const SCREENCAST_THUMBNAIL_MAX: usize = 64 * 1024;
pub const MPRIS_PLAYER_MAX: usize = 32;
pub const MPRIS_ARTIST_MAX: usize = 16;
pub const MPRIS_STRING_MAX: usize = 4 * 1024;
pub const MPRIS_ARTWORK_MAX: usize = 512 * 1024;
pub const MPRIS_UPDATE_MAX_DECOMPRESSED: usize = 16 * 1024 * 1024;

pub const CAPTURE_MICROPHONE: u8 = 1 << 0;
pub const CAPTURE_CAMERA: u8 = 1 << 1;
pub const CAPTURE_PORTAL_UI: u8 = 1 << 2;
pub const CAPTURE_FLAGS_ALL: u8 = CAPTURE_MICROPHONE | CAPTURE_CAMERA | CAPTURE_PORTAL_UI;

pub const AUDIO_CODEC_PCM: u8 = 1 << 0;
pub const AUDIO_CODEC_OPUS: u8 = 1 << 1;
pub const AUDIO_CODECS_ALL: u8 = AUDIO_CODEC_PCM | AUDIO_CODEC_OPUS;
pub const VIDEO_CODEC_MJPEG: u8 = 1 << 0;
pub const VIDEO_CODEC_H264: u8 = 1 << 1;
pub const VIDEO_CODEC_AV1: u8 = 1 << 2;
pub const VIDEO_CODEC_H264_444: u8 = 1 << 3;
pub const VIDEO_CODEC_AV1_444: u8 = 1 << 4;
pub const VIDEO_CODECS_ALL: u8 = VIDEO_CODEC_MJPEG
    | VIDEO_CODEC_H264
    | VIDEO_CODEC_AV1
    | VIDEO_CODEC_H264_444
    | VIDEO_CODEC_AV1_444;

/// Camera codec indices. Their corresponding capability bit is `1 << index`.
/// The compressed variants deliberately mirror the surface-streaming order,
/// shifted by the Motion JPEG compatibility codec at index zero.
pub const CAMERA_CODEC_MJPEG: u8 = 0;
pub const CAMERA_CODEC_H264: u8 = 1;
pub const CAMERA_CODEC_AV1: u8 = 2;
pub const CAMERA_CODEC_H264_444: u8 = 3;
pub const CAMERA_CODEC_AV1_444: u8 = 4;

pub const RUNTIME_PIPEWIRE: u8 = 1 << 0;
pub const RUNTIME_MICROPHONE: u8 = 1 << 1;
pub const RUNTIME_CAMERA: u8 = 1 << 2;
pub const RUNTIME_PORTAL_FRONTEND: u8 = 1 << 3;
pub const RUNTIME_PORTAL_ACCESS: u8 = 1 << 4;
pub const RUNTIME_PORTAL_SCREENCAST: u8 = 1 << 5;
pub const RUNTIME_MPRIS: u8 = 1 << 6;
pub const RUNTIME_FLAGS_ALL: u8 = RUNTIME_PIPEWIRE
    | RUNTIME_MICROPHONE
    | RUNTIME_CAMERA
    | RUNTIME_PORTAL_FRONTEND
    | RUNTIME_PORTAL_ACCESS
    | RUNTIME_PORTAL_SCREENCAST
    | RUNTIME_MPRIS;

pub const ACTIVE_MICROPHONE: u8 = 1 << 0;
pub const ACTIVE_CAMERA: u8 = 1 << 1;
pub const ACTIVE_SCREENCAST: u8 = 1 << 2;
pub const ACTIVE_FLAGS_ALL: u8 = ACTIVE_MICROPHONE | ACTIVE_CAMERA | ACTIVE_SCREENCAST;

pub const MEDIA_DATA_KEYFRAME: u8 = 1 << 0;
pub const MEDIA_DATA_DISCONTINUITY: u8 = 1 << 1;
pub const MEDIA_DATA_END_OF_STREAM: u8 = 1 << 2;
pub const MEDIA_DATA_FLAGS_ALL: u8 =
    MEDIA_DATA_KEYFRAME | MEDIA_DATA_DISCONTINUITY | MEDIA_DATA_END_OF_STREAM;

pub const MEDIA_CREDIT_KEYFRAME: u8 = 1 << 0;

pub const MPRIS_UPDATE_RESET: u8 = 1 << 0;
pub const MPRIS_UPDATE_SYNC: u8 = 1 << 1;
pub const MPRIS_UPDATE_REPLAY: u8 = 1 << 2;
pub const MPRIS_UPDATE_FLAGS_ALL: u8 = MPRIS_UPDATE_RESET | MPRIS_UPDATE_SYNC | MPRIS_UPDATE_REPLAY;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaKind {
    Microphone = 0,
    Camera = 1,
}

impl MediaKind {
    fn parse(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Microphone),
            1 => Some(Self::Camera),
            _ => None,
        }
    }

    pub fn frame_max(self) -> usize {
        match self {
            Self::Microphone => MICROPHONE_FRAME_MAX,
            Self::Camera => CAMERA_FRAME_MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PortalDecision {
    Deny = 0,
    Grant = 1,
    Cancelled = 2,
}

impl PortalDecision {
    fn parse(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Deny),
            1 => Some(Self::Grant),
            2 => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MprisActionKind {
    SelectActive = 0,
    Play = 1,
    Pause = 2,
    PlayPause = 3,
    Stop = 4,
    Next = 5,
    Previous = 6,
    Seek = 7,
    SetPosition = 8,
    Volume = 9,
    Shuffle = 10,
    LoopStatus = 11,
    Rate = 12,
    Raise = 13,
}

impl MprisActionKind {
    fn parse(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::SelectActive),
            1 => Some(Self::Play),
            2 => Some(Self::Pause),
            3 => Some(Self::PlayPause),
            4 => Some(Self::Stop),
            5 => Some(Self::Next),
            6 => Some(Self::Previous),
            7 => Some(Self::Seek),
            8 => Some(Self::SetPosition),
            9 => Some(Self::Volume),
            10 => Some(Self::Shuffle),
            11 => Some(Self::LoopStatus),
            12 => Some(Self::Rate),
            13 => Some(Self::Raise),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PlaybackStatus {
    Stopped = 0,
    Paused = 1,
    Playing = 2,
}

impl PlaybackStatus {
    fn parse(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Stopped),
            1 => Some(Self::Paused),
            2 => Some(Self::Playing),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LoopStatus {
    None = 0,
    Track = 1,
    Playlist = 2,
}

impl LoopStatus {
    fn parse(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::None),
            1 => Some(Self::Track),
            2 => Some(Self::Playlist),
            _ => None,
        }
    }
}

pub const MPRIS_CAN_CONTROL: u16 = 1 << 0;
pub const MPRIS_CAN_PLAY: u16 = 1 << 1;
pub const MPRIS_CAN_PAUSE: u16 = 1 << 2;
pub const MPRIS_CAN_GO_NEXT: u16 = 1 << 3;
pub const MPRIS_CAN_GO_PREVIOUS: u16 = 1 << 4;
pub const MPRIS_CAN_SEEK: u16 = 1 << 5;
pub const MPRIS_CAN_RAISE: u16 = 1 << 6;
pub const MPRIS_CAN_SET_VOLUME: u16 = 1 << 7;
pub const MPRIS_CAN_SET_SHUFFLE: u16 = 1 << 8;
pub const MPRIS_CAN_SET_LOOP_STATUS: u16 = 1 << 9;
pub const MPRIS_CAN_SET_RATE: u16 = 1 << 10;
pub const MPRIS_CAPABILITIES_ALL: u16 = (1 << 11) - 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaCapabilities {
    pub flags: u8,
    pub audio_codecs: u8,
    pub video_codecs: u8,
    pub max_width: u16,
    pub max_height: u16,
    pub max_fps: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaStart {
    pub nonce: u32,
    pub kind: MediaKind,
    /// Codec index within the corresponding codec bit registry.
    pub codec: u8,
    pub width: u16,
    pub height: u16,
    pub fps: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalChoiceValue {
    pub id: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalReply {
    pub request_id: u32,
    pub decision: PortalDecision,
    pub surface_ids: Vec<u16>,
    pub choices: Vec<PortalChoiceValue>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MprisAction {
    pub nonce: u32,
    pub player_id: u32,
    pub kind: MprisActionKind,
    pub track_revision: u32,
    pub value: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientControl {
    Capabilities(MediaCapabilities),
    Start(MediaStart),
    Stop { lease_id: u32 },
    PortalReply(PortalReply),
    ScreenCastStop { session_id: u32 },
    MprisSubscribe { enabled: bool },
    MprisAction(MprisAction),
}

pub fn msg_client_control(control: &ClientControl) -> Vec<u8> {
    let mut out = vec![C2S_MEDIA_CONTROL];
    match control {
        ClientControl::Capabilities(value) => {
            out.push(0);
            out.extend_from_slice(&[
                value.flags & CAPTURE_FLAGS_ALL,
                value.audio_codecs & AUDIO_CODECS_ALL,
                value.video_codecs & VIDEO_CODECS_ALL,
            ]);
            out.extend_from_slice(&value.max_width.to_le_bytes());
            out.extend_from_slice(&value.max_height.to_le_bytes());
            out.push(value.max_fps);
        }
        ClientControl::Start(value) => {
            out.push(1);
            out.extend_from_slice(&value.nonce.to_le_bytes());
            out.extend_from_slice(&[value.kind as u8, value.codec]);
            out.extend_from_slice(&value.width.to_le_bytes());
            out.extend_from_slice(&value.height.to_le_bytes());
            out.push(value.fps);
        }
        ClientControl::Stop { lease_id } => {
            out.push(2);
            out.extend_from_slice(&lease_id.to_le_bytes());
        }
        ClientControl::PortalReply(value) => {
            out.push(3);
            out.extend_from_slice(&value.request_id.to_le_bytes());
            out.push(value.decision as u8);
            let count = value.surface_ids.len().min(SCREENCAST_STREAM_MAX);
            out.push(count as u8);
            for id in &value.surface_ids[..count] {
                out.extend_from_slice(&id.to_le_bytes());
            }
            let count = value.choices.len().min(PORTAL_CHOICE_MAX);
            out.push(count as u8);
            for choice in &value.choices[..count] {
                push_str16(&mut out, &choice.id);
                push_str16(&mut out, &choice.value);
            }
        }
        ClientControl::ScreenCastStop { session_id } => {
            out.push(4);
            out.extend_from_slice(&session_id.to_le_bytes());
        }
        ClientControl::MprisSubscribe { enabled } => {
            out.extend_from_slice(&[5, u8::from(*enabled)]);
        }
        ClientControl::MprisAction(value) => {
            out.push(6);
            out.extend_from_slice(&value.nonce.to_le_bytes());
            out.extend_from_slice(&value.player_id.to_le_bytes());
            out.push(value.kind as u8);
            out.extend_from_slice(&value.track_revision.to_le_bytes());
            out.extend_from_slice(&value.value.to_le_bytes());
        }
    }
    out
}

/// Parse a known client control. Unknown subtypes return `Ok(None)` so a
/// newer peer can extend this family without breaking an older server.
pub fn parse_client_control(msg: &[u8]) -> Result<Option<ClientControl>, &'static str> {
    if msg.len() < 2 || msg[0] != C2S_MEDIA_CONTROL {
        return Err("not a media control message");
    }
    let mut input = &msg[2..];
    let value = match msg[1] {
        0 => {
            if input.len() != 8 {
                return Err("malformed capabilities");
            }
            let flags = take_u8(&mut input)?;
            let audio_codecs = take_u8(&mut input)?;
            let video_codecs = take_u8(&mut input)?;
            if flags & !CAPTURE_FLAGS_ALL != 0
                || audio_codecs & !AUDIO_CODECS_ALL != 0
                || video_codecs & !VIDEO_CODECS_ALL != 0
            {
                return Err("unknown capability bits");
            }
            ClientControl::Capabilities(MediaCapabilities {
                flags,
                audio_codecs,
                video_codecs,
                max_width: take_u16(&mut input)?,
                max_height: take_u16(&mut input)?,
                max_fps: take_u8(&mut input)?,
            })
        }
        1 => {
            if input.len() != 11 {
                return Err("malformed start");
            }
            let nonce = take_u32(&mut input)?;
            if nonce == 0 {
                return Err("zero nonce");
            }
            let kind = MediaKind::parse(take_u8(&mut input)?).ok_or("unknown media kind")?;
            let codec = take_u8(&mut input)?;
            let width = take_u16(&mut input)?;
            let height = take_u16(&mut input)?;
            let fps = take_u8(&mut input)?;
            if (kind == MediaKind::Microphone && (width != 0 || height != 0 || fps != 0))
                || (kind == MediaKind::Camera && (width == 0 || height == 0 || fps == 0))
            {
                return Err("invalid media format");
            }
            ClientControl::Start(MediaStart {
                nonce,
                kind,
                codec,
                width,
                height,
                fps,
            })
        }
        2 => {
            if input.len() != 4 {
                return Err("malformed stop");
            }
            ClientControl::Stop {
                lease_id: nonzero(take_u32(&mut input)?, "zero lease id")?,
            }
        }
        3 => {
            let request_id = nonzero(take_u32(&mut input)?, "zero request id")?;
            let decision =
                PortalDecision::parse(take_u8(&mut input)?).ok_or("unknown portal decision")?;
            let surface_count = take_u8(&mut input)? as usize;
            if surface_count > SCREENCAST_STREAM_MAX {
                return Err("too many surfaces");
            }
            let mut surface_ids = Vec::with_capacity(surface_count);
            for _ in 0..surface_count {
                let id = take_u16(&mut input)?;
                if id == 0 || surface_ids.contains(&id) {
                    return Err("invalid surface id");
                }
                surface_ids.push(id);
            }
            let choice_count = take_u8(&mut input)? as usize;
            if choice_count > PORTAL_CHOICE_MAX {
                return Err("too many choices");
            }
            let mut choices = Vec::with_capacity(choice_count);
            for _ in 0..choice_count {
                choices.push(PortalChoiceValue {
                    id: take_str16(&mut input, MPRIS_STRING_MAX)?,
                    value: take_str16(&mut input, MPRIS_STRING_MAX)?,
                });
            }
            if !input.is_empty()
                || (decision != PortalDecision::Grant
                    && (!surface_ids.is_empty() || !choices.is_empty()))
            {
                return Err("invalid portal reply");
            }
            ClientControl::PortalReply(PortalReply {
                request_id,
                decision,
                surface_ids,
                choices,
            })
        }
        4 => {
            if input.len() != 4 {
                return Err("malformed screencast stop");
            }
            ClientControl::ScreenCastStop {
                session_id: nonzero(take_u32(&mut input)?, "zero session id")?,
            }
        }
        5 => {
            if input.len() != 1 {
                return Err("malformed MPRIS subscription");
            }
            ClientControl::MprisSubscribe {
                enabled: take_bool(&mut input)?,
            }
        }
        6 => {
            if input.len() != 21 {
                return Err("malformed MPRIS action");
            }
            let nonce = nonzero(take_u32(&mut input)?, "zero nonce")?;
            let player_id = nonzero(take_u32(&mut input)?, "zero player id")?;
            let kind = MprisActionKind::parse(take_u8(&mut input)?).ok_or("unknown action")?;
            let track_revision = take_u32(&mut input)?;
            let value = take_i64(&mut input)?;
            if (kind == MprisActionKind::SetPosition) != (track_revision != 0) {
                return Err("invalid track revision");
            }
            ClientControl::MprisAction(MprisAction {
                nonce,
                player_id,
                kind,
                track_revision,
                value,
            })
        }
        _ => return Ok(None),
    };
    if !input.is_empty() {
        return Err("trailing media control bytes");
    }
    Ok(Some(value))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenCastState {
    pub session_id: u32,
    pub app_id: String,
    pub surface_ids: Vec<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaState {
    pub runtime_flags: u8,
    pub active_flags: u8,
    pub microphone_owner: u64,
    pub camera_owner: u64,
    pub screencasts: Vec<ScreenCastState>,
}

/// Server-side camera decoder availability. This is a separate, ignorable
/// record so extended codec bits are never sent to a legacy server that still
/// treats them as reserved capability bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerMediaCapabilities {
    pub video_codecs: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaLease {
    pub nonce: u32,
    pub status: u8,
    pub kind: MediaKind,
    pub lease_id: u32,
    pub codec: u8,
    pub width: u16,
    pub height: u16,
    pub fps: u8,
    pub initial_credit: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RevokeReason {
    Stopped = 0,
    Disconnected = 1,
    DeviceEnded = 2,
    IdleTimeout = 3,
    PipeWireFailed = 4,
    FormatError = 5,
    CreditViolation = 6,
    ServerShutdown = 7,
}

impl RevokeReason {
    fn parse(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Stopped),
            1 => Some(Self::Disconnected),
            2 => Some(Self::DeviceEnded),
            3 => Some(Self::IdleTimeout),
            4 => Some(Self::PipeWireFailed),
            5 => Some(Self::FormatError),
            6 => Some(Self::CreditViolation),
            7 => Some(Self::ServerShutdown),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaRevoked {
    pub lease_id: u32,
    pub reason: RevokeReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaCredit {
    pub lease_id: u32,
    pub bytes: u32,
    pub flags: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalChoice {
    pub id: String,
    pub label: String,
    pub options: Vec<PortalChoiceValue>,
    pub initial_value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalAccessRequest {
    pub request_id: u32,
    pub deadline_ms: u32,
    pub parent_surface_id: Option<u16>,
    pub app_id: String,
    pub title: String,
    pub subtitle: String,
    pub body: String,
    pub deny_label: String,
    pub grant_label: String,
    pub icon_name: String,
    pub choices: Vec<PortalChoice>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenCastCandidate {
    pub surface_id: u16,
    pub width: u16,
    pub height: u16,
    pub title: String,
    pub app_id: String,
    pub thumbnail_png: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalScreenCastRequest {
    pub request_id: u32,
    pub deadline_ms: u32,
    pub parent_surface_id: Option<u16>,
    pub app_id: String,
    pub multiple: bool,
    pub candidates: Vec<ScreenCastCandidate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortalRequest {
    Access(PortalAccessRequest),
    ScreenCast(PortalScreenCastRequest),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortalCancel {
    pub request_id: u32,
    pub reason: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MprisPlayer {
    pub player_id: u32,
    pub revision: u32,
    pub track_revision: u32,
    pub active: bool,
    pub playback_status: PlaybackStatus,
    pub loop_status: LoopStatus,
    pub shuffle: bool,
    pub capability_flags: u16,
    pub rate_ppm: i32,
    pub minimum_rate_ppm: i32,
    pub maximum_rate_ppm: i32,
    pub volume_ppm: u32,
    pub position_us: i64,
    pub length_us: i64,
    pub identity: String,
    pub desktop_entry: String,
    pub title: String,
    pub album: String,
    pub artists: Vec<String>,
    pub artwork_width: u16,
    pub artwork_height: u16,
    pub artwork_png: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MprisRecord {
    Delete { player_id: u32 },
    Upsert(MprisPlayer),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MprisActionResult {
    pub nonce: u32,
    pub status: u8,
    pub player_id: u32,
    pub revision: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerControl {
    ServerCapabilities(ServerMediaCapabilities),
    State(MediaState),
    Lease(MediaLease),
    Revoked(MediaRevoked),
    Credit(MediaCredit),
    PortalRequest(PortalRequest),
    PortalCancel(PortalCancel),
    MprisUpdate {
        flags: u8,
        records: Vec<MprisRecord>,
    },
    MprisActionResult(MprisActionResult),
}

pub fn msg_server_control(control: &ServerControl) -> Vec<u8> {
    match control {
        ServerControl::MprisUpdate { flags, records } => msg_mpris_update(*flags, records),
        _ => {
            let mut out = vec![S2C_MEDIA_CONTROL];
            match control {
                ServerControl::ServerCapabilities(value) => {
                    out.extend_from_slice(&[8, value.video_codecs & VIDEO_CODECS_ALL]);
                }
                ServerControl::State(value) => {
                    out.push(0);
                    out.extend_from_slice(&[
                        value.runtime_flags & RUNTIME_FLAGS_ALL,
                        value.active_flags & ACTIVE_FLAGS_ALL,
                    ]);
                    out.extend_from_slice(&value.microphone_owner.to_le_bytes());
                    out.extend_from_slice(&value.camera_owner.to_le_bytes());
                    let count = value.screencasts.len().min(SCREENCAST_STREAM_MAX);
                    out.push(count as u8);
                    for session in &value.screencasts[..count] {
                        out.extend_from_slice(&session.session_id.to_le_bytes());
                        push_str16(&mut out, &session.app_id);
                        let surfaces = session.surface_ids.len().min(SCREENCAST_STREAM_MAX);
                        out.push(surfaces as u8);
                        for id in &session.surface_ids[..surfaces] {
                            out.extend_from_slice(&id.to_le_bytes());
                        }
                    }
                }
                ServerControl::Lease(value) => {
                    out.push(1);
                    out.extend_from_slice(&value.nonce.to_le_bytes());
                    out.extend_from_slice(&[value.status, value.kind as u8]);
                    out.extend_from_slice(&value.lease_id.to_le_bytes());
                    out.push(value.codec);
                    out.extend_from_slice(&value.width.to_le_bytes());
                    out.extend_from_slice(&value.height.to_le_bytes());
                    out.push(value.fps);
                    out.extend_from_slice(&value.initial_credit.to_le_bytes());
                }
                ServerControl::Revoked(value) => {
                    out.push(2);
                    out.extend_from_slice(&value.lease_id.to_le_bytes());
                    out.push(value.reason as u8);
                }
                ServerControl::Credit(value) => {
                    out.push(3);
                    out.extend_from_slice(&value.lease_id.to_le_bytes());
                    out.extend_from_slice(&value.bytes.to_le_bytes());
                    out.push(value.flags & MEDIA_CREDIT_KEYFRAME);
                }
                ServerControl::PortalRequest(value) => {
                    out.push(4);
                    match value {
                        PortalRequest::Access(request) => {
                            push_portal_common(
                                &mut out,
                                request.request_id,
                                0,
                                request.deadline_ms,
                                request.parent_surface_id,
                            );
                            push_bounded_str16(&mut out, &request.app_id, MPRIS_STRING_MAX);
                            push_bounded_str16(&mut out, &request.title, MPRIS_STRING_MAX);
                            push_bounded_str16(&mut out, &request.subtitle, MPRIS_STRING_MAX);
                            push_bounded_str32(&mut out, &request.body, PORTAL_PROMPT_MAX);
                            push_bounded_str16(&mut out, &request.deny_label, MPRIS_STRING_MAX);
                            push_bounded_str16(&mut out, &request.grant_label, MPRIS_STRING_MAX);
                            push_bounded_str16(&mut out, &request.icon_name, MPRIS_STRING_MAX);
                            let count = request.choices.len().min(PORTAL_CHOICE_MAX);
                            out.push(count as u8);
                            for choice in &request.choices[..count] {
                                push_bounded_str16(&mut out, &choice.id, MPRIS_STRING_MAX);
                                push_bounded_str16(&mut out, &choice.label, MPRIS_STRING_MAX);
                                let options = choice.options.len().min(PORTAL_CHOICE_OPTION_MAX);
                                out.push(options as u8);
                                for option in &choice.options[..options] {
                                    push_bounded_str16(&mut out, &option.id, MPRIS_STRING_MAX);
                                    push_bounded_str16(&mut out, &option.value, MPRIS_STRING_MAX);
                                }
                                push_bounded_str16(
                                    &mut out,
                                    &choice.initial_value,
                                    MPRIS_STRING_MAX,
                                );
                            }
                        }
                        PortalRequest::ScreenCast(request) => {
                            push_portal_common(
                                &mut out,
                                request.request_id,
                                1,
                                request.deadline_ms,
                                request.parent_surface_id,
                            );
                            push_bounded_str16(&mut out, &request.app_id, MPRIS_STRING_MAX);
                            out.push(u8::from(request.multiple));
                            let count = request.candidates.len().min(SCREENCAST_CANDIDATE_MAX);
                            out.push(count as u8);
                            let identity_bytes = request.candidates[..count]
                                .iter()
                                .map(|candidate| {
                                    2 + 2
                                        + 2
                                        + 2
                                        + bounded_utf8_len(&candidate.title, MPRIS_STRING_MAX)
                                        + 2
                                        + bounded_utf8_len(&candidate.app_id, MPRIS_STRING_MAX)
                                        + 4
                                })
                                .sum::<usize>();
                            let mut thumbnail_bytes = PORTAL_MESSAGE_MAX
                                .saturating_sub(out.len().saturating_add(identity_bytes));
                            for candidate in &request.candidates[..count] {
                                out.extend_from_slice(&candidate.surface_id.to_le_bytes());
                                out.extend_from_slice(&candidate.width.to_le_bytes());
                                out.extend_from_slice(&candidate.height.to_le_bytes());
                                push_bounded_str16(&mut out, &candidate.title, MPRIS_STRING_MAX);
                                push_bounded_str16(&mut out, &candidate.app_id, MPRIS_STRING_MAX);
                                let thumbnail = if candidate.thumbnail_png.len()
                                    <= SCREENCAST_THUMBNAIL_MAX
                                    && candidate.thumbnail_png.len() <= thumbnail_bytes
                                {
                                    candidate.thumbnail_png.as_slice()
                                } else {
                                    &[]
                                };
                                push_bytes32(&mut out, thumbnail);
                                thumbnail_bytes = thumbnail_bytes.saturating_sub(thumbnail.len());
                            }
                        }
                    }
                }
                ServerControl::PortalCancel(value) => {
                    out.push(5);
                    out.extend_from_slice(&value.request_id.to_le_bytes());
                    out.push(value.reason);
                }
                ServerControl::MprisActionResult(value) => {
                    out.push(7);
                    out.extend_from_slice(&value.nonce.to_le_bytes());
                    out.push(value.status);
                    out.extend_from_slice(&value.player_id.to_le_bytes());
                    out.extend_from_slice(&value.revision.to_le_bytes());
                }
                ServerControl::MprisUpdate { .. } => unreachable!(),
            }
            out
        }
    }
}

pub fn parse_server_control(msg: &[u8]) -> Result<Option<ServerControl>, &'static str> {
    if msg.len() < 2 || msg[0] != S2C_MEDIA_CONTROL {
        return Err("not a media control message");
    }
    if msg[1] == 6 {
        let (flags, records) = parse_mpris_update(msg)?;
        return Ok(Some(ServerControl::MprisUpdate { flags, records }));
    }
    let mut input = &msg[2..];
    let value = match msg[1] {
        0 => {
            let runtime_flags = take_u8(&mut input)?;
            let active_flags = take_u8(&mut input)?;
            if runtime_flags & !RUNTIME_FLAGS_ALL != 0 || active_flags & !ACTIVE_FLAGS_ALL != 0 {
                return Err("unknown media state bits");
            }
            let microphone_owner = take_u64(&mut input)?;
            let camera_owner = take_u64(&mut input)?;
            let count = take_u8(&mut input)? as usize;
            if count > SCREENCAST_STREAM_MAX {
                return Err("too many screencast sessions");
            }
            let mut screencasts = Vec::with_capacity(count);
            for _ in 0..count {
                let session_id = nonzero(take_u32(&mut input)?, "zero session id")?;
                if screencasts
                    .iter()
                    .any(|session: &ScreenCastState| session.session_id == session_id)
                {
                    return Err("duplicate screencast session");
                }
                let app_id = take_str16(&mut input, MPRIS_STRING_MAX)?;
                let surface_count = take_u8(&mut input)? as usize;
                if surface_count == 0 || surface_count > SCREENCAST_STREAM_MAX {
                    return Err("invalid screencast surface count");
                }
                let mut surface_ids = Vec::with_capacity(surface_count);
                for _ in 0..surface_count {
                    let id = take_u16(&mut input)?;
                    if id == 0 || surface_ids.contains(&id) {
                        return Err("invalid screencast surface");
                    }
                    surface_ids.push(id);
                }
                screencasts.push(ScreenCastState {
                    session_id,
                    app_id,
                    surface_ids,
                });
            }
            if (active_flags & ACTIVE_MICROPHONE != 0) != (microphone_owner != 0)
                || (active_flags & ACTIVE_CAMERA != 0) != (camera_owner != 0)
                || (active_flags & ACTIVE_SCREENCAST != 0) != !screencasts.is_empty()
            {
                return Err("inconsistent media state");
            }
            ServerControl::State(MediaState {
                runtime_flags,
                active_flags,
                microphone_owner,
                camera_owner,
                screencasts,
            })
        }
        1 => {
            if input.len() != 20 {
                return Err("malformed lease");
            }
            let nonce = nonzero(take_u32(&mut input)?, "zero nonce")?;
            let status = take_u8(&mut input)?;
            let kind = MediaKind::parse(take_u8(&mut input)?).ok_or("unknown media kind")?;
            let lease_id = take_u32(&mut input)?;
            let codec = take_u8(&mut input)?;
            let width = take_u16(&mut input)?;
            let height = take_u16(&mut input)?;
            let fps = take_u8(&mut input)?;
            let initial_credit = take_u32(&mut input)?;
            if (status == crate::STATUS_OK) != (lease_id != 0) {
                return Err("inconsistent lease result");
            }
            ServerControl::Lease(MediaLease {
                nonce,
                status,
                kind,
                lease_id,
                codec,
                width,
                height,
                fps,
                initial_credit,
            })
        }
        2 => {
            if input.len() != 5 {
                return Err("malformed revoke");
            }
            ServerControl::Revoked(MediaRevoked {
                lease_id: nonzero(take_u32(&mut input)?, "zero lease id")?,
                reason: RevokeReason::parse(take_u8(&mut input)?).ok_or("unknown revoke reason")?,
            })
        }
        3 => {
            if input.len() != 9 {
                return Err("malformed credit");
            }
            let lease_id = nonzero(take_u32(&mut input)?, "zero lease id")?;
            let bytes = take_u32(&mut input)?;
            let flags = take_u8(&mut input)?;
            if flags & !MEDIA_CREDIT_KEYFRAME != 0 {
                return Err("unknown credit flags");
            }
            ServerControl::Credit(MediaCredit {
                lease_id,
                bytes,
                flags,
            })
        }
        4 => {
            if msg.len() > PORTAL_MESSAGE_MAX {
                return Err("portal request is too large");
            }
            let (request_id, kind, deadline_ms, parent_surface_id) =
                take_portal_common(&mut input)?;
            let request = match kind {
                0 => {
                    let app_id = take_str16(&mut input, MPRIS_STRING_MAX)?;
                    let title = take_str16(&mut input, MPRIS_STRING_MAX)?;
                    let subtitle = take_str16(&mut input, MPRIS_STRING_MAX)?;
                    let body = take_str32(&mut input, PORTAL_PROMPT_MAX)?;
                    let deny_label = take_str16(&mut input, MPRIS_STRING_MAX)?;
                    let grant_label = take_str16(&mut input, MPRIS_STRING_MAX)?;
                    let icon_name = take_str16(&mut input, MPRIS_STRING_MAX)?;
                    let count = take_u8(&mut input)? as usize;
                    if count > PORTAL_CHOICE_MAX {
                        return Err("too many portal choices");
                    }
                    let mut choices = Vec::with_capacity(count);
                    for _ in 0..count {
                        let id = take_str16(&mut input, MPRIS_STRING_MAX)?;
                        let label = take_str16(&mut input, MPRIS_STRING_MAX)?;
                        let option_count = take_u8(&mut input)? as usize;
                        if option_count > PORTAL_CHOICE_OPTION_MAX {
                            return Err("too many portal choice options");
                        }
                        let mut options = Vec::with_capacity(option_count);
                        for _ in 0..option_count {
                            options.push(PortalChoiceValue {
                                id: take_str16(&mut input, MPRIS_STRING_MAX)?,
                                value: take_str16(&mut input, MPRIS_STRING_MAX)?,
                            });
                        }
                        choices.push(PortalChoice {
                            id,
                            label,
                            options,
                            initial_value: take_str16(&mut input, MPRIS_STRING_MAX)?,
                        });
                    }
                    PortalRequest::Access(PortalAccessRequest {
                        request_id,
                        deadline_ms,
                        parent_surface_id,
                        app_id,
                        title,
                        subtitle,
                        body,
                        deny_label,
                        grant_label,
                        icon_name,
                        choices,
                    })
                }
                1 => {
                    let app_id = take_str16(&mut input, MPRIS_STRING_MAX)?;
                    let multiple = take_bool(&mut input)?;
                    let count = take_u8(&mut input)? as usize;
                    if count > SCREENCAST_CANDIDATE_MAX {
                        return Err("too many screencast candidates");
                    }
                    let mut candidates = Vec::with_capacity(count);
                    for _ in 0..count {
                        let surface_id = nonzero(take_u16(&mut input)?, "zero surface id")?;
                        if candidates.iter().any(|candidate: &ScreenCastCandidate| {
                            candidate.surface_id == surface_id
                        }) {
                            return Err("duplicate screencast candidate");
                        }
                        let width = take_u16(&mut input)?;
                        let height = take_u16(&mut input)?;
                        let title = take_str16(&mut input, MPRIS_STRING_MAX)?;
                        let app_id = take_str16(&mut input, MPRIS_STRING_MAX)?;
                        let thumbnail_png = take_bytes32(&mut input, SCREENCAST_THUMBNAIL_MAX)?;
                        candidates.push(ScreenCastCandidate {
                            surface_id,
                            width,
                            height,
                            title,
                            app_id,
                            thumbnail_png,
                        });
                    }
                    PortalRequest::ScreenCast(PortalScreenCastRequest {
                        request_id,
                        deadline_ms,
                        parent_surface_id,
                        app_id,
                        multiple,
                        candidates,
                    })
                }
                _ => return Err("unknown portal request kind"),
            };
            ServerControl::PortalRequest(request)
        }
        5 => {
            if input.len() != 5 {
                return Err("malformed portal cancellation");
            }
            ServerControl::PortalCancel(PortalCancel {
                request_id: nonzero(take_u32(&mut input)?, "zero request id")?,
                reason: take_u8(&mut input)?,
            })
        }
        7 => {
            if input.len() != 13 {
                return Err("malformed MPRIS result");
            }
            ServerControl::MprisActionResult(MprisActionResult {
                nonce: nonzero(take_u32(&mut input)?, "zero nonce")?,
                status: take_u8(&mut input)?,
                player_id: nonzero(take_u32(&mut input)?, "zero player id")?,
                revision: take_u32(&mut input)?,
            })
        }
        8 => {
            if input.len() != 1 {
                return Err("malformed server media capabilities");
            }
            let video_codecs = take_u8(&mut input)?;
            if video_codecs & !VIDEO_CODECS_ALL != 0 || video_codecs & VIDEO_CODEC_MJPEG == 0 {
                return Err("invalid server video codecs");
            }
            ServerControl::ServerCapabilities(ServerMediaCapabilities { video_codecs })
        }
        _ => return Ok(None),
    };
    if !input.is_empty() {
        return Err("trailing media control bytes");
    }
    Ok(Some(value))
}

fn msg_mpris_update(flags: u8, records: &[MprisRecord]) -> Vec<u8> {
    let mut raw = Vec::new();
    let count = records.len().min(MPRIS_PLAYER_MAX);
    raw.push(count as u8);
    for record in &records[..count] {
        match record {
            MprisRecord::Delete { player_id } => {
                raw.push(0);
                raw.extend_from_slice(&player_id.to_le_bytes());
            }
            MprisRecord::Upsert(player) => {
                raw.push(1);
                raw.extend_from_slice(&player.player_id.to_le_bytes());
                raw.extend_from_slice(&player.revision.to_le_bytes());
                raw.extend_from_slice(&player.track_revision.to_le_bytes());
                raw.extend_from_slice(&[
                    u8::from(player.active),
                    player.playback_status as u8,
                    player.loop_status as u8,
                    u8::from(player.shuffle),
                ]);
                raw.extend_from_slice(
                    &(player.capability_flags & MPRIS_CAPABILITIES_ALL).to_le_bytes(),
                );
                raw.extend_from_slice(&player.rate_ppm.to_le_bytes());
                raw.extend_from_slice(&player.minimum_rate_ppm.to_le_bytes());
                raw.extend_from_slice(&player.maximum_rate_ppm.to_le_bytes());
                raw.extend_from_slice(&player.volume_ppm.to_le_bytes());
                raw.extend_from_slice(&player.position_us.to_le_bytes());
                raw.extend_from_slice(&player.length_us.to_le_bytes());
                push_bounded_str16(&mut raw, &player.identity, MPRIS_STRING_MAX);
                push_bounded_str16(&mut raw, &player.desktop_entry, MPRIS_STRING_MAX);
                push_bounded_str16(&mut raw, &player.title, MPRIS_STRING_MAX);
                push_bounded_str16(&mut raw, &player.album, MPRIS_STRING_MAX);
                let artists = player.artists.len().min(MPRIS_ARTIST_MAX);
                raw.push(artists as u8);
                for artist in &player.artists[..artists] {
                    push_bounded_str16(&mut raw, artist, MPRIS_STRING_MAX);
                }
                let artwork =
                    &player.artwork_png[..player.artwork_png.len().min(MPRIS_ARTWORK_MAX)];
                let (width, height) = if artwork.is_empty() {
                    (0, 0)
                } else {
                    (player.artwork_width, player.artwork_height)
                };
                raw.extend_from_slice(&width.to_le_bytes());
                raw.extend_from_slice(&height.to_le_bytes());
                push_bytes32(&mut raw, artwork);
            }
        }
    }
    let compressed = compress_prepend_size(&raw);
    let mut out = Vec::with_capacity(2 + compressed.len());
    out.extend_from_slice(&[S2C_MEDIA_CONTROL, 6, flags & MPRIS_UPDATE_FLAGS_ALL]);
    out.extend_from_slice(&compressed);
    out
}

fn parse_mpris_update(msg: &[u8]) -> Result<(u8, Vec<MprisRecord>), &'static str> {
    if msg.len() < 8 || msg[..2] != [S2C_MEDIA_CONTROL, 6] {
        return Err("malformed MPRIS update");
    }
    let flags = msg[2];
    if flags & !MPRIS_UPDATE_FLAGS_ALL != 0 {
        return Err("unknown MPRIS update flags");
    }
    let declared =
        u32::from_le_bytes(msg[3..7].try_into().map_err(|_| "truncated LZ4 size")?) as usize;
    if declared > MPRIS_UPDATE_MAX_DECOMPRESSED {
        return Err("MPRIS update too large");
    }
    let raw = decompress_size_prepended(&msg[3..]).map_err(|_| "invalid MPRIS LZ4")?;
    if raw.len() != declared {
        return Err("MPRIS size mismatch");
    }
    let mut input = raw.as_slice();
    let count = take_u8(&mut input)? as usize;
    if count > MPRIS_PLAYER_MAX {
        return Err("too many MPRIS records");
    }
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let op = take_u8(&mut input)?;
        let player_id = nonzero(take_u32(&mut input)?, "zero player id")?;
        match op {
            0 => records.push(MprisRecord::Delete { player_id }),
            1 => {
                let revision = take_u32(&mut input)?;
                let track_revision = take_u32(&mut input)?;
                let active = take_bool(&mut input)?;
                let playback_status =
                    PlaybackStatus::parse(take_u8(&mut input)?).ok_or("unknown playback status")?;
                let loop_status =
                    LoopStatus::parse(take_u8(&mut input)?).ok_or("unknown loop status")?;
                let shuffle = take_bool(&mut input)?;
                let capability_flags = take_u16(&mut input)?;
                if capability_flags & !MPRIS_CAPABILITIES_ALL != 0 {
                    return Err("unknown MPRIS capability");
                }
                let rate_ppm = take_i32(&mut input)?;
                let minimum_rate_ppm = take_i32(&mut input)?;
                let maximum_rate_ppm = take_i32(&mut input)?;
                let volume_ppm = take_u32(&mut input)?;
                let position_us = take_i64(&mut input)?;
                let length_us = take_i64(&mut input)?;
                let identity = take_str16(&mut input, MPRIS_STRING_MAX)?;
                let desktop_entry = take_str16(&mut input, MPRIS_STRING_MAX)?;
                let title = take_str16(&mut input, MPRIS_STRING_MAX)?;
                let album = take_str16(&mut input, MPRIS_STRING_MAX)?;
                let artist_count = take_u8(&mut input)? as usize;
                if artist_count > MPRIS_ARTIST_MAX {
                    return Err("too many artists");
                }
                let mut artists = Vec::with_capacity(artist_count);
                for _ in 0..artist_count {
                    artists.push(take_str16(&mut input, MPRIS_STRING_MAX)?);
                }
                let artwork_width = take_u16(&mut input)?;
                let artwork_height = take_u16(&mut input)?;
                let artwork_png = take_bytes32(&mut input, MPRIS_ARTWORK_MAX)?;
                if !artwork_fields_consistent(&artwork_png, artwork_width, artwork_height) {
                    return Err("inconsistent artwork");
                }
                records.push(MprisRecord::Upsert(MprisPlayer {
                    player_id,
                    revision,
                    track_revision,
                    active,
                    playback_status,
                    loop_status,
                    shuffle,
                    capability_flags,
                    rate_ppm,
                    minimum_rate_ppm,
                    maximum_rate_ppm,
                    volume_ppm,
                    position_us,
                    length_us,
                    identity,
                    desktop_entry,
                    title,
                    album,
                    artists,
                    artwork_width,
                    artwork_height,
                    artwork_png,
                }));
            }
            _ => return Err("unknown MPRIS record operation"),
        }
    }
    if !input.is_empty() {
        return Err("trailing MPRIS bytes");
    }
    Ok((flags, records))
}

fn artwork_fields_consistent(png: &[u8], width: u16, height: u16) -> bool {
    if png.is_empty() {
        width == 0 && height == 0
    } else {
        width != 0 && height != 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaData {
    pub lease_id: u32,
    pub sequence: u32,
    pub capture_us: u64,
    pub kind: MediaKind,
    pub codec: u8,
    pub flags: u8,
    pub fragment_index: u16,
    pub fragment_count: u16,
    pub frame_len: u32,
    pub data: Vec<u8>,
}

pub fn msg_media_data(value: &MediaData) -> Result<Vec<u8>, &'static str> {
    validate_media_data(value)?;
    let mut out = Vec::with_capacity(28 + value.data.len());
    out.push(C2S_MEDIA_DATA);
    out.extend_from_slice(&value.lease_id.to_le_bytes());
    out.extend_from_slice(&value.sequence.to_le_bytes());
    out.extend_from_slice(&value.capture_us.to_le_bytes());
    out.extend_from_slice(&[value.kind as u8, value.codec, value.flags]);
    out.extend_from_slice(&value.fragment_index.to_le_bytes());
    out.extend_from_slice(&value.fragment_count.to_le_bytes());
    out.extend_from_slice(&value.frame_len.to_le_bytes());
    out.extend_from_slice(&value.data);
    Ok(out)
}

pub fn parse_media_data(msg: &[u8]) -> Result<MediaData, &'static str> {
    if msg.len() < 28 || msg[0] != C2S_MEDIA_DATA {
        return Err("malformed media data");
    }
    // Reject oversized transport payloads before copying attacker-controlled
    // bytes into an owned allocation. The remaining semantic checks run on a
    // fragment already bounded to the protocol ceiling.
    if msg.len() - 28 > MEDIA_FRAGMENT_MAX {
        return Err("media frame too large");
    }
    let mut input = &msg[1..];
    let value = MediaData {
        lease_id: take_u32(&mut input)?,
        sequence: take_u32(&mut input)?,
        capture_us: take_u64(&mut input)?,
        kind: MediaKind::parse(take_u8(&mut input)?).ok_or("unknown media kind")?,
        codec: take_u8(&mut input)?,
        flags: take_u8(&mut input)?,
        fragment_index: take_u16(&mut input)?,
        fragment_count: take_u16(&mut input)?,
        frame_len: take_u32(&mut input)?,
        data: input.to_vec(),
    };
    validate_media_data(&value)?;
    Ok(value)
}

fn validate_media_data(value: &MediaData) -> Result<(), &'static str> {
    if value.lease_id == 0 {
        return Err("zero lease id");
    }
    if value.flags & !MEDIA_DATA_FLAGS_ALL != 0 {
        return Err("unknown media data flags");
    }
    if value.fragment_count == 0
        || value.fragment_count > MEDIA_FRAGMENT_COUNT_MAX
        || value.fragment_index >= value.fragment_count
    {
        return Err("invalid fragmentation");
    }
    if value.data.len() > MEDIA_FRAGMENT_MAX
        || value.frame_len as usize > value.kind.frame_max()
        || value.data.len() > value.frame_len as usize
        || (value.fragment_count == 1 && value.data.len() != value.frame_len as usize)
    {
        return Err("media frame too large");
    }
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MprisMirror {
    pub players: BTreeMap<u32, MprisPlayer>,
    staging: Option<BTreeMap<u32, MprisPlayer>>,
}

impl MprisMirror {
    pub fn apply(&mut self, msg: &[u8]) -> Result<u8, &'static str> {
        let (flags, records) = parse_mpris_update(msg)?;
        if flags & MPRIS_UPDATE_RESET != 0 {
            self.staging = Some(BTreeMap::new());
        }
        let target = self.staging.as_mut().unwrap_or(&mut self.players);
        for record in records {
            match record {
                MprisRecord::Delete { player_id } => {
                    target.remove(&player_id);
                }
                MprisRecord::Upsert(player) => {
                    target.insert(player.player_id, player);
                }
            }
        }
        if flags & MPRIS_UPDATE_SYNC != 0
            && let Some(staging) = self.staging.take()
        {
            self.players = staging;
        }
        Ok(flags)
    }

    pub fn reset(&mut self) {
        self.players.clear();
        self.staging = None;
    }
}

fn push_portal_common(
    out: &mut Vec<u8>,
    request_id: u32,
    kind: u8,
    deadline_ms: u32,
    parent: Option<u16>,
) {
    out.extend_from_slice(&request_id.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&deadline_ms.to_le_bytes());
    out.extend_from_slice(&parent.unwrap_or(0).to_le_bytes());
}

fn take_portal_common(input: &mut &[u8]) -> Result<(u32, u8, u32, Option<u16>), &'static str> {
    let request_id = nonzero(take_u32(input)?, "zero request id")?;
    let kind = take_u8(input)?;
    let deadline_ms = take_u32(input)?;
    let parent = take_u16(input)?;
    Ok((
        request_id,
        kind,
        deadline_ms,
        (parent != 0).then_some(parent),
    ))
}

fn push_str16(out: &mut Vec<u8>, value: &str) {
    push_bounded_str16(out, value, u16::MAX as usize);
}

fn push_bounded_str16(out: &mut Vec<u8>, value: &str, max: usize) {
    let end = bounded_utf8_len(value, max.min(u16::MAX as usize));
    out.extend_from_slice(&(end as u16).to_le_bytes());
    out.extend_from_slice(&value.as_bytes()[..end]);
}

fn bounded_utf8_len(value: &str, max: usize) -> usize {
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn push_bounded_str32(out: &mut Vec<u8>, value: &str, max: usize) {
    let end = bounded_utf8_len(value, max.min(u32::MAX as usize));
    out.extend_from_slice(&(end as u32).to_le_bytes());
    out.extend_from_slice(&value.as_bytes()[..end]);
}

fn push_bytes32(out: &mut Vec<u8>, value: &[u8]) {
    let len = value.len().min(u32::MAX as usize);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&value[..len]);
}

fn take<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], &'static str> {
    if input.len() < N {
        return Err("truncated field");
    }
    let value = input[..N].try_into().map_err(|_| "truncated field")?;
    *input = &input[N..];
    Ok(value)
}

fn take_u8(input: &mut &[u8]) -> Result<u8, &'static str> {
    Ok(take::<1>(input)?[0])
}
fn take_bool(input: &mut &[u8]) -> Result<bool, &'static str> {
    match take_u8(input)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err("invalid boolean"),
    }
}
fn take_u16(input: &mut &[u8]) -> Result<u16, &'static str> {
    Ok(u16::from_le_bytes(take(input)?))
}
fn take_u32(input: &mut &[u8]) -> Result<u32, &'static str> {
    Ok(u32::from_le_bytes(take(input)?))
}
fn take_i32(input: &mut &[u8]) -> Result<i32, &'static str> {
    Ok(i32::from_le_bytes(take(input)?))
}
fn take_u64(input: &mut &[u8]) -> Result<u64, &'static str> {
    Ok(u64::from_le_bytes(take(input)?))
}
fn take_i64(input: &mut &[u8]) -> Result<i64, &'static str> {
    Ok(i64::from_le_bytes(take(input)?))
}

fn take_str16(input: &mut &[u8], max: usize) -> Result<String, &'static str> {
    let len = take_u16(input)? as usize;
    if len > max || input.len() < len {
        return Err("invalid string length");
    }
    let value = std::str::from_utf8(&input[..len])
        .map_err(|_| "invalid UTF-8")?
        .to_string();
    *input = &input[len..];
    Ok(value)
}

fn take_str32(input: &mut &[u8], max: usize) -> Result<String, &'static str> {
    let len = take_u32(input)? as usize;
    if len > max || input.len() < len {
        return Err("invalid string length");
    }
    let value = std::str::from_utf8(&input[..len])
        .map_err(|_| "invalid UTF-8")?
        .to_string();
    *input = &input[len..];
    Ok(value)
}

fn take_bytes32(input: &mut &[u8], max: usize) -> Result<Vec<u8>, &'static str> {
    let len = take_u32(input)? as usize;
    if len > max || input.len() < len {
        return Err("invalid byte length");
    }
    let value = input[..len].to_vec();
    *input = &input[len..];
    Ok(value)
}

fn nonzero<T: Default + PartialEq>(value: T, error: &'static str) -> Result<T, &'static str> {
    if value == T::default() {
        Err(error)
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(id: u32) -> MprisPlayer {
        MprisPlayer {
            player_id: id,
            revision: 9,
            track_revision: 3,
            active: true,
            playback_status: PlaybackStatus::Playing,
            loop_status: LoopStatus::Track,
            shuffle: true,
            capability_flags: MPRIS_CAPABILITIES_ALL,
            rate_ppm: 1_000_000,
            minimum_rate_ppm: 500_000,
            maximum_rate_ppm: 2_000_000,
            volume_ppm: 750_000,
            position_us: 3_000_000,
            length_us: 20_000_000,
            identity: "Player".into(),
            desktop_entry: "player".into(),
            title: "Track".into(),
            album: "Album".into(),
            artists: vec!["Artist".into()],
            artwork_width: 2,
            artwork_height: 3,
            artwork_png: vec![1, 2, 3],
        }
    }

    #[test]
    fn client_controls_roundtrip_and_unknown_is_ignored() {
        let controls = vec![
            ClientControl::Capabilities(MediaCapabilities {
                flags: CAPTURE_FLAGS_ALL,
                audio_codecs: AUDIO_CODECS_ALL,
                video_codecs: VIDEO_CODECS_ALL,
                max_width: 1920,
                max_height: 1080,
                max_fps: 30,
            }),
            ClientControl::Start(MediaStart {
                nonce: 7,
                kind: MediaKind::Camera,
                codec: 1,
                width: 1280,
                height: 720,
                fps: 30,
            }),
            ClientControl::Stop { lease_id: 4 },
            ClientControl::PortalReply(PortalReply {
                request_id: 8,
                decision: PortalDecision::Grant,
                surface_ids: vec![2, 3],
                choices: vec![],
            }),
            ClientControl::ScreenCastStop { session_id: 5 },
            ClientControl::MprisSubscribe { enabled: true },
            ClientControl::MprisAction(MprisAction {
                nonce: 9,
                player_id: 2,
                kind: MprisActionKind::SetPosition,
                track_revision: 3,
                value: 12,
            }),
        ];
        for value in controls {
            assert_eq!(
                parse_client_control(&msg_client_control(&value)),
                Ok(Some(value))
            );
        }
        assert_eq!(parse_client_control(&[C2S_MEDIA_CONTROL, 250, 1]), Ok(None));
        assert!(parse_client_control(&[C2S_MEDIA_CONTROL, 5, 2]).is_err());
    }

    #[test]
    fn server_controls_roundtrip() {
        let controls = vec![
            ServerControl::ServerCapabilities(ServerMediaCapabilities {
                video_codecs: VIDEO_CODECS_ALL,
            }),
            ServerControl::State(MediaState {
                runtime_flags: RUNTIME_FLAGS_ALL,
                active_flags: ACTIVE_MICROPHONE | ACTIVE_SCREENCAST,
                microphone_owner: 7,
                camera_owner: 0,
                screencasts: vec![ScreenCastState {
                    session_id: 4,
                    app_id: "meet".into(),
                    surface_ids: vec![3],
                }],
            }),
            ServerControl::Lease(MediaLease {
                nonce: 1,
                status: crate::STATUS_OK,
                kind: MediaKind::Microphone,
                lease_id: 2,
                codec: 0,
                width: 0,
                height: 0,
                fps: 0,
                initial_credit: 19_200,
            }),
            ServerControl::Revoked(MediaRevoked {
                lease_id: 2,
                reason: RevokeReason::Stopped,
            }),
            ServerControl::Credit(MediaCredit {
                lease_id: 2,
                bytes: 100,
                flags: MEDIA_CREDIT_KEYFRAME,
            }),
            ServerControl::PortalRequest(PortalRequest::Access(PortalAccessRequest {
                request_id: 3,
                deadline_ms: 1000,
                parent_surface_id: Some(4),
                app_id: "app".into(),
                title: "Permission".into(),
                subtitle: String::new(),
                body: "Allow?".into(),
                deny_label: "Deny".into(),
                grant_label: "Allow".into(),
                icon_name: "app".into(),
                choices: vec![PortalChoice {
                    id: "remember".into(),
                    label: "Remember".into(),
                    options: vec![PortalChoiceValue {
                        id: "yes".into(),
                        value: "Yes".into(),
                    }],
                    initial_value: "yes".into(),
                }],
            })),
            ServerControl::PortalRequest(PortalRequest::ScreenCast(PortalScreenCastRequest {
                request_id: 4,
                deadline_ms: 2000,
                parent_surface_id: None,
                app_id: "meet".into(),
                multiple: true,
                candidates: vec![ScreenCastCandidate {
                    surface_id: 5,
                    width: 800,
                    height: 600,
                    title: "Window".into(),
                    app_id: "browser".into(),
                    thumbnail_png: vec![1, 2],
                }],
            })),
            ServerControl::PortalCancel(PortalCancel {
                request_id: 3,
                reason: 1,
            }),
            ServerControl::MprisUpdate {
                flags: MPRIS_UPDATE_RESET | MPRIS_UPDATE_SYNC,
                records: vec![MprisRecord::Upsert(player(7))],
            },
            ServerControl::MprisActionResult(MprisActionResult {
                nonce: 7,
                status: crate::STATUS_OK,
                player_id: 7,
                revision: 10,
            }),
        ];
        for value in controls {
            assert_eq!(
                parse_server_control(&msg_server_control(&value)),
                Ok(Some(value))
            );
        }
    }

    #[test]
    fn server_capabilities_reject_reserved_bits_and_require_mjpeg() {
        assert_eq!(
            parse_server_control(&[S2C_MEDIA_CONTROL, 8, VIDEO_CODEC_MJPEG]),
            Ok(Some(ServerControl::ServerCapabilities(
                ServerMediaCapabilities {
                    video_codecs: VIDEO_CODEC_MJPEG,
                },
            )))
        );
        assert!(parse_server_control(&[S2C_MEDIA_CONTROL, 8, 0]).is_err());
        assert!(parse_server_control(&[S2C_MEDIA_CONTROL, 8, 0x80]).is_err());
        assert!(parse_server_control(&[S2C_MEDIA_CONTROL, 8]).is_err());
    }

    #[test]
    fn portal_request_wire_starts_with_request_id_then_kind() {
        let message = msg_server_control(&ServerControl::PortalRequest(PortalRequest::Access(
            PortalAccessRequest {
                request_id: 0x4433_2211,
                deadline_ms: 0x8877_6655,
                parent_surface_id: Some(0xaa99),
                app_id: String::new(),
                title: String::new(),
                subtitle: String::new(),
                body: String::new(),
                deny_label: String::new(),
                grant_label: String::new(),
                icon_name: String::new(),
                choices: Vec::new(),
            },
        )));
        assert_eq!(
            &message[..13],
            &[
                S2C_MEDIA_CONTROL,
                4,
                0x11,
                0x22,
                0x33,
                0x44,
                0,
                0x55,
                0x66,
                0x77,
                0x88,
                0x99,
                0xaa,
            ]
        );
    }

    #[test]
    fn server_controls_reject_zero_and_duplicate_ids() {
        let duplicate_sessions = msg_server_control(&ServerControl::State(MediaState {
            runtime_flags: RUNTIME_FLAGS_ALL,
            active_flags: ACTIVE_SCREENCAST,
            microphone_owner: 0,
            camera_owner: 0,
            screencasts: vec![
                ScreenCastState {
                    session_id: 4,
                    app_id: "first".into(),
                    surface_ids: vec![1],
                },
                ScreenCastState {
                    session_id: 4,
                    app_id: "second".into(),
                    surface_ids: vec![2],
                },
            ],
        }));
        assert!(parse_server_control(&duplicate_sessions).is_err());

        let duplicate_candidates = msg_server_control(&ServerControl::PortalRequest(
            PortalRequest::ScreenCast(PortalScreenCastRequest {
                request_id: 1,
                deadline_ms: 1,
                parent_surface_id: None,
                app_id: "app".into(),
                multiple: true,
                candidates: vec![
                    ScreenCastCandidate {
                        surface_id: 7,
                        width: 1,
                        height: 1,
                        title: "first".into(),
                        app_id: "app".into(),
                        thumbnail_png: Vec::new(),
                    },
                    ScreenCastCandidate {
                        surface_id: 7,
                        width: 1,
                        height: 1,
                        title: "second".into(),
                        app_id: "app".into(),
                        thumbnail_png: Vec::new(),
                    },
                ],
            }),
        ));
        assert!(parse_server_control(&duplicate_candidates).is_err());

        let zero_player =
            msg_server_control(&ServerControl::MprisActionResult(MprisActionResult {
                nonce: 1,
                status: crate::STATUS_OK,
                player_id: 0,
                revision: 1,
            }));
        assert!(parse_server_control(&zero_player).is_err());
    }

    #[test]
    fn screencast_request_omits_excess_thumbnails_before_four_mib() {
        let thumbnail = vec![0x80; SCREENCAST_THUMBNAIL_MAX];
        let candidates = (1..=SCREENCAST_CANDIDATE_MAX)
            .map(|id| ScreenCastCandidate {
                surface_id: id as u16,
                width: 1920,
                height: 1080,
                title: "t".repeat(MPRIS_STRING_MAX),
                app_id: "a".repeat(MPRIS_STRING_MAX),
                thumbnail_png: thumbnail.clone(),
            })
            .collect();
        let message = msg_server_control(&ServerControl::PortalRequest(PortalRequest::ScreenCast(
            PortalScreenCastRequest {
                request_id: 1,
                deadline_ms: 1,
                parent_surface_id: None,
                app_id: "app".into(),
                multiple: true,
                candidates,
            },
        )));
        assert!(message.len() <= PORTAL_MESSAGE_MAX);
        let Some(ServerControl::PortalRequest(PortalRequest::ScreenCast(parsed))) =
            parse_server_control(&message).unwrap()
        else {
            panic!("expected ScreenCast request");
        };
        assert_eq!(parsed.candidates.len(), SCREENCAST_CANDIDATE_MAX);
        assert!(
            parsed
                .candidates
                .iter()
                .any(|candidate| candidate.thumbnail_png.is_empty())
        );
    }

    #[test]
    fn media_data_roundtrip_and_bounds() {
        let value = MediaData {
            lease_id: 1,
            sequence: u32::MAX,
            capture_us: 123,
            kind: MediaKind::Microphone,
            codec: 0,
            flags: MEDIA_DATA_DISCONTINUITY,
            fragment_index: 0,
            fragment_count: 1,
            frame_len: 3,
            data: vec![1, 2, 3],
        };
        assert_eq!(
            parse_media_data(&msg_media_data(&value).unwrap()),
            Ok(value)
        );

        let mut oversized = vec![C2S_MEDIA_DATA];
        oversized.extend_from_slice(&1u32.to_le_bytes());
        oversized.extend_from_slice(&0u32.to_le_bytes());
        oversized.extend_from_slice(&0u64.to_le_bytes());
        oversized.extend_from_slice(&[MediaKind::Microphone as u8, 0, 0]);
        oversized.extend_from_slice(&0u16.to_le_bytes());
        oversized.extend_from_slice(&1u16.to_le_bytes());
        oversized.extend_from_slice(&((MICROPHONE_FRAME_MAX + 1) as u32).to_le_bytes());
        assert!(parse_media_data(&oversized).is_err());
    }

    #[test]
    fn mpris_mirror_stages_snapshot() {
        let mut mirror = MprisMirror::default();
        mirror.players.insert(99, player(99));
        let first = msg_server_control(&ServerControl::MprisUpdate {
            flags: MPRIS_UPDATE_RESET | MPRIS_UPDATE_REPLAY,
            records: vec![MprisRecord::Upsert(player(1))],
        });
        mirror.apply(&first).unwrap();
        assert!(mirror.players.contains_key(&99));
        let second = msg_server_control(&ServerControl::MprisUpdate {
            flags: MPRIS_UPDATE_SYNC | MPRIS_UPDATE_REPLAY,
            records: vec![MprisRecord::Upsert(player(2))],
        });
        mirror.apply(&second).unwrap();
        assert_eq!(
            mirror.players.keys().copied().collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn declared_mpris_size_is_checked_before_decompression() {
        let mut msg = vec![S2C_MEDIA_CONTROL, 6, 0];
        msg.extend_from_slice(&((MPRIS_UPDATE_MAX_DECOMPRESSED + 1) as u32).to_le_bytes());
        msg.push(0);
        assert!(parse_server_control(&msg).is_err());
    }

    #[test]
    fn mpris_artwork_requires_two_nonzero_dimensions() {
        assert!(artwork_fields_consistent(&[], 0, 0));
        assert!(!artwork_fields_consistent(&[], 0, 3));
        assert!(!artwork_fields_consistent(&[], 2, 0));
        for (artwork_width, artwork_height) in [(0, 0), (0, 3), (2, 0)] {
            let mut malformed = player(7);
            malformed.artwork_width = artwork_width;
            malformed.artwork_height = artwork_height;
            let msg = msg_server_control(&ServerControl::MprisUpdate {
                flags: 0,
                records: vec![MprisRecord::Upsert(malformed)],
            });
            assert_eq!(parse_server_control(&msg), Err("inconsistent artwork"));
        }
    }
}
