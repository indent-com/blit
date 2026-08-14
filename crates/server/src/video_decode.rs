//! Stateful H.264/AV1 camera decoding through native GPU APIs with an exact-
//! format software fallback. No FFmpeg symbols or processes are involved.

#![cfg(target_os = "linux")]

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::sync::OnceLock;

const MAX_ENCODED_BYTES: usize = 4 * 1024 * 1024;
const MAX_UNITS_PER_PACKET: usize = 4096;
const DEFAULT_DECODER_ORDER: &str = "nvdec,vaapi,vulkan,software";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoCodec {
    H264,
    Av1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Chroma {
    Cs420,
    Cs444,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecoderBackend {
    Nvdec,
    Vaapi,
    Vulkan,
    Software,
    #[cfg(test)]
    FailsForTest,
    #[cfg(test)]
    FailsAfterRecoveryForTest,
}

impl DecoderBackend {
    fn label(self) -> &'static str {
        match self {
            Self::Nvdec => "nvdec",
            Self::Vaapi => "vaapi",
            Self::Vulkan => "vulkan",
            Self::Software => "software",
            #[cfg(test)]
            Self::FailsForTest => "failing-test-backend",
            #[cfg(test)]
            Self::FailsAfterRecoveryForTest => "failing-after-recovery-test-backend",
        }
    }

    fn hardware(self) -> bool {
        !matches!(self, Self::Software)
    }
}

fn configured_decoder_order() -> VecDeque<DecoderBackend> {
    let configured = std::env::var("BLIT_MEDIA_CAMERA_DECODERS")
        .unwrap_or_else(|_| DEFAULT_DECODER_ORDER.to_owned());
    parse_decoder_order(&configured)
}

fn parse_decoder_order(configured: &str) -> VecDeque<DecoderBackend> {
    let mut result = VecDeque::new();
    for token in configured.split([',', ':']) {
        let backend = match token.trim().to_ascii_lowercase().as_str() {
            "nvdec" | "cuda" => Some(DecoderBackend::Nvdec),
            "vaapi" => Some(DecoderBackend::Vaapi),
            "vulkan" => Some(DecoderBackend::Vulkan),
            "software" | "sw" => Some(DecoderBackend::Software),
            _ => None,
        };
        if let Some(backend) = backend
            && !result.contains(&backend)
        {
            result.push_back(backend);
        }
    }
    // Direct GPU decode is opportunistic. The codec-specific software path is
    // the exact-format final candidate even when an explicitly requested GPU
    // driver or profile is absent. Put software first to skip GPU probing.
    if !result.contains(&DecoderBackend::Software) {
        result.push_back(DecoderBackend::Software);
    }
    result
}

/// Error classes used by the camera lease to distinguish malformed peer data
/// from a hardware state loss which requires a new recovery point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecodeError {
    Unavailable(String),
    Malformed(String),
    UnsupportedOutput(String),
    Resource(String),
    /// The failing hardware backend has already been retired for this lease
    /// and the next configured backend is ready for a new keyframe.
    HardwareReset(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(f, "video decoder unavailable: {detail}"),
            Self::Malformed(detail) => write!(f, "malformed encoded video: {detail}"),
            Self::UnsupportedOutput(detail) => write!(f, "unsupported decoded video: {detail}"),
            Self::Resource(detail) => write!(f, "video decoder resource limit: {detail}"),
            Self::HardwareReset(detail) => write!(f, "hardware decoder reset: {detail}"),
        }
    }
}

impl Error for DecodeError {}

enum DecoderSession {
    Nvdec(Box<crate::nvdec_decode::NvdecDecoder>),
    Vaapi(Box<crate::vaapi_decode::Decoder>),
    Vulkan(Box<crate::video_decode_vulkan::Decoder>),
    Software(Box<crate::software_decode::Decoder>),
    #[cfg(test)]
    FailsForTest,
    #[cfg(test)]
    FailsAfterRecoveryForTest {
        recovered: bool,
    },
}

impl DecoderSession {
    fn new(
        backend: DecoderBackend,
        codec: VideoCodec,
        chroma: Chroma,
        width: u16,
        height: u16,
    ) -> Result<Self, DecodeError> {
        match backend {
            DecoderBackend::Nvdec => crate::nvdec_decode::NvdecDecoder::new(
                map_nvdec_codec(codec),
                map_nvdec_chroma(chroma),
                width,
                height,
            )
            .map(|decoder| Self::Nvdec(Box::new(decoder)))
            .map_err(map_nvdec_error),
            DecoderBackend::Vaapi => crate::vaapi_decode::Decoder::new(
                map_vaapi_codec(codec),
                map_vaapi_chroma(chroma),
                width,
                height,
            )
            .map(|decoder| Self::Vaapi(Box::new(decoder)))
            .map_err(map_vaapi_error),
            DecoderBackend::Vulkan => crate::video_decode_vulkan::Decoder::new(
                map_vulkan_codec(codec),
                map_vulkan_chroma(chroma),
                width,
                height,
            )
            .map(|decoder| Self::Vulkan(Box::new(decoder)))
            .map_err(map_vulkan_error),
            DecoderBackend::Software => crate::software_decode::Decoder::new(
                map_software_codec(codec),
                map_software_chroma(chroma),
                width,
                height,
            )
            .map(|decoder| Self::Software(Box::new(decoder)))
            .map_err(map_software_error),
            #[cfg(test)]
            DecoderBackend::FailsForTest => Ok(Self::FailsForTest),
            #[cfg(test)]
            DecoderBackend::FailsAfterRecoveryForTest => {
                Ok(Self::FailsAfterRecoveryForTest { recovered: false })
            }
        }
    }

    fn decode(
        &mut self,
        encoded: &[u8],
        recovery_packet: bool,
    ) -> Result<Option<Vec<u8>>, DecodeError> {
        match self {
            Self::Nvdec(decoder) => decoder
                .decode(encoded, recovery_packet)
                .map(Some)
                .map_err(map_nvdec_error),
            Self::Vaapi(decoder) => decoder.decode(encoded).map_err(map_vaapi_error),
            Self::Vulkan(decoder) => decoder.decode(encoded).map_err(map_vulkan_error),
            Self::Software(decoder) => decoder
                .decode(encoded, recovery_packet)
                .map_err(map_software_error),
            #[cfg(test)]
            Self::FailsForTest => Err(DecodeError::Resource(
                "injected hardware decode failure".into(),
            )),
            #[cfg(test)]
            Self::FailsAfterRecoveryForTest { recovered } => {
                if recovery_packet && !*recovered {
                    *recovered = true;
                    Ok(Some(vec![0; 16 * 16 * 4]))
                } else {
                    Err(DecodeError::Resource(
                        "injected dependent-packet hardware failure".into(),
                    ))
                }
            }
        }
    }

    fn flush(&mut self) {
        match self {
            Self::Nvdec(decoder) => {
                let _ = decoder.reset();
            }
            Self::Vaapi(decoder) => decoder.flush(),
            Self::Vulkan(decoder) => decoder.flush(),
            Self::Software(decoder) => decoder.flush(),
            #[cfg(test)]
            Self::FailsForTest => {}
            #[cfg(test)]
            Self::FailsAfterRecoveryForTest { recovered } => *recovered = false,
        }
    }
}

