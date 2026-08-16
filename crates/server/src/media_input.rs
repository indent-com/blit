//! Connection-bound viewer media leases and bounded fragment reassembly.

#![cfg(target_os = "linux")]

use crate::audio_pw::{PcmSource, RawVideoSource};
use blit_remote::media::{
    AUDIO_CODEC_OPUS, AUDIO_CODEC_PCM, CAMERA_CODEC_AV1, CAMERA_CODEC_AV1_444, CAMERA_CODEC_H264,
    CAMERA_CODEC_H264_444, CAMERA_CODEC_MJPEG, CAPTURE_CAMERA, CAPTURE_MICROPHONE,
    MEDIA_CREDIT_KEYFRAME, MEDIA_DATA_DISCONTINUITY, MEDIA_DATA_END_OF_STREAM, MEDIA_DATA_KEYFRAME,
    MediaCapabilities, MediaCredit, MediaData, MediaKind, MediaLease, MediaRevoked, MediaStart,
    RevokeReason,
};
use blit_remote::{STATUS_CONFLICT, STATUS_INVALID, STATUS_OK, STATUS_OTHER, STATUS_PERMISSION};

use crate::media_policy::MediaCodecPolicy;
use std::collections::VecDeque;
use std::io::Cursor;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const PCM_FRAME_BYTES: usize = 960 * 2;
const INITIAL_PCM_CREDIT: u32 = (PCM_FRAME_BYTES * 10) as u32;
const INITIAL_OPUS_CREDIT: u32 = 40_000;

/// How much camera video the link may hold in flight, as time.
///
/// Credit comes back only once a frame has been *decoded*, so this window is
/// exactly how far behind the picture is allowed to fall: the viewer keeps
/// encoding until the window is full, and everything it sent is still in
/// front of the frame it is about to send.  A quarter second is enough to
/// ride out a decode hiccup without the delay becoming visible.
const CAMERA_WINDOW: Duration = Duration::from_millis(250);

/// The smallest window worth granting, whatever the cadence works out to.
///
/// A keyframe is an order of magnitude larger than an inter frame, and a
/// window that cannot hold one deadlocks the stream outright: the viewer
/// parks on `cameraRequiredCredit` waiting for room only a smaller frame
/// could ever free, and no smaller frame is coming because the next one it
/// owes is a keyframe.
const CAMERA_CREDIT_FLOOR: u32 = 512 * 1024;

/// The largest, for a lease whose declared cadence is implausible.
const CAMERA_CREDIT_CEILING: u32 = 4 * 1024 * 1024;

/// The in-flight byte window granted to a camera lease of this shape.
///
/// This used to be a flat 8 MiB, described as two maximum-size frames. Real
/// frames are nothing like maximum size — a 720p30 H.264 frame is tens of
/// KiB — so the window was really hundreds of frames deep, and on an uplink
/// slower than the encoder the viewer happily filled all of it before credit
/// ran out. Every one of those bytes sits in front of the picture the camera
/// is showing now, so the stream ran tens of seconds behind and stayed there.
///
/// A window is a latency, not a quantity: the negotiated cadence says what a
/// second of this video costs, so [`CAMERA_WINDOW`] converts straight into
/// bytes. The floor is the part that cannot be argued down — the window has
/// to hold one whole keyframe, or the viewer parks forever on credit that
/// only a smaller frame could release, and the next frame it owes is a
/// keyframe. That floor is why this alone cannot pin the delay to
/// `CAMERA_WINDOW`; it bounds the damage, and the viewer's own transport
/// backpressure does the fine work.
fn camera_credit_window(codec: CameraCodec, width: u16, height: u16, fps: u8) -> u32 {
    let pixels = f64::from(width) * f64::from(height);
    let per_second = pixels * f64::from(fps) * codec.bits_per_pixel() / 8.0;
    let rate_window = per_second * CAMERA_WINDOW.as_secs_f64();
    let keyframe_room = pixels * codec.keyframe_bits_per_pixel() / 8.0;
    let window = rate_window.max(keyframe_room);
    // A non-finite or negative product means the lease described something
    // nonsensical; the floor is a safe answer for it.
    if !window.is_finite() || window <= 0.0 {
        return CAMERA_CREDIT_FLOOR;
    }
    (window.min(f64::from(CAMERA_CREDIT_CEILING)) as u32)
        .clamp(CAMERA_CREDIT_FLOOR, CAMERA_CREDIT_CEILING)
}
const LEASE_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard ceiling on inbound camera cadence, whatever the operator configures.
/// The wire carries fps in a `u8`; past 120 a viewer is asking the decode
/// workers for more frames per second than the compositor will composite.
const CAMERA_FPS_CEILING: u8 = 120;
const MAX_CAMERA_DECODE_WORKERS: usize = 2;

static ACTIVE_CAMERA_DECODE_WORKERS: AtomicUsize = AtomicUsize::new(0);

struct CameraWorkerPermit;