/// One stateful codec stream. Reference state and the failed-backend cursor
/// are deliberately scoped to one camera lease.
pub(crate) struct Decoder {
    session: Option<DecoderSession>,
    remaining_backends: VecDeque<DecoderBackend>,
    codec: VideoCodec,
    chroma: Chroma,
    width: usize,
    height: usize,
    headers_validated: bool,
    announced_backend: bool,
}

impl Decoder {
    pub(crate) fn new(
        codec: VideoCodec,
        chroma: Chroma,
        width: u16,
        height: u16,
    ) -> Result<Self, DecodeError> {
        Self::new_with_order(codec, chroma, width, height, configured_decoder_order())
    }

    fn new_with_order(
        codec: VideoCodec,
        chroma: Chroma,
        width: u16,
        height: u16,
        remaining_backends: VecDeque<DecoderBackend>,
    ) -> Result<Self, DecodeError> {
        let decoded_width = usize::from(width);
        let decoded_height = usize::from(height);
        validate_dimensions(chroma, decoded_width, decoded_height)?;
        let mut decoder = Self {
            session: None,
            remaining_backends,
            codec,
            chroma,
            width: decoded_width,
            height: decoded_height,
            headers_validated: false,
            announced_backend: false,
        };
        decoder.activate_next_backend()?;
        Ok(decoder)
    }

    fn new_software(
        codec: VideoCodec,
        chroma: Chroma,
        width: u16,
        height: u16,
    ) -> Result<Self, DecodeError> {
        Self::new_with_order(
            codec,
            chroma,
            width,
            height,
            VecDeque::from([DecoderBackend::Software]),
        )
    }

    fn activate_next_backend(&mut self) -> Result<(), DecodeError> {
        self.session = None;
        let mut failures = Vec::new();
        while let Some(backend) = self.remaining_backends.pop_front() {
            match DecoderSession::new(
                backend,
                self.codec,
                self.chroma,
                self.width as u16,
                self.height as u16,
            ) {
                Ok(session) => {
                    self.session = Some(session);
                    self.announced_backend = false;
                    return Ok(());
                }
                Err(error) => {
                    if backend.hardware() {
                        eprintln!(
                            "[camera-decode] {} {} {}x{}: {} unavailable ({error}); trying next backend",
                            codec_name(self.codec),
                            chroma_name(self.chroma),
                            self.width,
                            self.height,
                            backend.label(),
                        );
                    }
                    failures.push(format!("{}: {error}", backend.label()));
                }
            }
        }
        Err(DecodeError::Unavailable(if failures.is_empty() {
            "no configured camera decoder backend is available".into()
        } else {
            failures.join("; ")
        }))
    }

    fn active_backend(&self) -> Result<DecoderBackend, DecodeError> {
        match self.session.as_ref() {
            Some(DecoderSession::Nvdec(_)) => Ok(DecoderBackend::Nvdec),
            Some(DecoderSession::Vaapi(_)) => Ok(DecoderBackend::Vaapi),
            Some(DecoderSession::Vulkan(_)) => Ok(DecoderBackend::Vulkan),
            Some(DecoderSession::Software(_)) => Ok(DecoderBackend::Software),
            #[cfg(test)]
            Some(DecoderSession::FailsForTest) => Ok(DecoderBackend::FailsForTest),
            #[cfg(test)]
            Some(DecoderSession::FailsAfterRecoveryForTest { .. }) => {
                Ok(DecoderBackend::FailsAfterRecoveryForTest)
            }
            None => Err(DecodeError::Unavailable("no active decoder backend".into())),
        }
    }

    /// Decode one complete access unit / temporal unit. A recovery packet is
    /// retried on each fresh backend. A dependent packet is never fed into a
    /// fresh backend without its references: the caller gets HardwareReset
    /// after the cursor advances and must establish a keyframe barrier.
    pub(crate) fn decode(
        &mut self,
        encoded: &[u8],
        recovery_packet: bool,
    ) -> Result<Option<Vec<u8>>, DecodeError> {
        if encoded.is_empty() {
            return Err(DecodeError::Malformed("empty packet".into()));
        }
        if encoded.len() > MAX_ENCODED_BYTES {
            return Err(DecodeError::Resource(format!(
                "encoded packet is {} bytes (maximum {MAX_ENCODED_BYTES})",
                encoded.len()
            )));
        }

        match self.codec {
            VideoCodec::H264 => validate_h264_packet(
                encoded,
                self.chroma,
                self.width,
                self.height,
                &mut self.headers_validated,
            )?,
            VideoCodec::Av1 => {
                validate_av1_packet(
                    encoded,
                    self.chroma,
                    self.width,
                    self.height,
                    &mut self.headers_validated,
                )?;
            }
        }

        loop {
            let backend = self.active_backend()?;
            let result = self
                .session
                .as_mut()
                .expect("active_backend checked the session")
                .decode(encoded, recovery_packet)
                .and_then(|frame| {
                    if let Some(ref rgba) = frame {
                        let expected = self.width * self.height * 4;
                        if rgba.len() != expected {
                            return Err(DecodeError::UnsupportedOutput(format!(
                                "{} returned {} RGBA bytes, expected {expected}",
                                backend.label(),
                                rgba.len(),
                            )));
                        }
                    }
                    Ok(frame)
                });
            let hardware_failed = match &result {
                Err(_) => backend.hardware(),
                Ok(None) => recovery_packet && backend.hardware(),
                Ok(Some(_)) => false,
            };
            if !hardware_failed {
                if matches!(&result, Ok(Some(_))) && !self.announced_backend {
                    eprintln!(
                        "[camera-decode] {} {} {}x{}: {}",
                        codec_name(self.codec),
                        chroma_name(self.chroma),
                        self.width,
                        self.height,
                        backend.label(),
                    );
                    self.announced_backend = true;
                }
                return result;
            }

            let detail = match result {
                Err(error) => error.to_string(),
                Ok(None) => "recovery packet produced no frame".into(),
                Ok(Some(_)) => unreachable!(),
            };
            eprintln!(
                "[camera-decode] {} {} {}x{}: {} failed ({detail}); trying next backend",
                codec_name(self.codec),
                chroma_name(self.chroma),
                self.width,
                self.height,
                backend.label(),
            );
            self.activate_next_backend()?;
            if recovery_packet {
                continue;
            }

            self.headers_validated = false;
            return Err(DecodeError::HardwareReset(format!(
                "{} failed ({detail}); advanced to {}",
                backend.label(),
                self.active_backend()?.label(),
            )));
        }
    }

    pub(crate) fn flush(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.flush();
        }
        self.headers_validated = false;
    }
}

pub(crate) fn available(codec: VideoCodec, chroma: Chroma) -> bool {
    static H264_420: OnceLock<bool> = OnceLock::new();
    static H264_444: OnceLock<bool> = OnceLock::new();
    static AV1_420: OnceLock<bool> = OnceLock::new();
    static AV1_444: OnceLock<bool> = OnceLock::new();
    let probe = match (codec, chroma) {
        (VideoCodec::H264, Chroma::Cs420) => &H264_420,
        (VideoCodec::H264, Chroma::Cs444) => &H264_444,
        (VideoCodec::Av1, Chroma::Cs420) => &AV1_420,
        (VideoCodec::Av1, Chroma::Cs444) => &AV1_444,
    };
    // Capability advertisement must not initialize a user's GPU or open a DRM
    // node. The bundled software path guarantees the profile; native devices
    // are selected opportunistically once a lease receives its keyframe.
    *probe.get_or_init(|| Decoder::new_software(codec, chroma, 64, 64).is_ok())
}

pub(crate) fn preflight_keyframe(
    codec: VideoCodec,
    chroma: Chroma,
    encoded: &[u8],
    width: u16,
    height: u16,
) -> Result<(), DecodeError> {
    if encoded.is_empty() {
        return Err(DecodeError::Malformed("empty keyframe packet".into()));
    }
    if encoded.len() > MAX_ENCODED_BYTES {
        return Err(DecodeError::Resource(format!(
            "encoded keyframe is {} bytes (maximum {MAX_ENCODED_BYTES})",
            encoded.len()
        )));
    }
    let width = usize::from(width);
    let height = usize::from(height);
    validate_dimensions(chroma, width, height)?;
    match codec {
        VideoCodec::H264 => preflight_h264_keyframe(encoded, chroma, width, height),
        VideoCodec::Av1 => preflight_av1_keyframe(encoded, chroma, width, height),
    }
}

fn map_nvdec_codec(codec: VideoCodec) -> crate::nvdec_decode::NvdecCodec {
    match codec {
        VideoCodec::H264 => crate::nvdec_decode::NvdecCodec::H264,
        VideoCodec::Av1 => crate::nvdec_decode::NvdecCodec::Av1,
    }
}

fn map_nvdec_chroma(chroma: Chroma) -> crate::nvdec_decode::NvdecChroma {
    match chroma {
        Chroma::Cs420 => crate::nvdec_decode::NvdecChroma::Cs420,
        Chroma::Cs444 => crate::nvdec_decode::NvdecChroma::Cs444,
    }
}

fn map_vaapi_codec(codec: VideoCodec) -> crate::vaapi_decode::Codec {
    match codec {
        VideoCodec::H264 => crate::vaapi_decode::Codec::H264,
        VideoCodec::Av1 => crate::vaapi_decode::Codec::Av1,
    }
}

fn map_vaapi_chroma(chroma: Chroma) -> crate::vaapi_decode::Chroma {
    match chroma {
        Chroma::Cs420 => crate::vaapi_decode::Chroma::Cs420,
        Chroma::Cs444 => crate::vaapi_decode::Chroma::Cs444,
    }
}

fn map_vulkan_codec(codec: VideoCodec) -> crate::video_decode_vulkan::Codec {
    match codec {
        VideoCodec::H264 => crate::video_decode_vulkan::Codec::H264,
        VideoCodec::Av1 => crate::video_decode_vulkan::Codec::Av1,
    }
}

fn map_vulkan_chroma(chroma: Chroma) -> crate::video_decode_vulkan::Chroma {
    match chroma {
        Chroma::Cs420 => crate::video_decode_vulkan::Chroma::Cs420,
        Chroma::Cs444 => crate::video_decode_vulkan::Chroma::Cs444,
    }
}

fn map_software_codec(codec: VideoCodec) -> crate::software_decode::Codec {
    match codec {
        VideoCodec::H264 => crate::software_decode::Codec::H264,
        VideoCodec::Av1 => crate::software_decode::Codec::Av1,
    }
}

fn map_software_chroma(chroma: Chroma) -> crate::software_decode::Chroma {
    match chroma {
        Chroma::Cs420 => crate::software_decode::Chroma::Cs420,
        Chroma::Cs444 => crate::software_decode::Chroma::Cs444,
    }
}

fn map_software_error(error: crate::software_decode::Error) -> DecodeError {
    match error {
        crate::software_decode::Error::Unavailable(detail) => DecodeError::Unavailable(detail),
        crate::software_decode::Error::Invalid(detail) => DecodeError::Malformed(detail),
        crate::software_decode::Error::Unsupported(detail) => {
            DecodeError::UnsupportedOutput(detail)
        }
        crate::software_decode::Error::Resource(detail) => DecodeError::Resource(detail),
    }
}

fn map_nvdec_error(error: crate::nvdec_decode::NvdecError) -> DecodeError {
    match error {
        crate::nvdec_decode::NvdecError::Unavailable(detail) => DecodeError::Unavailable(detail),
        crate::nvdec_decode::NvdecError::InvalidInput(detail) => DecodeError::Malformed(detail),
        crate::nvdec_decode::NvdecError::UnsupportedOutput(detail) => {
            DecodeError::UnsupportedOutput(detail)
        }
        crate::nvdec_decode::NvdecError::Driver(detail) => DecodeError::Resource(detail),
    }
}

fn map_vaapi_error(error: crate::vaapi_decode::Error) -> DecodeError {
    match error {
        crate::vaapi_decode::Error::Unavailable(detail) => DecodeError::Unavailable(detail),
        crate::vaapi_decode::Error::Invalid(detail) => DecodeError::Malformed(detail),
        crate::vaapi_decode::Error::Resource(detail) => DecodeError::Resource(detail),
    }
}

fn map_vulkan_error(error: crate::video_decode_vulkan::Error) -> DecodeError {
    match error {
        crate::video_decode_vulkan::Error::Unavailable(detail) => DecodeError::Unavailable(detail),
        crate::video_decode_vulkan::Error::Invalid(detail) => DecodeError::Malformed(detail),
        crate::video_decode_vulkan::Error::Resource(detail) => DecodeError::Resource(detail),
    }
}

fn validate_dimensions(chroma: Chroma, width: usize, height: usize) -> Result<(), DecodeError> {
    if width == 0 || height == 0 {
        return Err(DecodeError::UnsupportedOutput(
            "dimensions must be non-zero".into(),
        ));
    }
    if width > 4096 || height > 4096 {
        return Err(DecodeError::Resource(format!(
            "dimensions {width}x{height} exceed the decoder bound"
        )));
    }
    width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| DecodeError::Resource("RGBA output size overflow".into()))?;
    if chroma == Chroma::Cs420 && (!width.is_multiple_of(2) || !height.is_multiple_of(2)) {
        return Err(DecodeError::UnsupportedOutput(
            "4:2:0 dimensions must be even".into(),
        ));
    }
    Ok(())
}

fn codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "H.264",
        VideoCodec::Av1 => "AV1",
    }
}

fn chroma_name(chroma: Chroma) -> &'static str {
    match chroma {
        Chroma::Cs420 => "4:2:0",
        Chroma::Cs444 => "4:4:4",
    }
}

// -------------------------------------------------------------------------
// H.264 Annex-B preflight
// -------------------------------------------------------------------------

fn preflight_h264_keyframe(
    encoded: &[u8],
    expected_chroma: Chroma,
    expected_width: usize,
    expected_height: usize,
) -> Result<(), DecodeError> {
    let mut headers_validated = false;
    validate_h264_packet(
        encoded,
        expected_chroma,
        expected_width,
        expected_height,
        &mut headers_validated,
    )?;

    let starts = annex_b_starts(encoded)?;
    let mut saw_sps = false;
    let mut saw_pps = false;
    let mut saw_idr = false;
    for (index, &(start, prefix_len)) in starts.iter().enumerate() {
        let nal_start = start + prefix_len;
        let nal_end = starts
            .get(index + 1)
            .map_or(encoded.len(), |&(next, _)| next);
        if nal_start >= nal_end {
            continue;
        }
        match encoded[nal_start] & 0x1f {
            5 => saw_idr = true,
            7 => {
                saw_sps = true;
            }
            8 => saw_pps = true,
            _ => {}
        }
    }
    if !saw_sps || !saw_pps || !saw_idr {
        return Err(DecodeError::Malformed(format!(
            "H.264 keyframe requires SPS, PPS, and IDR (found SPS={saw_sps}, PPS={saw_pps}, IDR={saw_idr})"
        )));
    }
    Ok(())
}