impl CameraWorkerPermit {
    fn acquire() -> Option<Self> {
        ACTIVE_CAMERA_DECODE_WORKERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_CAMERA_DECODE_WORKERS).then_some(active + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for CameraWorkerPermit {
    fn drop(&mut self) {
        ACTIVE_CAMERA_DECODE_WORKERS.fetch_sub(1, Ordering::Release);
    }
}

struct Reassembly {
    sequence: u32,
    capture_us: u64,
    flags: u8,
    fragment_count: u16,
    next_fragment: u16,
    frame_len: u32,
    data: Vec<u8>,
}

impl Reassembly {
    fn from_first_fragment(frame: &MediaData) -> Self {
        // Grow with bytes that have actually spent lease credit. Reserving the
        // declared complete-frame length here lets an empty fragment claim a
        // 4 MiB camera frame, abandon it with a new sequence, and repeat
        // without consuming any credit.
        Self {
            sequence: frame.sequence,
            capture_us: frame.capture_us,
            flags: frame.flags,
            fragment_count: frame.fragment_count,
            next_fragment: 0,
            frame_len: frame.frame_len,
            data: Vec::with_capacity(frame.data.len()),
        }
    }
}

fn empty_non_eos_fragment(frame: &MediaData) -> bool {
    frame.data.is_empty() && frame.flags & MEDIA_DATA_END_OF_STREAM == 0
}

struct MicrophoneLease {
    lease_id: u32,
    owner: u64,
    credit: u32,
    credit_pending: u32,
    last_data: Instant,
    last_complete: Option<u32>,
    last_capture_us: Option<u64>,
    reassembly: Option<Reassembly>,
    codec: u8,
    opus: Option<opus::Decoder>,
    source: PcmSource,
}

struct CameraLease {
    lease_id: u32,
    owner: u64,
    credit: u32,
    credit_pending: u32,
    last_data: Instant,
    last_complete: Option<u32>,
    reassembly: Option<Reassembly>,
    malformed_frames: u8,
    codec: CameraCodec,
    needs_keyframe: bool,
    worker: CameraWorker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CameraCodec {
    Mjpeg,
    H264Cs420,
    Av1Cs420,
    H264Cs444,
    Av1Cs444,
}

impl CameraCodec {
    fn from_wire(value: u8) -> Option<Self> {
        match value {
            CAMERA_CODEC_MJPEG => Some(Self::Mjpeg),
            CAMERA_CODEC_H264 => Some(Self::H264Cs420),
            CAMERA_CODEC_AV1 => Some(Self::Av1Cs420),
            CAMERA_CODEC_H264_444 => Some(Self::H264Cs444),
            CAMERA_CODEC_AV1_444 => Some(Self::Av1Cs444),
            _ => None,
        }
    }

    fn wire(self) -> u8 {
        match self {
            Self::Mjpeg => CAMERA_CODEC_MJPEG,
            Self::H264Cs420 => CAMERA_CODEC_H264,
            Self::Av1Cs420 => CAMERA_CODEC_AV1,
            Self::H264Cs444 => CAMERA_CODEC_H264_444,
            Self::Av1Cs444 => CAMERA_CODEC_AV1_444,
        }
    }

    fn capability(self) -> u8 {
        1 << self.wire()
    }

    fn interframe(self) -> bool {
        self != Self::Mjpeg
    }

    /// Bits per pixel a steady frame of this codec is asked to cost.
    ///
    /// These mirror the viewer's own `cameraEncoderConfig`, which derives its
    /// bitrate the same way — keep the two in step, because the whole point
    /// of the number here is to predict what the viewer is about to send.
    /// Motion JPEG configures no bitrate at all (every picture is a whole
    /// intra frame), so it is given the cost of one.
    fn bits_per_pixel(self) -> f64 {
        match self {
            Self::Mjpeg => 1.2,
            Self::H264Cs420 => 0.11,
            Self::H264Cs444 => 0.16,
            Self::Av1Cs420 => 0.075,
            Self::Av1Cs444 => 0.11,
        }
    }

    /// Bits per pixel to leave room for in a single keyframe.
    ///
    /// An interframe codec spends an order of magnitude more on the pictures
    /// it cannot predict, and the window has to hold one whole: see
    /// [`camera_credit_window`] for what happens when it does not.
    fn keyframe_bits_per_pixel(self) -> f64 {
        match self {
            // Every Motion JPEG frame already is one.
            Self::Mjpeg => 1.2,
            Self::H264Cs420 | Self::Av1Cs420 => 1.5,
            Self::H264Cs444 | Self::Av1Cs444 => 2.5,
        }
    }

    fn decoder_profile(
        self,
    ) -> Option<(crate::video_decode::VideoCodec, crate::video_decode::Chroma)> {
        use crate::video_decode::{Chroma, VideoCodec};
        match self {
            Self::Mjpeg => None,
            Self::H264Cs420 => Some((VideoCodec::H264, Chroma::Cs420)),
            Self::Av1Cs420 => Some((VideoCodec::Av1, Chroma::Cs420)),
            Self::H264Cs444 => Some((VideoCodec::H264, Chroma::Cs444)),
            Self::Av1Cs444 => Some((VideoCodec::Av1, Chroma::Cs444)),
        }
    }
}

struct CameraDecodeJob {
    encoded: Vec<u8>,
    credit_bytes: u32,
    keyframe: bool,
    reset_decoder: bool,
}

enum CameraDecodeResult {
    Decoded {
        credit_bytes: u32,
        keyframe: bool,
    },
    BackendReset {
        credit_bytes: u32,
    },
    Malformed {
        credit_bytes: u32,
        request_keyframe: bool,
    },
    SourceFailed,
}

#[derive(Default)]
struct CameraDecodeQueue {
    jobs: VecDeque<CameraDecodeJob>,
    stopped: bool,
    awaiting_keyframe: bool,
}

struct CameraWorker {
    queue: Arc<(Mutex<CameraDecodeQueue>, Condvar)>,
    results: mpsc::Receiver<CameraDecodeResult>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CameraEnqueueResult {
    returned_credit: u32,
    request_keyframe: bool,
}

fn drain_camera_jobs(queue: &mut VecDeque<CameraDecodeJob>) -> u32 {
    queue
        .drain(..)
        .fold(0u32, |total, job| total.saturating_add(job.credit_bytes))
}

fn push_camera_job(
    queue: &mut VecDeque<CameraDecodeJob>,
    mut job: CameraDecodeJob,
    codec: CameraCodec,
) -> CameraEnqueueResult {
    if !codec.interframe() {
        let returned_credit = if queue.len() >= 2 {
            queue.pop_front().map_or(0, |job| job.credit_bytes)
        } else {
            0
        };
        queue.push_back(job);
        return CameraEnqueueResult {
            returned_credit,
            request_keyframe: false,
        };
    }

    if queue.len() < 2 {
        queue.push_back(job);
        return CameraEnqueueResult::default();
    }

    // Inter frames depend on everything before them. Once pressure forces a
    // drop, retaining later deltas would feed a corrupt reference chain to the
    // decoder. Drop the whole pending chain and resume only from a keyframe.
    let mut returned_credit = drain_camera_jobs(queue);
    if job.keyframe {
        job.reset_decoder = true;
        queue.push_back(job);
        CameraEnqueueResult {
            returned_credit,
            request_keyframe: false,
        }
    } else {
        returned_credit = returned_credit.saturating_add(job.credit_bytes);
        CameraEnqueueResult {
            returned_credit,
            request_keyframe: true,
        }
    }
}

fn enqueue_camera_job(
    state: &mut CameraDecodeQueue,
    mut job: CameraDecodeJob,
    codec: CameraCodec,
) -> CameraEnqueueResult {
    if codec.interframe() && state.awaiting_keyframe {
        if !job.keyframe {
            return CameraEnqueueResult {
                returned_credit: job.credit_bytes,
                request_keyframe: true,
            };
        }
        // Permit dependent packets to queue behind this recovery point.  If
        // it fails, the worker reinstates the barrier and drains them.
        state.awaiting_keyframe = false;
        job.reset_decoder = true;
    }
    push_camera_job(&mut state.jobs, job, codec)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CameraFrameDecodeError {
    Invalid,
    /// The native decoder has already advanced past a failed hardware backend.
    /// Keep it so the requested keyframe starts on that next backend.
    HardwareReset,
}

fn decode_camera_frame(
    decoder: &mut Option<crate::video_decode::Decoder>,
    codec: CameraCodec,
    width: u16,
    height: u16,
    job: &CameraDecodeJob,
) -> Result<Option<Vec<u8>>, CameraFrameDecodeError> {
    if codec == CameraCodec::Mjpeg {
        let dimensions =
            image::ImageReader::with_format(Cursor::new(&job.encoded), image::ImageFormat::Jpeg)
                .into_dimensions()
                .map_err(|_| CameraFrameDecodeError::Invalid)?;
        if dimensions != (u32::from(width), u32::from(height)) {
            return Err(CameraFrameDecodeError::Invalid);
        }
        return image::ImageReader::with_format(
            Cursor::new(&job.encoded),
            image::ImageFormat::Jpeg,
        )
        .decode()
        .map(|image| Some(image.into_rgba8().into_raw()))
        .map_err(|_| CameraFrameDecodeError::Invalid);
    }

    let (family, chroma) = codec
        .decoder_profile()
        .ok_or(CameraFrameDecodeError::Invalid)?;
    if job.keyframe {
        crate::video_decode::preflight_keyframe(family, chroma, &job.encoded, width, height)
            .map_err(|_| CameraFrameDecodeError::Invalid)?;
    }
    if job.reset_decoder
        && let Some(decoder) = decoder.as_mut()
    {
        decoder.flush();
    }
    if decoder.is_none() {
        if !job.keyframe {
            return Err(CameraFrameDecodeError::Invalid);
        }
        *decoder = Some(
            crate::video_decode::Decoder::new(family, chroma, width, height)
                .map_err(|_| CameraFrameDecodeError::Invalid)?,
        );
    }
    let decoded = match decoder
        .as_mut()
        .ok_or(CameraFrameDecodeError::Invalid)?
        .decode(&job.encoded, job.keyframe)
    {
        Ok(decoded) => decoded,
        Err(crate::video_decode::DecodeError::HardwareReset(_)) => {
            return Err(CameraFrameDecodeError::HardwareReset);
        }
        Err(_) => return Err(CameraFrameDecodeError::Invalid),
    };
    // Each camera packet is one complete low-latency access/temporal unit and
    // therefore one frame. A header-only, buffered, or silently dropped unit
    // is a decode failure; accepting `None` would reset the malformed counter
    // and return credit while the camera source stops advancing.
    if decoded.is_none() {
        return Err(CameraFrameDecodeError::Invalid);
    }
    Ok(decoded)
}

impl CameraWorker {
    fn start(
        source: RawVideoSource,
        codec: CameraCodec,
        width: u16,
        height: u16,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self, String> {
        // Lease teardown intentionally does not join a decoder that may still
        // be inside a codec library. Keep those detached tails process-wide
        // bounded so rapid stop/start cycles cannot accumulate workers.
        let permit = CameraWorkerPermit::acquire()
            .ok_or_else(|| "too many camera decoder workers are still active".to_owned())?;
        let queue = Arc::new((Mutex::new(CameraDecodeQueue::default()), Condvar::new()));
        let worker_queue = queue.clone();
        // The input queue is bounded, and so is the completion side.  A
        // stalled server thread may delay credit, but cannot accumulate an
        // unbounded stream of tiny completion records.
        let (result_tx, results) = mpsc::sync_channel(4);
        let thread = std::thread::Builder::new()
            .name("blit-camera-decode".into())
            .spawn(move || {
                let _permit = permit;
                let mut decoder = None;
                loop {
                    let job = {
                        let (lock, ready) = &*worker_queue;
                        let mut state = match lock.lock() {
                            Ok(state) => state,
                            Err(_) => return,
                        };
                        while state.jobs.is_empty() && !state.stopped {
                            state = match ready.wait(state) {
                                Ok(state) => state,
                                Err(_) => return,
                            };
                        }
                        if state.stopped {
                            return;
                        }
                        state.jobs.pop_front().expect("queue checked above")
                    };
                    let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        decode_camera_frame(&mut decoder, codec, width, height, &job)
                    }))
                    .unwrap_or(Err(CameraFrameDecodeError::Invalid));
                    let result = match decoded {
                        Ok(rgba) => {
                            if rgba.is_none_or(|rgba| source.push(rgba).is_ok()) {
                                CameraDecodeResult::Decoded {
                                    credit_bytes: job.credit_bytes,
                                    keyframe: job.keyframe,
                                }
                            } else {
                                CameraDecodeResult::SourceFailed
                            }
                        }
                        Err(error) => {
                            let mut credit_bytes = job.credit_bytes;
                            if codec.interframe() {
                                if error != CameraFrameDecodeError::HardwareReset
                                    && let Some(decoder) = decoder.as_mut()
                                {
                                    // Retain the decoder's per-lease backend
                                    // cursor.  Flushing is sufficient to make
                                    // the next keyframe establish fresh codec
                                    // state; reconstructing here would retry
                                    // hardware backends already rejected by a
                                    // valid recovery packet.
                                    decoder.flush();
                                }
                                let (lock, _) = &*worker_queue;
                                let mut state = match lock.lock() {
                                    Ok(state) => state,
                                    Err(_) => return,
                                };
                                state.awaiting_keyframe = true;
                                credit_bytes =
                                    credit_bytes.saturating_add(drain_camera_jobs(&mut state.jobs));
                            }
                            if error == CameraFrameDecodeError::HardwareReset {
                                CameraDecodeResult::BackendReset { credit_bytes }
                            } else {
                                CameraDecodeResult::Malformed {
                                    credit_bytes,
                                    request_keyframe: codec.interframe(),
                                }
                            }
                        }
                    };
                    if result_tx.send(result).is_err() {
                        return;
                    }
                    if let Some(notify) = notify.as_ref() {
                        notify();
                    }
                }
            })
            .map_err(|error| format!("failed to start camera decoder: {error}"))?;
        Ok(Self {
            queue,
            results,
            thread: Some(thread),
        })
    }

    /// Queue one complete frame. At most two wait behind the decoder. JPEG
    /// keeps the newest two; inter-frame codecs drop the dependency chain and
    /// establish a keyframe barrier when pressure forces a gap.
    fn enqueue(&self, job: CameraDecodeJob, codec: CameraCodec) -> Result<CameraEnqueueResult, ()> {
        let (lock, ready) = &*self.queue;
        let mut state = lock.lock().map_err(|_| ())?;
        if state.stopped {
            return Err(());
        }
        let result = enqueue_camera_job(&mut state, job, codec);
        if !state.jobs.is_empty() {
            ready.notify_one();
        }
        Ok(result)
    }

    fn discard_pending(&self) -> Result<u32, ()> {
        let (lock, _) = &*self.queue;
        let mut state = lock.lock().map_err(|_| ())?;
        Ok(drain_camera_jobs(&mut state.jobs))
    }
}

impl Drop for CameraWorker {
    fn drop(&mut self) {
        let (lock, ready) = &*self.queue;
        if let Ok(mut state) = lock.lock() {
            state.stopped = true;
            state.jobs.clear();
            ready.notify_one();
        }
        // A malformed JPEG must not make lease teardown block the compositor
        // session lock while an image decoder finishes. Dropping the handle
        // detaches the worker; it observes `stopped` after the current job and
        // then releases its PipeWire source.
        let _ = self.thread.take();
    }
}

#[derive(Default)]
pub struct MediaInput {
    next_lease_id: u32,
    microphone: Option<MicrophoneLease>,
    camera: Option<CameraLease>,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
}

pub enum DataResult {
    Ignored,
    Credit { owner: u64, credit: MediaCredit },
    Revoked { owner: u64, revoked: MediaRevoked },
}

/// Camera formats this host can decode *and* is allowed to accept.
///
/// `allowed` is the operator's [`MediaCodecPolicy::camera`] mask. Motion JPEG
/// is unconditional: the policy parser always keeps it, and a
/// `ServerCapabilities` message without it is rejected by every client.
pub fn camera_codec_mask(allowed: u8) -> u8 {
    let mut codecs = CameraCodec::Mjpeg.capability();
    for codec in [
        CameraCodec::H264Cs420,
        CameraCodec::Av1Cs420,
        CameraCodec::H264Cs444,
        CameraCodec::Av1Cs444,
    ] {
        if allowed & codec.capability() != 0
            && let Some((family, chroma)) = codec.decoder_profile()
            && crate::video_decode::available(family, chroma)
        {
            codecs |= codec.capability();
        }
    }
    codecs
}

impl MediaInput {
    pub fn with_notify(notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            notify: Some(notify),
            ..Self::default()
        }
    }

    pub fn microphone_owner(&self) -> u64 {
        self.microphone.as_ref().map_or(0, |lease| lease.owner)
    }

    pub fn camera_owner(&self) -> u64 {
        self.camera.as_ref().map_or(0, |lease| lease.owner)
    }

    pub fn start(
        &mut self,
        owner: u64,
        capabilities: MediaCapabilities,
        request: MediaStart,
        policy: MediaCodecPolicy,
        runtime_enabled: bool,
        runtime_dir: Option<&Path>,
    ) -> MediaLease {
        let rejected = |status| MediaLease {
            nonce: request.nonce,
            status,
            kind: request.kind,
            lease_id: 0,
            codec: request.codec,
            width: request.width,
            height: request.height,
            fps: request.fps,
            initial_credit: 0,
        };
        // Client capabilities describe what the browser can provide.  They
        // must never override the operator's compositor-wide device gates.
        if !runtime_enabled {
            return rejected(STATUS_PERMISSION);
        }
        let Some(runtime_dir) = runtime_dir else {
            return rejected(STATUS_OTHER);
        };
        self.next_lease_id = self.next_lease_id.wrapping_add(1);
        if self.next_lease_id == 0 {
            self.next_lease_id = 1;
        }
        let lease_id = self.next_lease_id;
        match request.kind {
            MediaKind::Microphone => {
                if capabilities.flags & CAPTURE_MICROPHONE == 0 {
                    return rejected(STATUS_PERMISSION);
                }
                // The client's mask says what it can produce; the policy says
                // what this server accepts.  Both have to hold.
                let offered = capabilities.audio_codecs & policy.microphone;
                if (request.codec == 0 && offered & AUDIO_CODEC_PCM == 0)
                    || (request.codec == 1 && offered & AUDIO_CODEC_OPUS == 0)
                    || request.codec > 1
                {
                    return rejected(STATUS_INVALID);
                }
                if self.microphone.is_some() {
                    return rejected(STATUS_CONFLICT);
                }
                let source = match PcmSource::start(runtime_dir) {
                    Ok(source) => source,
                    Err(_) => return rejected(STATUS_OTHER),
                };
                let opus = if request.codec == 1 {
                    match opus::Decoder::new(48_000, opus::Channels::Mono) {
                        Ok(decoder) => Some(decoder),
                        Err(_) => return rejected(STATUS_OTHER),
                    }
                } else {
                    None
                };
                let initial_credit = if request.codec == 1 {
                    INITIAL_OPUS_CREDIT
                } else {
                    INITIAL_PCM_CREDIT
                };
                self.microphone = Some(MicrophoneLease {
                    lease_id,
                    owner,
                    credit: initial_credit,
                    credit_pending: 0,
                    last_data: Instant::now(),
                    last_complete: None,
                    last_capture_us: None,
                    reassembly: None,
                    codec: request.codec,
                    opus,
                    source,
                });
                MediaLease {
                    nonce: request.nonce,
                    status: STATUS_OK,
                    kind: request.kind,
                    lease_id,
                    codec: request.codec,
                    width: 0,
                    height: 0,
                    fps: 0,
                    initial_credit,
                }
            }
            MediaKind::Camera => {
                if capabilities.flags & CAPTURE_CAMERA == 0 {
                    return rejected(STATUS_PERMISSION);
                }
                let Some(codec) = CameraCodec::from_wire(request.codec) else {
                    return rejected(STATUS_INVALID);
                };
                if capabilities.video_codecs & codec.capability() == 0
                    || camera_codec_mask(policy.camera) & codec.capability() == 0
                {
                    return rejected(STATUS_INVALID);
                }
                if self.camera.is_some() {
                    return rejected(STATUS_CONFLICT);
                }
                let max_width = env_bound("BLIT_MEDIA_CAMERA_MAX_WIDTH", 1920).min(1920);
                let max_height = env_bound("BLIT_MEDIA_CAMERA_MAX_HEIGHT", 1080).min(1080);
                // Defaults, not laws — an operator raises either with
                // BLIT_MEDIA_CAMERA_MAX_FPS, up to CAMERA_FPS_CEILING.
                //
                // The compressed default has to cover every cadence the panel
                // offers. A ceiling below that is worse than no choice at all:
                // the viewer picks 60, the request is refused as invalid, and
                // the refusal names a number nothing in the UI mentioned.
                // Motion JPEG stays lower because it sends a whole intra frame
                // each time, but not so low that the panel's own options are
                // unreachable.
                let codec_default_fps = if codec == CameraCodec::Mjpeg { 30 } else { 60 };
                let max_fps = env_bound("BLIT_MEDIA_CAMERA_MAX_FPS", codec_default_fps)
                    .min(u16::from(CAMERA_FPS_CEILING));
                let requires_even_extent =
                    matches!(codec, CameraCodec::H264Cs420 | CameraCodec::Av1Cs420);
                if request.width == 0
                    || request.height == 0
                    || request.fps == 0
                    || (requires_even_extent
                        && (!request.width.is_multiple_of(2) || !request.height.is_multiple_of(2)))
                    || request.width > capabilities.max_width
                    || request.height > capabilities.max_height
                    || request.fps > capabilities.max_fps
                    || request.width > max_width
                    || request.height > max_height
                    || request.fps > max_fps as u8
                {
                    return rejected(STATUS_INVALID);
                }
                let source = match RawVideoSource::start_camera(
                    runtime_dir,
                    request.width,
                    request.height,
                    request.fps,
                ) {
                    Ok(source) => source,
                    Err(_) => return rejected(STATUS_OTHER),
                };
                let worker = match CameraWorker::start(
                    source,
                    codec,
                    request.width,
                    request.height,
                    self.notify.clone(),
                ) {
                    Ok(worker) => worker,
                    Err(_) => return rejected(STATUS_OTHER),
                };
                let credit_window =
                    camera_credit_window(codec, request.width, request.height, request.fps);
                self.camera = Some(CameraLease {
                    lease_id,
                    owner,
                    credit: credit_window,
                    credit_pending: 0,
                    last_data: Instant::now(),
                    last_complete: None,
                    reassembly: None,
                    malformed_frames: 0,
                    codec,
                    needs_keyframe: codec.interframe(),
                    worker,
                });
                MediaLease {
                    nonce: request.nonce,
                    status: STATUS_OK,
                    kind: request.kind,
                    lease_id,
                    codec: request.codec,
                    width: request.width,
                    height: request.height,
                    fps: request.fps,
                    initial_credit: credit_window,
                }
            }
        }
    }

    pub fn stop(
        &mut self,
        owner: u64,
        lease_id: u32,
        reason: RevokeReason,
    ) -> Option<MediaRevoked> {
        if self
            .microphone
            .as_ref()
            .is_some_and(|lease| lease.owner == owner && lease.lease_id == lease_id)
        {
            self.microphone.take();
            Some(MediaRevoked { lease_id, reason })
        } else if self
            .camera
            .as_ref()
            .is_some_and(|lease| lease.owner == owner && lease.lease_id == lease_id)
        {
            self.camera.take();
            Some(MediaRevoked { lease_id, reason })
        } else {
            None
        }
    }

    pub fn disconnect(&mut self, owner: u64) -> Vec<MediaRevoked> {
        let mut revoked = Vec::with_capacity(2);
        if self
            .microphone
            .as_ref()
            .is_some_and(|lease| lease.owner == owner)
            && let Some(lease) = self.microphone.take()
        {
            revoked.push(MediaRevoked {
                lease_id: lease.lease_id,
                reason: RevokeReason::Disconnected,
            });
        }
        if self
            .camera
            .as_ref()
            .is_some_and(|lease| lease.owner == owner)
            && let Some(lease) = self.camera.take()
        {
            revoked.push(MediaRevoked {
                lease_id: lease.lease_id,
                reason: RevokeReason::Disconnected,
            });
        }
        revoked
    }

    pub fn revoke_all(&mut self, reason: RevokeReason) -> Vec<(u64, MediaRevoked)> {
        let mut revoked = Vec::with_capacity(2);
        if let Some(lease) = self.microphone.take() {
            revoked.push((
                lease.owner,
                MediaRevoked {
                    lease_id: lease.lease_id,
                    reason,
                },
            ));
        }
        if let Some(lease) = self.camera.take() {
            revoked.push((
                lease.owner,
                MediaRevoked {
                    lease_id: lease.lease_id,
                    reason,
                },
            ));
        }
        revoked
    }

    pub fn expire(&mut self, now: Instant) -> Vec<(u64, MediaRevoked)> {
        let microphone_expired = self
            .microphone
            .as_ref()
            .is_some_and(|lease| now.duration_since(lease.last_data) >= LEASE_IDLE_TIMEOUT);
        let camera_expired = self
            .camera
            .as_ref()
            .is_some_and(|lease| now.duration_since(lease.last_data) >= LEASE_IDLE_TIMEOUT);
        let mut revoked = Vec::new();
        if microphone_expired && let Some(lease) = self.microphone.take() {
            revoked.push((
                lease.owner,
                MediaRevoked {
                    lease_id: lease.lease_id,
                    reason: RevokeReason::IdleTimeout,
                },
            ));
        }
        if camera_expired && let Some(lease) = self.camera.take() {
            revoked.push((
                lease.owner,
                MediaRevoked {
                    lease_id: lease.lease_id,
                    reason: RevokeReason::IdleTimeout,
                },
            ));
        }
        revoked
    }

    /// Drain camera decode completions without blocking the server/session
    /// thread. Credit is returned only once a frame leaves the bounded worker
    /// queue, so a sender cannot outrun decoder work indefinitely.
    pub fn poll(&mut self) -> Vec<DataResult> {
        let mut returned = 0u32;
        let mut source_failed = false;
        let mut revoke_malformed = false;
        let mut request_keyframe = false;
        let Some(lease) = self.camera.as_mut() else {
            return Vec::new();
        };
        while let Ok(result) = lease.worker.results.try_recv() {
            match result {
                CameraDecodeResult::Decoded {
                    credit_bytes,
                    keyframe,
                } => {
                    lease.malformed_frames = 0;
                    returned = returned.saturating_add(credit_bytes);
                    if keyframe {
                        lease.needs_keyframe = false;
                        request_keyframe = false;
                    }
                }
                CameraDecodeResult::Malformed {
                    credit_bytes,
                    request_keyframe: result_requests_keyframe,
                } => {
                    lease.malformed_frames = lease.malformed_frames.saturating_add(1);
                    returned = returned.saturating_add(credit_bytes);
                    if result_requests_keyframe {
                        lease.needs_keyframe = true;
                        request_keyframe = true;
                    }
                    revoke_malformed |= lease.malformed_frames >= 10;
                }
                CameraDecodeResult::BackendReset { credit_bytes } => {
                    returned = returned.saturating_add(credit_bytes);
                    lease.needs_keyframe = true;
                    request_keyframe = true;
                }
                CameraDecodeResult::SourceFailed => source_failed = true,
            }
        }
        if source_failed || revoke_malformed {
            let lease = self.camera.take().expect("borrowed above");
            return vec![DataResult::Revoked {
                owner: lease.owner,
                revoked: MediaRevoked {
                    lease_id: lease.lease_id,
                    reason: if source_failed {
                        RevokeReason::PipeWireFailed
                    } else {
                        RevokeReason::FormatError
                    },
                },
            }];
        }
        if returned == 0 {
            return Vec::new();
        }
        lease.credit = lease.credit.saturating_add(returned);
        vec![DataResult::Credit {
            owner: lease.owner,
            credit: MediaCredit {
                lease_id: lease.lease_id,
                bytes: returned,
                flags: if request_keyframe {
                    MEDIA_CREDIT_KEYFRAME
                } else {
                    0
                },
            },
        }]
    }

    pub fn data(&mut self, owner: u64, frame: MediaData) -> DataResult {
        if frame.kind == MediaKind::Camera {
            return self.camera_data(owner, frame);
        }
        let Some(lease) = self.microphone.as_mut() else {
            return DataResult::Ignored;
        };
        if lease.owner != owner
            || lease.lease_id != frame.lease_id
            || frame.kind != MediaKind::Microphone
            || frame.codec != lease.codec
        {
            return DataResult::Ignored;
        }
        if frame.flags & MEDIA_DATA_END_OF_STREAM != 0 {
            let lease_id = lease.lease_id;
            self.microphone.take();
            return DataResult::Revoked {
                owner,
                revoked: MediaRevoked {
                    lease_id,
                    reason: RevokeReason::DeviceEnded,
                },
            };
        }
        if empty_non_eos_fragment(&frame) {
            return self.revoke_format(owner);
        }
        if frame.data.len() as u32 > lease.credit {
            let lease_id = lease.lease_id;
            self.microphone.take();
            return DataResult::Revoked {
                owner,
                revoked: MediaRevoked {
                    lease_id,
                    reason: RevokeReason::CreditViolation,
                },
            };
        }
        if lease
            .last_complete
            .is_some_and(|last| !sequence_newer(frame.sequence, last))
        {
            return DataResult::Ignored;
        }

        let replace = lease
            .reassembly
            .as_ref()
            .is_none_or(|current| current.sequence != frame.sequence);
        if replace {
            if let Some(current) = lease.reassembly.as_ref()
                && !sequence_newer(frame.sequence, current.sequence)
            {
                return DataResult::Ignored;
            }
            if frame.fragment_index != 0 {
                return self.revoke_format(owner);
            }
            if let Some(abandoned) = lease.reassembly.take() {
                lease.credit_pending = lease
                    .credit_pending
                    .saturating_add(abandoned.data.len() as u32);
            }
            lease.reassembly = Some(Reassembly::from_first_fragment(&frame));
        }
        let current = lease.reassembly.as_mut().expect("created above");
        if current.capture_us != frame.capture_us
            || current.flags != frame.flags
            || current.fragment_count != frame.fragment_count
            || current.frame_len != frame.frame_len
            || current.next_fragment != frame.fragment_index
            || current.data.len() + frame.data.len() > current.frame_len as usize
        {
            return self.revoke_format(owner);
        }
        lease.credit -= frame.data.len() as u32;
        lease.last_data = Instant::now();
        current.data.extend_from_slice(&frame.data);
        current.next_fragment += 1;
        if current.next_fragment != current.fragment_count {
            return DataResult::Ignored;
        }
        let completed = lease.reassembly.take().expect("complete reassembly");
        if completed.data.len() != completed.frame_len as usize {
            return self.revoke_format(owner);
        }
        let missing = lease
            .last_capture_us
            .map(|previous| completed.capture_us.saturating_sub(previous))
            .map(|gap| (gap.saturating_add(10_000) / 20_000).saturating_sub(1))
            .unwrap_or(0)
            .min(3) as usize;
        let concealed = missing.max(usize::from(
            completed.flags & MEDIA_DATA_DISCONTINUITY != 0 && missing == 0,
        ));
        for _ in 0..concealed {
            let concealed_pcm = if let Some(decoder) = lease.opus.as_mut() {
                let mut samples = [0i16; 960];
                match decoder.decode(&[], &mut samples, false) {
                    Ok(960) => samples
                        .into_iter()
                        .flat_map(i16::to_le_bytes)
                        .collect::<Vec<_>>(),
                    _ => vec![0; PCM_FRAME_BYTES],
                }
            } else {
                vec![0; PCM_FRAME_BYTES]
            };
            if lease.source.push(concealed_pcm).is_err() {
                return self.revoke_format(owner);
            }
        }
        let pcm = if let Some(decoder) = lease.opus.as_mut() {
            let mut samples = [0i16; 960];
            match decoder.decode(&completed.data, &mut samples, false) {
                Ok(960) => samples
                    .into_iter()
                    .flat_map(i16::to_le_bytes)
                    .collect::<Vec<_>>(),
                _ => return self.revoke_format(owner),
            }
        } else if completed.data.len() == PCM_FRAME_BYTES {
            completed.data
        } else {
            return self.revoke_format(owner);
        };
        if lease.source.push(pcm).is_err() {
            return self.revoke_format(owner);
        }
        lease.last_complete = Some(completed.sequence);
        lease.last_capture_us = Some(completed.capture_us);
        let returned = completed.frame_len.saturating_add(lease.credit_pending);
        lease.credit_pending = 0;
        lease.credit = lease.credit.saturating_add(returned);
        DataResult::Credit {
            owner,
            credit: MediaCredit {
                lease_id: lease.lease_id,
                bytes: returned,
                flags: 0,
            },
        }
    }

    fn revoke_format(&mut self, owner: u64) -> DataResult {
        let Some(lease) = self.microphone.take() else {
            return DataResult::Ignored;
        };
        DataResult::Revoked {
            owner,
            revoked: MediaRevoked {
                lease_id: lease.lease_id,
                reason: RevokeReason::FormatError,
            },
        }
    }

    fn camera_data(&mut self, owner: u64, frame: MediaData) -> DataResult {
        let Some(lease) = self.camera.as_mut() else {
            return DataResult::Ignored;
        };
        if lease.owner != owner
            || lease.lease_id != frame.lease_id
            || frame.codec != lease.codec.wire()
        {
            return DataResult::Ignored;
        }
        if frame.flags & MEDIA_DATA_END_OF_STREAM != 0 {
            let lease_id = lease.lease_id;
            self.camera.take();
            return DataResult::Revoked {
                owner,
                revoked: MediaRevoked {
                    lease_id,
                    reason: RevokeReason::DeviceEnded,
                },
            };
        }
        if empty_non_eos_fragment(&frame) {
            return self.revoke_camera_format(owner);
        }
        if frame.data.len() as u32 > lease.credit {
            let lease_id = lease.lease_id;
            self.camera.take();
            return DataResult::Revoked {
                owner,
                revoked: MediaRevoked {
                    lease_id,
                    reason: RevokeReason::CreditViolation,
                },
            };
        }
        if lease
            .last_complete
            .is_some_and(|last| !sequence_newer(frame.sequence, last))
        {
            return DataResult::Ignored;
        }
        let replace = lease
            .reassembly
            .as_ref()
            .is_none_or(|value| value.sequence != frame.sequence);
        if replace {
            if let Some(current) = lease.reassembly.as_ref()
                && !sequence_newer(frame.sequence, current.sequence)
            {
                return DataResult::Ignored;
            }
            if frame.fragment_index != 0 {
                return self.revoke_camera_format(owner);
            }
            if let Some(abandoned) = lease.reassembly.take() {
                lease.credit_pending = lease
                    .credit_pending
                    .saturating_add(abandoned.data.len() as u32);
            }
            lease.reassembly = Some(Reassembly::from_first_fragment(&frame));
        }
        let current = lease.reassembly.as_mut().expect("created above");
        if current.capture_us != frame.capture_us
            || current.flags != frame.flags
            || current.fragment_count != frame.fragment_count
            || current.frame_len != frame.frame_len
            || current.next_fragment != frame.fragment_index
            || current.data.len() + frame.data.len() > current.frame_len as usize
        {
            return self.revoke_camera_format(owner);
        }
        lease.credit -= frame.data.len() as u32;
        lease.last_data = Instant::now();
        current.data.extend_from_slice(&frame.data);
        current.next_fragment += 1;
        if current.next_fragment != current.fragment_count {
            return DataResult::Ignored;
        }
        let completed = lease.reassembly.take().expect("complete reassembly");
        if completed.data.len() != completed.frame_len as usize {
            return self.revoke_camera_format(owner);
        }
        let credit_bytes = completed.frame_len.saturating_add(lease.credit_pending);
        lease.credit_pending = 0;
        let keyframe = completed.flags & MEDIA_DATA_KEYFRAME != 0;
        let discontinuity = completed.flags & MEDIA_DATA_DISCONTINUITY != 0;
        let mut returned_credit = 0u32;
        let mut request_keyframe = false;
        let mut reset_decoder = false;

        if lease.codec.interframe() && discontinuity {
            returned_credit = match lease.worker.discard_pending() {
                Ok(bytes) => bytes,
                Err(()) => return self.revoke_camera_format(owner),
            };
            lease.needs_keyframe = true;
        }
        if lease.codec.interframe() && lease.needs_keyframe {
            if !keyframe {
                returned_credit = returned_credit.saturating_add(credit_bytes);
                lease.last_complete = Some(completed.sequence);
                lease.credit = lease.credit.saturating_add(returned_credit);
                return DataResult::Credit {
                    owner,
                    credit: MediaCredit {
                        lease_id: lease.lease_id,
                        bytes: returned_credit,
                        flags: MEDIA_CREDIT_KEYFRAME,
                    },
                };
            }
            lease.needs_keyframe = false;
            reset_decoder = true;
        }

        let enqueue = match lease.worker.enqueue(
            CameraDecodeJob {
                encoded: completed.data,
                credit_bytes,
                keyframe,
                reset_decoder,
            },
            lease.codec,
        ) {
            Ok(result) => result,
            Err(()) => return self.revoke_camera_format(owner),
        };
        returned_credit = returned_credit.saturating_add(enqueue.returned_credit);
        if enqueue.request_keyframe {
            lease.needs_keyframe = true;
            request_keyframe = true;
        }
        lease.last_complete = Some(completed.sequence);
        if returned_credit == 0 {
            DataResult::Ignored
        } else {
            lease.credit = lease.credit.saturating_add(returned_credit);
            DataResult::Credit {
                owner,
                credit: MediaCredit {
                    lease_id: lease.lease_id,
                    bytes: returned_credit,
                    flags: if request_keyframe {
                        MEDIA_CREDIT_KEYFRAME
                    } else {
                        0
                    },
                },
            }
        }
    }

    fn revoke_camera_format(&mut self, owner: u64) -> DataResult {
        let Some(lease) = self.camera.take() else {
            return DataResult::Ignored;
        };
        DataResult::Revoked {
            owner,
            revoked: MediaRevoked {
                lease_id: lease.lease_id,
                reason: RevokeReason::FormatError,
            },
        }
    }
}

fn env_bound(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn sequence_newer(value: u32, previous: u32) -> bool {
    value != previous && value.wrapping_sub(previous) < (1 << 31)
}

#[cfg(test)]
mod tests {
    use super::{
        CameraCodec, CameraDecodeJob, CameraDecodeQueue, CameraEnqueueResult, CameraWorkerPermit,
        MediaCodecPolicy, MediaInput, Reassembly, camera_codec_mask, empty_non_eos_fragment,
        enqueue_camera_job, push_camera_job, sequence_newer,
    };
    use blit_remote::media::{
        AUDIO_CODEC_OPUS, AUDIO_CODEC_PCM, CAMERA_CODEC_AV1, CAMERA_FRAME_MAX, CAPTURE_CAMERA,
        CAPTURE_MICROPHONE, MEDIA_DATA_END_OF_STREAM, MediaCapabilities, MediaData, MediaKind,
        MediaStart, VIDEO_CODEC_MJPEG, VIDEO_CODECS_ALL,
    };
    use blit_remote::{STATUS_INVALID, STATUS_PERMISSION};
    use std::path::Path;

    #[test]
    fn modular_sequence_order_wraps() {
        assert!(sequence_newer(0, u32::MAX));
        assert!(sequence_newer(12, 11));
        assert!(!sequence_newer(11, 11));
        assert!(!sequence_newer(10, 11));
    }

    #[test]
    fn first_fragment_reserves_only_bytes_that_spent_credit() {
        let frame = MediaData {
            lease_id: 1,
            sequence: 2,
            capture_us: 3,
            kind: MediaKind::Camera,
            codec: 0,
            flags: 0,
            fragment_index: 0,
            fragment_count: 16,
            frame_len: CAMERA_FRAME_MAX as u32,
            data: vec![0xaa],
        };

        let reassembly = Reassembly::from_first_fragment(&frame);
        assert!(reassembly.data.capacity() < CAMERA_FRAME_MAX);
    }

    #[test]
    fn empty_payload_is_valid_only_for_end_of_stream() {
        let mut frame = MediaData {
            lease_id: 1,
            sequence: 2,
            capture_us: 3,
            kind: MediaKind::Microphone,
            codec: 0,
            flags: 0,
            fragment_index: 0,
            fragment_count: 1,
            frame_len: 0,
            data: Vec::new(),
        };

        assert!(empty_non_eos_fragment(&frame));
        frame.flags = MEDIA_DATA_END_OF_STREAM;
        assert!(!empty_non_eos_fragment(&frame));
    }

    #[test]
    fn operator_gate_rejects_an_advertised_device_before_runtime_lookup() {
        let mut input = MediaInput::default();
        let lease = input.start(
            7,
            MediaCapabilities {
                flags: CAPTURE_MICROPHONE,
                audio_codecs: AUDIO_CODEC_PCM,
                ..MediaCapabilities::default()
            },
            MediaStart {
                nonce: 11,
                kind: MediaKind::Microphone,
                codec: 0,
                width: 0,
                height: 0,
                fps: 0,
            },
            MediaCodecPolicy::default(),
            false,
            None,
        );
        assert_eq!(lease.status, STATUS_PERMISSION);
        assert_eq!(lease.lease_id, 0);
    }

    /// The operator's codec policy is checked before anything opens a device,
    /// so a disallowed format is refused even though the client advertised it
    /// and this host could decode it.
    ///
    /// Only the refusals are asserted. An accepted codec goes on to build a
    /// PipeWire source, and that mutates this process's `XDG_RUNTIME_DIR` —
    /// not something a unit test should do to its own environment.
    #[test]
    fn policy_rejects_codecs_the_operator_disabled() {
        let mut input = MediaInput::default();
        // Present, so the gate above the codec check passes; empty, so
        // nothing behind it could succeed either.
        let runtime_dir = Some(Path::new("/nonexistent/blit-media-policy-test"));

        let opus = input.start(
            7,
            MediaCapabilities {
                flags: CAPTURE_MICROPHONE,
                audio_codecs: AUDIO_CODEC_PCM | AUDIO_CODEC_OPUS,
                ..MediaCapabilities::default()
            },
            MediaStart {
                nonce: 11,
                kind: MediaKind::Microphone,
                codec: 1,
                width: 0,
                height: 0,
                fps: 0,
            },
            MediaCodecPolicy {
                microphone: AUDIO_CODEC_PCM,
                ..MediaCodecPolicy::default()
            },
            true,
            runtime_dir,
        );
        assert_eq!(opus.status, STATUS_INVALID);
        assert_eq!(opus.lease_id, 0);

        let av1 = input.start(
            7,
            MediaCapabilities {
                flags: CAPTURE_CAMERA,
                video_codecs: VIDEO_CODECS_ALL,
                max_width: 1280,
                max_height: 720,
                max_fps: 30,
                ..MediaCapabilities::default()
            },
            MediaStart {
                nonce: 12,
                kind: MediaKind::Camera,
                codec: CAMERA_CODEC_AV1,
                width: 1280,
                height: 720,
                fps: 30,
            },
            MediaCodecPolicy {
                camera: VIDEO_CODEC_MJPEG,
                ..MediaCodecPolicy::default()
            },
            true,
            runtime_dir,
        );
        assert_eq!(av1.status, STATUS_INVALID);
        assert_eq!(av1.lease_id, 0);
    }

    #[test]
    fn camera_codec_mask_intersects_policy_and_keeps_mjpeg() {
        let mjpeg = CameraCodec::Mjpeg.capability();
        // An empty policy still advertises Motion JPEG: clients reject a
        // capability message without it.
        assert_eq!(camera_codec_mask(0), mjpeg);
        // Nothing outside the policy can appear, whatever the host decodes.
        let h264_only = mjpeg | CameraCodec::H264Cs420.capability();
        assert_eq!(camera_codec_mask(h264_only) & !h264_only, 0);
        assert_eq!(camera_codec_mask(h264_only) & mjpeg, mjpeg);
        // Narrowing the policy can only remove formats, never add them —
        // the decoder probe still has the final say on everything but MJPEG.
        let all = camera_codec_mask(u8::MAX);
        assert_eq!(camera_codec_mask(h264_only) & !all, 0);
    }

    #[test]
    fn camera_decode_queue_keeps_the_two_newest_pending_frames() {
        let mut queue = std::collections::VecDeque::new();
        let job = |credit_bytes| CameraDecodeJob {
            encoded: Vec::new(),
            credit_bytes,
            keyframe: true,
            reset_decoder: false,
        };

        assert_eq!(
            push_camera_job(&mut queue, job(10), CameraCodec::Mjpeg),
            CameraEnqueueResult::default()
        );
        assert_eq!(
            push_camera_job(&mut queue, job(20), CameraCodec::Mjpeg),
            CameraEnqueueResult::default()
        );
        assert_eq!(
            push_camera_job(&mut queue, job(30), CameraCodec::Mjpeg),
            CameraEnqueueResult {
                returned_credit: 10,
                request_keyframe: false,
            }
        );
        assert_eq!(
            queue.iter().map(|job| job.credit_bytes).collect::<Vec<_>>(),
            vec![20, 30]
        );
    }

    #[test]
    fn interframe_queue_pressure_discards_dependency_chain() {
        let mut queue = std::collections::VecDeque::new();
        let job = |credit_bytes, keyframe| CameraDecodeJob {
            encoded: Vec::new(),
            credit_bytes,
            keyframe,
            reset_decoder: false,
        };

        assert_eq!(
            push_camera_job(&mut queue, job(10, false), CameraCodec::H264Cs420),
            CameraEnqueueResult::default()
        );
        assert_eq!(
            push_camera_job(&mut queue, job(20, false), CameraCodec::H264Cs420),
            CameraEnqueueResult::default()
        );
        assert_eq!(
            push_camera_job(&mut queue, job(30, false), CameraCodec::H264Cs420),
            CameraEnqueueResult {
                returned_credit: 60,
                request_keyframe: true,
            }
        );
        assert!(queue.is_empty());

        let _ = push_camera_job(&mut queue, job(40, false), CameraCodec::H264Cs420);
        let _ = push_camera_job(&mut queue, job(50, false), CameraCodec::H264Cs420);
        assert_eq!(
            push_camera_job(&mut queue, job(60, true), CameraCodec::H264Cs420),
            CameraEnqueueResult {
                returned_credit: 90,
                request_keyframe: false,
            }
        );
        assert_eq!(queue.len(), 1);
        assert!(queue[0].keyframe);
        assert!(queue[0].reset_decoder);
    }

    #[test]
    fn worker_keyframe_barrier_drops_deltas_and_preserves_recovery_order() {
        let mut state = CameraDecodeQueue {
            awaiting_keyframe: true,
            ..CameraDecodeQueue::default()
        };
        let job = |credit_bytes, keyframe| CameraDecodeJob {
            encoded: Vec::new(),
            credit_bytes,
            keyframe,
            reset_decoder: false,
        };

        assert_eq!(
            enqueue_camera_job(&mut state, job(10, false), CameraCodec::H264Cs420),
            CameraEnqueueResult {
                returned_credit: 10,
                request_keyframe: true,
            }
        );
        assert!(state.jobs.is_empty());
        assert!(state.awaiting_keyframe);

        assert_eq!(
            enqueue_camera_job(&mut state, job(20, true), CameraCodec::H264Cs420),
            CameraEnqueueResult::default()
        );
        assert!(!state.awaiting_keyframe);
        assert_eq!(state.jobs.len(), 1);
        assert!(state.jobs[0].keyframe);
        assert!(state.jobs[0].reset_decoder);
    }

    #[test]
    fn detached_camera_worker_tails_are_process_bounded() {
        let first = CameraWorkerPermit::acquire().expect("first worker permit");
        let second = CameraWorkerPermit::acquire().expect("detached tail allowance");
        assert!(CameraWorkerPermit::acquire().is_none());
        drop(first);
        let replacement = CameraWorkerPermit::acquire().expect("released worker permit");
        drop((second, replacement));
    }
}

#[cfg(test)]
mod credit_window_tests {
    use super::{
        CAMERA_CREDIT_CEILING, CAMERA_CREDIT_FLOOR, CAMERA_WINDOW, CameraCodec,
        camera_credit_window,
    };

    /// The window has to hold a whole keyframe. A viewer that cannot fit one
    /// sets `cameraRequiredCredit` to a frame length larger than any credit
    /// it can ever hold — credit is conserved, so it never grows past the
    /// window — and waits for room that only a smaller frame could free,
    /// while the frame it owes is a keyframe. The stream stops for good.
    #[test]
    fn every_window_holds_a_keyframe() {
        for codec in [
            CameraCodec::Mjpeg,
            CameraCodec::H264Cs420,
            CameraCodec::Av1Cs420,
            CameraCodec::H264Cs444,
            CameraCodec::Av1Cs444,
        ] {
            for (w, h, fps) in [(320, 240, 15), (1280, 720, 30), (1920, 1080, 60)] {
                let window = camera_credit_window(codec, w, h, fps);
                let keyframe =
                    (f64::from(w) * f64::from(h) * codec.keyframe_bits_per_pixel() / 8.0) as u32;
                assert!(
                    window >= keyframe.min(CAMERA_CREDIT_CEILING),
                    "{codec:?} {w}x{h}@{fps}: window {window} cannot hold a {keyframe}-byte keyframe",
                );
            }
        }
    }

    /// The whole point: a window is a delay. Where the cadence is rich enough
    /// that the rate term wins, the window must be worth about
    /// `CAMERA_WINDOW` of video and not more.
    #[test]
    fn a_rate_bound_window_is_worth_its_latency_target() {
        let (w, h, fps) = (1920u16, 1080u16, 60u8);
        let codec = CameraCodec::Mjpeg;
        let window = camera_credit_window(codec, w, h, fps);
        let per_second =
            f64::from(w) * f64::from(h) * f64::from(fps) * codec.bits_per_pixel() / 8.0;
        let seconds = f64::from(window) / per_second;
        assert!(
            seconds <= CAMERA_WINDOW.as_secs_f64() * 1.05,
            "window is worth {seconds:.3}s of video, target {:.3}s",
            CAMERA_WINDOW.as_secs_f64(),
        );
    }

    /// The old flat window was 8 MiB. Nothing may reach that again: it is
    /// tens of seconds of video on the links this exists to protect.
    #[test]
    fn no_window_approaches_the_old_flat_grant() {
        for codec in [CameraCodec::Mjpeg, CameraCodec::H264Cs420] {
            let window = camera_credit_window(codec, 1920, 1080, 120);
            assert!(window <= CAMERA_CREDIT_CEILING, "{codec:?}: {window}");
        }
    }

    #[test]
    fn a_nonsensical_lease_still_gets_a_usable_window() {
        assert_eq!(
            camera_credit_window(CameraCodec::H264Cs420, 0, 0, 0),
            CAMERA_CREDIT_FLOOR,
        );
    }
}