fn validate_h264_packet(
    encoded: &[u8],
    expected_chroma: Chroma,
    expected_width: usize,
    expected_height: usize,
    headers_validated: &mut bool,
) -> Result<(), DecodeError> {
    let starts = annex_b_starts(encoded)?;
    let mut saw_sps = false;
    for (index, &(start, prefix_len)) in starts.iter().enumerate() {
        let nal_start = start + prefix_len;
        let nal_end = starts
            .get(index + 1)
            .map_or(encoded.len(), |&(next, _)| next);
        if nal_start >= nal_end {
            continue;
        }
        let header = encoded[nal_start];
        if header & 0x80 != 0 {
            return Err(DecodeError::Malformed(
                "H.264 forbidden_zero_bit is set".into(),
            ));
        }
        if header & 0x1f == 7 {
            let info = parse_h264_sps(&encoded[nal_start + 1..nal_end])?;
            if info.chroma != expected_chroma {
                return Err(DecodeError::UnsupportedOutput(format!(
                    "H.264 SPS is {}, expected {}",
                    chroma_name(info.chroma),
                    chroma_name(expected_chroma)
                )));
            }
            if info.bit_depth != 8 {
                return Err(DecodeError::UnsupportedOutput(format!(
                    "H.264 SPS is {}-bit, expected 8-bit",
                    info.bit_depth
                )));
            }
            if expected_chroma == Chroma::Cs444 && info.profile_idc != 244 {
                return Err(DecodeError::UnsupportedOutput(format!(
                    "H.264 4:4:4 SPS uses profile_idc {}, expected High 4:4:4 Predictive (244)",
                    info.profile_idc
                )));
            }
            if info.width != expected_width || info.height != expected_height {
                return Err(DecodeError::UnsupportedOutput(format!(
                    "H.264 SPS dimensions {}x{}, expected {expected_width}x{expected_height}",
                    info.width, info.height
                )));
            }
            saw_sps = true;
        }
    }
    if saw_sps {
        *headers_validated = true;
    }
    if !*headers_validated {
        return Err(DecodeError::Malformed(
            "first H.264 packet does not contain an SPS".into(),
        ));
    }
    Ok(())
}

fn annex_b_starts(encoded: &[u8]) -> Result<Vec<(usize, usize)>, DecodeError> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= encoded.len() {
        let prefix_len = if encoded[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if encoded[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        starts.push((index, prefix_len));
        if starts.len() > MAX_UNITS_PER_PACKET {
            return Err(DecodeError::Resource(
                "too many H.264 NAL units in one packet".into(),
            ));
        }
        index += prefix_len;
    }
    let Some(&(first, _)) = starts.first() else {
        return Err(DecodeError::Malformed("H.264 packet is not Annex-B".into()));
    };
    if encoded[..first].iter().any(|&byte| byte != 0) {
        return Err(DecodeError::Malformed(
            "non-zero data precedes the first H.264 start code".into(),
        ));
    }
    Ok(starts)
}

#[derive(Debug)]
struct H264Sps {
    profile_idc: u8,
    chroma: Chroma,
    bit_depth: u8,
    width: usize,
    height: usize,
}

fn parse_h264_sps(escaped: &[u8]) -> Result<H264Sps, DecodeError> {
    if escaped.is_empty() {
        return Err(DecodeError::Malformed("empty H.264 SPS".into()));
    }
    let rbsp = h264_unescape(escaped)?;
    let mut bits = BitReader::new(&rbsp);
    let profile_idc = bits.read_bits(8)? as u8;
    bits.skip_bits(8)?; // constraint flags + reserved_zero_2bits
    bits.skip_bits(8)?; // level_idc
    let sps_id = bits.read_ue()?;
    if sps_id > 31 {
        return Err(DecodeError::Malformed("H.264 SPS id exceeds 31".into()));
    }

    let mut chroma_format_idc = 1_u32;
    let mut bit_depth_luma_minus8 = 0_u32;
    let mut bit_depth_chroma_minus8 = 0_u32;
    if matches!(
        profile_idc,
        44 | 83 | 86 | 100 | 110 | 118 | 122 | 128 | 134 | 135 | 138 | 139 | 244
    ) {
        chroma_format_idc = bits.read_ue()?;
        if chroma_format_idc > 3 {
            return Err(DecodeError::Malformed(
                "H.264 chroma_format_idc exceeds 3".into(),
            ));
        }
        if chroma_format_idc == 3 && bits.read_bit()? {
            return Err(DecodeError::UnsupportedOutput(
                "H.264 separate colour planes are not supported".into(),
            ));
        }
        bit_depth_luma_minus8 = bits.read_ue()?;
        bit_depth_chroma_minus8 = bits.read_ue()?;
        if bit_depth_luma_minus8 > 6 || bit_depth_chroma_minus8 > 6 {
            return Err(DecodeError::Malformed(
                "H.264 SPS bit depth is out of range".into(),
            ));
        }
        bits.skip_bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if bits.read_bit()? {
            let scaling_lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for index in 0..scaling_lists {
                if bits.read_bit()? {
                    skip_h264_scaling_list(&mut bits, if index < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    if bit_depth_luma_minus8 != bit_depth_chroma_minus8 {
        return Err(DecodeError::UnsupportedOutput(
            "H.264 luma/chroma bit depths differ".into(),
        ));
    }

    bits.read_ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = bits.read_ue()?;
    match pic_order_cnt_type {
        0 => {
            bits.read_ue()?;
        }
        1 => {
            bits.skip_bits(1)?;
            bits.read_se()?;
            bits.read_se()?;
            let cycle = bits.read_ue()?;
            if cycle > 255 {
                return Err(DecodeError::Resource(
                    "H.264 POC cycle exceeds 255 entries".into(),
                ));
            }
            for _ in 0..cycle {
                bits.read_se()?;
            }
        }
        2 => {}
        _ => {
            return Err(DecodeError::Malformed(
                "H.264 pic_order_cnt_type exceeds 2".into(),
            ));
        }
    }
    bits.read_ue()?; // max_num_ref_frames
    bits.skip_bits(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs_minus1 = bits.read_ue()?;
    let height_map_units_minus1 = bits.read_ue()?;
    let frame_mbs_only = bits.read_bit()?;
    if !frame_mbs_only {
        bits.skip_bits(1)?; // mb_adaptive_frame_field_flag
    }
    bits.skip_bits(1)?; // direct_8x8_inference_flag

    let (crop_left, crop_right, crop_top, crop_bottom) = if bits.read_bit()? {
        (
            bits.read_ue()?,
            bits.read_ue()?,
            bits.read_ue()?,
            bits.read_ue()?,
        )
    } else {
        (0, 0, 0, 0)
    };

    let coded_width = usize::try_from(width_mbs_minus1)
        .ok()
        .and_then(|value| value.checked_add(1))
        .and_then(|value| value.checked_mul(16))
        .ok_or_else(|| DecodeError::Resource("H.264 SPS width overflow".into()))?;
    let coded_height = usize::try_from(height_map_units_minus1)
        .ok()
        .and_then(|value| value.checked_add(1))
        .and_then(|value| value.checked_mul(if frame_mbs_only { 16 } else { 32 }))
        .ok_or_else(|| DecodeError::Resource("H.264 SPS height overflow".into()))?;

    let (crop_unit_x, crop_unit_y) = match chroma_format_idc {
        0 => (1_usize, if frame_mbs_only { 1 } else { 2 }),
        1 => (2, if frame_mbs_only { 2 } else { 4 }),
        2 => (2, if frame_mbs_only { 1 } else { 2 }),
        3 => (1, if frame_mbs_only { 1 } else { 2 }),
        _ => unreachable!(),
    };
    let crop_x = usize::try_from(
        crop_left
            .checked_add(crop_right)
            .ok_or_else(|| DecodeError::Resource("H.264 horizontal crop overflow".into()))?,
    )
    .ok()
    .and_then(|value| value.checked_mul(crop_unit_x))
    .ok_or_else(|| DecodeError::Resource("H.264 horizontal crop overflow".into()))?;
    let crop_y = usize::try_from(
        crop_top
            .checked_add(crop_bottom)
            .ok_or_else(|| DecodeError::Resource("H.264 vertical crop overflow".into()))?,
    )
    .ok()
    .and_then(|value| value.checked_mul(crop_unit_y))
    .ok_or_else(|| DecodeError::Resource("H.264 vertical crop overflow".into()))?;
    let width = coded_width
        .checked_sub(crop_x)
        .filter(|&value| value > 0)
        .ok_or_else(|| DecodeError::Malformed("H.264 SPS crops away its width".into()))?;
    let height = coded_height
        .checked_sub(crop_y)
        .filter(|&value| value > 0)
        .ok_or_else(|| DecodeError::Malformed("H.264 SPS crops away its height".into()))?;

    let chroma = match chroma_format_idc {
        1 => Chroma::Cs420,
        3 => Chroma::Cs444,
        0 => {
            return Err(DecodeError::UnsupportedOutput(
                "monochrome H.264 is not supported".into(),
            ));
        }
        2 => {
            return Err(DecodeError::UnsupportedOutput(
                "H.264 4:2:2 is not supported".into(),
            ));
        }
        _ => unreachable!(),
    };
    Ok(H264Sps {
        profile_idc,
        chroma,
        bit_depth: 8 + bit_depth_luma_minus8 as u8,
        width,
        height,
    })
}

fn h264_unescape(escaped: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let mut output = Vec::with_capacity(escaped.len());
    let mut zeroes = 0_u8;
    for &byte in escaped {
        if zeroes >= 2 && byte == 3 {
            zeroes = 0;
            continue;
        }
        if zeroes >= 2 && byte <= 2 {
            return Err(DecodeError::Malformed(
                "unescaped H.264 start-code sequence in SPS".into(),
            ));
        }
        output.push(byte);
        zeroes = if byte == 0 {
            zeroes.saturating_add(1)
        } else {
            0
        };
    }
    Ok(output)
}

fn skip_h264_scaling_list(bits: &mut BitReader<'_>, size: usize) -> Result<(), DecodeError> {
    let mut last_scale = 8_i32;
    let mut next_scale = 8_i32;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = bits.read_se()?;
            next_scale = (last_scale + delta).rem_euclid(256);
        }
        if next_scale != 0 {
            last_scale = next_scale;
        }
    }
    Ok(())
}

// -------------------------------------------------------------------------
// AV1 low-overhead OBU preflight
// -------------------------------------------------------------------------

fn preflight_av1_keyframe(
    encoded: &[u8],
    expected_chroma: Chroma,
    expected_width: usize,
    expected_height: usize,
) -> Result<(), DecodeError> {
    let mut headers_validated = false;
    validate_av1_packet(
        encoded,
        expected_chroma,
        expected_width,
        expected_height,
        &mut headers_validated,
    )?;

    let mut position = 0_usize;
    let mut units = 0_usize;
    let mut reduced_still_picture_header = None;
    let mut saw_sequence = false;
    let mut saw_complete_key_frame = false;
    let mut saw_key_frame_header = false;
    let mut saw_tile_group = false;
    while position < encoded.len() {
        units += 1;
        if units > MAX_UNITS_PER_PACKET {
            return Err(DecodeError::Resource(
                "too many AV1 OBUs in one keyframe packet".into(),
            ));
        }
        let header = encoded[position];
        position += 1;
        let obu_type = (header >> 3) & 0x0f;
        if header & 0x04 != 0 {
            position = position
                .checked_add(1)
                .filter(|&value| value <= encoded.len())
                .ok_or_else(|| DecodeError::Malformed("truncated AV1 OBU extension".into()))?;
        }
        let (payload_len, leb_bytes) = read_leb128(&encoded[position..])?;
        position = position
            .checked_add(leb_bytes)
            .ok_or_else(|| DecodeError::Resource("AV1 OBU offset overflow".into()))?;
        let end = position
            .checked_add(payload_len)
            .filter(|&end| end <= encoded.len())
            .ok_or_else(|| DecodeError::Malformed("truncated AV1 OBU payload".into()))?;
        let payload = &encoded[position..end];
        match obu_type {
            1 => {
                let sequence = parse_av1_sequence_header(payload)?;
                reduced_still_picture_header = Some(sequence.reduced_still_picture_header);
                saw_sequence = true;
            }
            // OBU_FRAME_HEADER or OBU_FRAME.  The opening uncompressed-header
            // bits are sufficient to distinguish a true random-access KEY
            // frame from an inter frame falsely carrying MEDIA_DATA_KEYFRAME.
            3 | 6 => {
                let reduced = reduced_still_picture_header.ok_or_else(|| {
                    DecodeError::Malformed("AV1 frame precedes its sequence header".into())
                })?;
                let is_key = if reduced {
                    true
                } else {
                    let mut bits = BitReader::new(payload);
                    let show_existing_frame = bits.read_bit()?;
                    !show_existing_frame && bits.read_bits(2)? == 0
                };
                if !is_key {
                    return Err(DecodeError::Malformed(
                        "AV1 packet marked keyframe contains a non-KEY frame".into(),
                    ));
                }
                if obu_type == 6 {
                    saw_complete_key_frame = true;
                } else {
                    saw_key_frame_header = true;
                }
            }
            4 => saw_tile_group = true,
            _ => {}
        }
        position = end;
    }
    let saw_key_frame = saw_complete_key_frame || (saw_key_frame_header && saw_tile_group);
    if !saw_sequence || !saw_key_frame {
        return Err(DecodeError::Malformed(format!(
            "AV1 keyframe requires sequence header and KEY frame (found sequence={saw_sequence}, KEY={saw_key_frame})"
        )));
    }
    Ok(())
}

fn validate_av1_packet(
    encoded: &[u8],
    expected_chroma: Chroma,
    expected_width: usize,
    expected_height: usize,
    headers_validated: &mut bool,
) -> Result<Option<bool>, DecodeError> {
    let mut position = 0_usize;
    let mut units = 0_usize;
    let mut full_range = None;
    while position < encoded.len() {
        units += 1;
        if units > MAX_UNITS_PER_PACKET {
            return Err(DecodeError::Resource(
                "too many AV1 OBUs in one packet".into(),
            ));
        }
        let header = encoded[position];
        position += 1;
        if header & 0x80 != 0 || header & 0x01 != 0 {
            return Err(DecodeError::Malformed(
                "invalid AV1 OBU header reserved bits".into(),
            ));
        }
        let obu_type = (header >> 3) & 0x0f;
        let extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        if extension {
            let extension_header = *encoded
                .get(position)
                .ok_or_else(|| DecodeError::Malformed("truncated AV1 OBU extension".into()))?;
            position += 1;
            if extension_header & 0x07 != 0 {
                return Err(DecodeError::Malformed(
                    "invalid AV1 OBU extension reserved bits".into(),
                ));
            }
        }
        if !has_size {
            return Err(DecodeError::Malformed(
                "AV1 packet is not low-overhead OBU format (missing OBU size)".into(),
            ));
        }
        let (payload_len, leb_bytes) = read_leb128(&encoded[position..])?;
        position = position
            .checked_add(leb_bytes)
            .ok_or_else(|| DecodeError::Resource("AV1 OBU offset overflow".into()))?;
        let end = position
            .checked_add(payload_len)
            .filter(|&end| end <= encoded.len())
            .ok_or_else(|| DecodeError::Malformed("truncated AV1 OBU payload".into()))?;
        if obu_type == 1 {
            let sequence = parse_av1_sequence_header(&encoded[position..end])?;
            if sequence.chroma != expected_chroma {
                return Err(DecodeError::UnsupportedOutput(format!(
                    "AV1 sequence is {}, expected {}",
                    chroma_name(sequence.chroma),
                    chroma_name(expected_chroma)
                )));
            }
            if sequence.bit_depth != 8 {
                return Err(DecodeError::UnsupportedOutput(format!(
                    "AV1 sequence is {}-bit, expected 8-bit",
                    sequence.bit_depth
                )));
            }
            if sequence.width != expected_width || sequence.height != expected_height {
                return Err(DecodeError::UnsupportedOutput(format!(
                    "AV1 sequence dimensions {}x{}, expected {expected_width}x{expected_height}",
                    sequence.width, sequence.height
                )));
            }
            *headers_validated = true;
            full_range = Some(sequence.full_range);
        }
        position = end;
    }
    if !*headers_validated {
        return Err(DecodeError::Malformed(
            "first AV1 packet does not contain a sequence header OBU".into(),
        ));
    }
    Ok(full_range)
}

fn read_leb128(input: &[u8]) -> Result<(usize, usize), DecodeError> {
    let mut value = 0_u64;
    for index in 0..8 {
        let byte = *input
            .get(index)
            .ok_or_else(|| DecodeError::Malformed("truncated AV1 OBU size".into()))?;
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            let value = usize::try_from(value)
                .map_err(|_| DecodeError::Resource("AV1 OBU size overflow".into()))?;
            return Ok((value, index + 1));
        }
    }
    Err(DecodeError::Malformed(
        "AV1 OBU size uses more than 8 LEB128 bytes".into(),
    ))
}

#[derive(Debug)]
struct Av1Sequence {
    reduced_still_picture_header: bool,
    chroma: Chroma,
    bit_depth: u8,
    width: usize,
    height: usize,
    full_range: bool,
}

fn parse_av1_sequence_header(payload: &[u8]) -> Result<Av1Sequence, DecodeError> {
    let mut bits = BitReader::new(payload);
    let profile = bits.read_bits(3)? as u8;
    if profile > 2 {
        return Err(DecodeError::Malformed("AV1 profile exceeds 2".into()));
    }
    let _still_picture = bits.read_bit()?;
    let reduced = bits.read_bit()?;

    let decoder_model_info = if reduced {
        bits.skip_bits(5)?; // seq_level_idx; reduced headers force seq_tier=0
        None
    } else {
        let timing_info_present = bits.read_bit()?;
        let decoder_model = if timing_info_present {
            bits.skip_bits(32)?; // num_units_in_display_tick
            bits.skip_bits(32)?; // time_scale
            if bits.read_bit()? {
                bits.read_uvlc()?; // num_ticks_per_picture_minus_1
            }
            bits.read_bit()?
        } else {
            false
        };
        let decoder_lengths = if decoder_model {
            let buffer_delay_length = bits.read_bits(5)? as usize + 1;
            bits.skip_bits(32)?; // num_units_in_decoding_tick
            bits.skip_bits(5)?; // buffer_removal_time_length_minus_1
            bits.skip_bits(5)?; // frame_presentation_time_length_minus_1
            Some(buffer_delay_length)
        } else {
            None
        };
        let initial_display_delay_present = bits.read_bit()?;
        let operating_points = bits.read_bits(5)? as usize + 1;
        for _ in 0..operating_points {
            bits.skip_bits(12)?; // operating_point_idc
            let level = bits.read_bits(5)?;
            if level > 7 {
                bits.skip_bits(1)?; // seq_tier
            }
            if let Some(delay_bits) = decoder_lengths
                && bits.read_bit()?
            {
                bits.skip_bits(delay_bits)?;
                bits.skip_bits(delay_bits)?;
                bits.skip_bits(1)?;
            }
            if initial_display_delay_present && bits.read_bit()? {
                bits.skip_bits(4)?;
            }
        }
        decoder_lengths
    };
    let _ = decoder_model_info;

    let width_bits = bits.read_bits(4)? as usize + 1;
    let height_bits = bits.read_bits(4)? as usize + 1;
    let width = bits.read_bits(width_bits)? as usize + 1;
    let height = bits.read_bits(height_bits)? as usize + 1;
    if !reduced && bits.read_bit()? {
        bits.skip_bits(4)?; // delta_frame_id_length_minus_2
        bits.skip_bits(3)?; // additional_frame_id_length_minus_1
    }
    bits.skip_bits(1)?; // use_128x128_superblock
    bits.skip_bits(1)?; // enable_filter_intra
    bits.skip_bits(1)?; // enable_intra_edge_filter
    if !reduced {
        bits.skip_bits(4)?; // interintra, masked, warped, dual-filter
        let enable_order_hint = bits.read_bit()?;
        if enable_order_hint {
            bits.skip_bits(2)?; // enable_jnt_comp, enable_ref_frame_mvs
        }
        let choose_screen_content = bits.read_bit()?;
        let force_screen_content = if choose_screen_content {
            2
        } else {
            bits.read_bits(1)?
        };
        if force_screen_content > 0 {
            let choose_integer_mv = bits.read_bit()?;
            if !choose_integer_mv {
                bits.skip_bits(1)?;
            }
        }
        if enable_order_hint {
            bits.skip_bits(3)?; // order_hint_bits_minus_1
        }
    }
    bits.skip_bits(3)?; // enable_superres, enable_cdef, enable_restoration

    let high_bitdepth = bits.read_bit()?;
    let bit_depth = if profile == 2 && high_bitdepth {
        if bits.read_bit()? { 12 } else { 10 }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    let monochrome = if profile == 1 {
        false
    } else {
        bits.read_bit()?
    };
    let color_description_present = bits.read_bit()?;
    let (color_primaries, transfer, matrix) = if color_description_present {
        (
            bits.read_bits(8)? as u8,
            bits.read_bits(8)? as u8,
            bits.read_bits(8)? as u8,
        )
    } else {
        (2, 2, 2) // unspecified
    };

    if monochrome {
        return Err(DecodeError::UnsupportedOutput(
            "monochrome AV1 is not supported".into(),
        ));
    }

    let (full_range, subsampling_x, subsampling_y) =
        if color_primaries == 1 && transfer == 13 && matrix == 0 {
            (true, false, false)
        } else {
            let full_range = bits.read_bit()?;
            let (x, y) = match profile {
                0 => (true, true),
                1 => (false, false),
                2 if bit_depth == 12 => {
                    let x = bits.read_bit()?;
                    let y = x && bits.read_bit()?;
                    (x, y)
                }
                2 => (true, false),
                _ => unreachable!(),
            };
            if x && y {
                bits.skip_bits(2)?; // chroma_sample_position
            }
            (full_range, x, y)
        };
    bits.skip_bits(1)?; // separate_uv_delta_q

    let chroma = match (subsampling_x, subsampling_y) {
        (true, true) => Chroma::Cs420,
        (false, false) => Chroma::Cs444,
        _ => {
            return Err(DecodeError::UnsupportedOutput(
                "AV1 4:2:2 is not supported".into(),
            ));
        }
    };
    // Surface streaming uses AV1 Main for 4:2:0 and High for 4:4:4.  Keep
    // camera negotiation identical rather than silently accepting a profile
    // whose codec string means something else.
    let expected_profile = if chroma == Chroma::Cs420 { 0 } else { 1 };
    if profile != expected_profile {
        return Err(DecodeError::UnsupportedOutput(format!(
            "AV1 profile {profile} does not match {} profile {expected_profile}",
            chroma_name(chroma)
        )));
    }
    Ok(Av1Sequence {
        reduced_still_picture_header: reduced,
        chroma,
        bit_depth,
        width,
        height,
        full_range,
    })
}

// -------------------------------------------------------------------------
// Bounded bit reader shared by SPS and sequence-header preflight
// -------------------------------------------------------------------------

struct BitReader<'a> {
    input: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, bit: 0 }
    }

    fn read_bit(&mut self) -> Result<bool, DecodeError> {
        Ok(self.read_bits(1)? != 0)
    }

    fn read_bits(&mut self, count: usize) -> Result<u32, DecodeError> {
        if count > 32 {
            return Err(DecodeError::Resource(format!(
                "bit field of {count} bits exceeds 32"
            )));
        }
        let end = self
            .bit
            .checked_add(count)
            .filter(|&end| end <= self.input.len().saturating_mul(8))
            .ok_or_else(|| DecodeError::Malformed("truncated codec header".into()))?;
        let mut value = 0_u32;
        while self.bit < end {
            let byte = self.input[self.bit / 8];
            value = (value << 1) | u32::from((byte >> (7 - self.bit % 8)) & 1);
            self.bit += 1;
        }
        Ok(value)
    }

    fn skip_bits(&mut self, count: usize) -> Result<(), DecodeError> {
        let end = self
            .bit
            .checked_add(count)
            .filter(|&end| end <= self.input.len().saturating_mul(8))
            .ok_or_else(|| DecodeError::Malformed("truncated codec header".into()))?;
        self.bit = end;
        Ok(())
    }

    fn read_ue(&mut self) -> Result<u32, DecodeError> {
        let mut leading_zeroes = 0_u32;
        while !self.read_bit()? {
            leading_zeroes += 1;
            if leading_zeroes > 31 {
                return Err(DecodeError::Malformed(
                    "Exp-Golomb value exceeds 32 bits".into(),
                ));
            }
        }
        if leading_zeroes == 0 {
            return Ok(0);
        }
        let suffix = self.read_bits(leading_zeroes as usize)?;
        Ok(((1_u32 << leading_zeroes) - 1) + suffix)
    }

    fn read_se(&mut self) -> Result<i32, DecodeError> {
        let value = self.read_ue()?;
        if value & 1 == 0 {
            Ok(-i32::try_from(value / 2).unwrap_or(i32::MAX))
        } else {
            Ok(i32::try_from(value.div_ceil(2)).unwrap_or(i32::MAX))
        }
    }

    fn read_uvlc(&mut self) -> Result<u32, DecodeError> {
        // AV1's uvlc syntax is equivalent to unsigned Exp-Golomb, except 32
        // leading zeroes is the saturated all-ones value.
        let mut leading_zeroes = 0_u32;
        while !self.read_bit()? {
            leading_zeroes += 1;
            if leading_zeroes == 32 {
                return Ok(u32::MAX);
            }
        }
        if leading_zeroes == 0 {
            return Ok(0);
        }
        let suffix = self.read_bits(leading_zeroes as usize)?;
        Ok(((1_u32 << leading_zeroes) - 1) + suffix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H264_420_RED: &str = concat!(
        "000000016742c00addec0440000003004000000300a3c489e0",
        "0000000168ce0f2c8000000165888404bc4628000a8bc7000128d8e0002fad80"
    );
    const H264_444_RED: &str = concat!(
        "0000000167f4100a91977b01100000030010000003002840",
        "0000000168ee0f11211000000165888404bffef7847e0535c13bfe659341ebf9965a423d"
    );
    const AV1_420_RED: &str =
        "12000a0a000000019ff9b5f200803214100080000014b9b9ac6e4d145e9136bae9bc64a0";
    const AV1_444_RED: &str =
        "12000a09200000019ff9b5f2043215100080000014b9b9ac6e4d145e91424a8e4eaeebb0";

    #[test]
    fn decoder_order_normalizes_deduplicates_and_keeps_software_fallback() {
        assert_eq!(
            Vec::from(parse_decoder_order("vulkan,nvdec:vulkan,unknown")),
            vec![
                DecoderBackend::Vulkan,
                DecoderBackend::Nvdec,
                DecoderBackend::Software,
            ]
        );
        assert_eq!(
            Vec::from(parse_decoder_order(" CUDA , VAAPI ")),
            vec![
                DecoderBackend::Nvdec,
                DecoderBackend::Vaapi,
                DecoderBackend::Software,
            ]
        );
        assert_eq!(
            Vec::from(parse_decoder_order("software,nvdec")),
            vec![DecoderBackend::Software, DecoderBackend::Nvdec]
        );
        assert_eq!(
            Vec::from(parse_decoder_order("garbage")),
            vec![DecoderBackend::Software]
        );
    }

    #[test]
    fn packet_formats_and_dimensions_are_strict() {
        assert!(matches!(
            validate_h264_packet(&[0, 0, 0, 2, 0x67, 0], Chroma::Cs420, 16, 16, &mut false),
            Err(DecodeError::Malformed(_))
        ));
        // Sequence-header OBU type 1 with obu_has_size_field clear.
        assert!(matches!(
            validate_av1_packet(&[1 << 3, 0], Chroma::Cs420, 16, 16, &mut false),
            Err(DecodeError::Malformed(_))
        ));
        assert!(validate_dimensions(Chroma::Cs420, 1920, 1080).is_ok());
        assert!(validate_dimensions(Chroma::Cs420, 1919, 1080).is_err());
        assert!(validate_dimensions(Chroma::Cs444, 1919, 1079).is_ok());
        assert!(validate_dimensions(Chroma::Cs444, 4097, 16).is_err());
    }

    #[test]
    fn keyframe_preflight_requires_exact_profile_and_random_access() {
        let h264 = decode_hex(H264_420_RED);
        preflight_keyframe(VideoCodec::H264, Chroma::Cs420, &h264, 16, 16).unwrap();
        assert!(matches!(
            preflight_keyframe(VideoCodec::H264, Chroma::Cs444, &h264, 16, 16),
            Err(DecodeError::UnsupportedOutput(_))
        ));
        let idr = h264
            .windows(4)
            .rposition(|window| window == [0, 0, 0, 1])
            .unwrap();
        assert!(matches!(
            preflight_keyframe(VideoCodec::H264, Chroma::Cs420, &h264[..idr], 16, 16),
            Err(DecodeError::Malformed(_))
        ));

        let av1 = decode_hex(AV1_420_RED);
        preflight_keyframe(VideoCodec::Av1, Chroma::Cs420, &av1, 16, 16).unwrap();
        assert!(matches!(
            preflight_keyframe(VideoCodec::Av1, Chroma::Cs444, &av1, 16, 16),
            Err(DecodeError::UnsupportedOutput(_))
        ));
        assert!(matches!(
            preflight_keyframe(VideoCodec::Av1, Chroma::Cs420, &av1[..14], 16, 16),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn failed_hardware_retries_the_same_keyframe_in_software() {
        let encoded = decode_hex(H264_420_RED);
        let mut decoder = Decoder::new_with_order(
            VideoCodec::H264,
            Chroma::Cs420,
            16,
            16,
            VecDeque::from([DecoderBackend::FailsForTest, DecoderBackend::Software]),
        )
        .unwrap();
        let decoded = decoder
            .decode(&encoded, true)
            .unwrap()
            .expect("the recovery packet must decode on the fallback");
        assert_eq!(decoded.len(), 16 * 16 * 4);
        assert_eq!(decoder.active_backend().unwrap(), DecoderBackend::Software);
    }

    #[test]
    fn failed_delta_advances_without_feeding_it_to_the_fresh_backend() {
        let encoded = decode_hex(H264_420_RED);
        let mut decoder = Decoder::new_with_order(
            VideoCodec::H264,
            Chroma::Cs420,
            16,
            16,
            VecDeque::from([
                DecoderBackend::FailsAfterRecoveryForTest,
                DecoderBackend::Software,
            ]),
        )
        .unwrap();
        assert!(decoder.decode(&encoded, true).unwrap().is_some());

        let error = decoder.decode(&encoded, false).unwrap_err();
        assert!(matches!(error, DecodeError::HardwareReset(_)));
        assert_eq!(decoder.active_backend().unwrap(), DecoderBackend::Software);

        // The failed hardware backend was retired, and the next recovery
        // point starts directly on software instead of retrying it.
        assert!(decoder.decode(&encoded, true).unwrap().is_some());
        assert_eq!(decoder.active_backend().unwrap(), DecoderBackend::Software);
    }

    #[test]
    fn software_decodes_exact_camera_profile_matrix_and_flushes() {
        for (codec, chroma, encoded) in [
            (VideoCodec::H264, Chroma::Cs420, H264_420_RED),
            (VideoCodec::H264, Chroma::Cs444, H264_444_RED),
            (VideoCodec::Av1, Chroma::Cs420, AV1_420_RED),
            (VideoCodec::Av1, Chroma::Cs444, AV1_444_RED),
        ] {
            let encoded = decode_hex(encoded);
            preflight_keyframe(codec, chroma, &encoded, 16, 16).unwrap();
            let mut decoder = Decoder::new_software(codec, chroma, 16, 16).unwrap();
            let first = decoder
                .decode(&encoded, true)
                .unwrap()
                .expect("keyframe must decode immediately");
            assert_eq!(first.len(), 16 * 16 * 4);
            assert!(first.chunks_exact(4).all(|pixel| pixel[3] == 255));

            decoder.flush();
            let recovered = decoder
                .decode(&encoded, true)
                .unwrap()
                .expect("keyframe after flush must decode immediately");
            assert_eq!(recovered, first);
        }
    }

    #[test]
    fn software_h264_retains_reference_state_between_camera_packets() {
        let sequence = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../vendor/cros-codecs/src/codec/h264/test_data/64x64-I-P.h264"
        ));
        let starts = annex_b_starts(sequence).unwrap();
        let delta_start = starts
            .iter()
            .find_map(|&(start, prefix_len)| {
                (sequence[start + prefix_len] & 0x1f == 1).then_some(start)
            })
            .expect("fixture must contain a non-IDR picture");
        let (keyframe, delta) = sequence.split_at(delta_start);
        preflight_keyframe(VideoCodec::H264, Chroma::Cs420, keyframe, 64, 64).unwrap();

        let mut decoder = Decoder::new_software(VideoCodec::H264, Chroma::Cs420, 64, 64).unwrap();
        let key_rgba = decoder
            .decode(keyframe, true)
            .unwrap()
            .expect("fixture IDR must decode immediately");
        let delta_rgba = decoder
            .decode(delta, false)
            .unwrap()
            .expect("dependent picture must decode from retained references");
        assert_eq!(key_rgba.len(), 64 * 64 * 4);
        assert_eq!(delta_rgba.len(), 64 * 64 * 4);
    }

    #[test]
    fn software_av1_retains_reference_state_between_camera_packets() {
        let ivf = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../vendor/cros-codecs/src/codec/av1/test_data/test-25fps.av1.ivf"
        ));
        assert_eq!(&ivf[..4], b"DKIF");
        let first_len = u32::from_le_bytes(ivf[32..36].try_into().unwrap()) as usize;
        let first_start = 44;
        let first_end = first_start + first_len;
        let second_len =
            u32::from_le_bytes(ivf[first_end..first_end + 4].try_into().unwrap()) as usize;
        let second_start = first_end + 12;
        let first = &ivf[first_start..first_end];
        let second = &ivf[second_start..second_start + second_len];
        preflight_keyframe(VideoCodec::Av1, Chroma::Cs420, first, 320, 240).unwrap();

        let mut decoder = Decoder::new_software(VideoCodec::Av1, Chroma::Cs420, 320, 240).unwrap();
        let key_rgba = decoder
            .decode(first, true)
            .unwrap()
            .expect("fixture KEY frame must decode immediately");
        let delta_rgba = decoder
            .decode(second, false)
            .unwrap()
            .expect("dependent AV1 frame must decode from retained references");
        assert_eq!(key_rgba.len(), 320 * 240 * 4);
        assert_eq!(delta_rgba.len(), 320 * 240 * 4);
    }

    fn decode_hex(input: &str) -> Vec<u8> {
        assert_eq!(input.len() % 2, 0);
        input
            .as_bytes()
            .chunks_exact(2)
            .map(|digits| {
                let text = std::str::from_utf8(digits).unwrap();
                u8::from_str_radix(text, 16).unwrap()
            })
            .collect()
    }
}
