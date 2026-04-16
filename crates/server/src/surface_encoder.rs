#![allow(clippy::too_many_arguments)]

use blit_compositor::PixelData;
use blit_remote::{
    CODEC_SUPPORT_AV1, CODEC_SUPPORT_AV1_444, CODEC_SUPPORT_H264, CODEC_SUPPORT_H264_444,
    SURFACE_FRAME_CODEC_AV1, SURFACE_FRAME_CODEC_H264,
};
use openh264::encoder::Encoder as OpenH264Encoder;
use openh264::formats::YUVBuffer;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceEncoderPreference {
    VulkanVideoH264,
    VulkanVideoAV1,
    H264Software,
    H264Vaapi,
    AV1Vaapi,
    NvencH264,
    NvencAV1,
    AV1Software,
}

// Type alias for backwards compatibility in tests.
pub type SurfaceH264EncoderPreference = SurfaceEncoderPreference;

/// openh264 hard limit: 3840x2160 horizontal or 2160x3840 vertical.
const H264_MAX_WIDTH: u16 = 3840;
const H264_MAX_HEIGHT: u16 = 2160;

impl SurfaceEncoderPreference {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "h264-vulkan" => Some(Self::VulkanVideoH264),
            "av1-vulkan" => Some(Self::VulkanVideoAV1),
            "h264-software" | "software" => Some(Self::H264Software),
            "h264-vaapi" | "vaapi" => Some(Self::H264Vaapi),
            "av1-vaapi" => Some(Self::AV1Vaapi),
            "h264-nvenc" => Some(Self::NvencH264),
            "av1-nvenc" => Some(Self::NvencAV1),
            "av1-software" => Some(Self::AV1Software),
            _ => None,
        }
    }

    /// Parse a comma-separated list of encoder preferences.
    pub fn parse_list(value: &str) -> Result<Vec<Self>, String> {
        let mut result = Vec::new();
        for item in value.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            result.push(Self::parse(item).ok_or_else(|| format!("unknown encoder: {item}"))?);
        }
        Ok(result)
    }

    /// Sensible default: hardware before software, NVENC preferred.
    ///
    /// Override at runtime with `BLIT_SURFACE_ENCODERS=h264-nvenc,h264-software`
    /// (comma-separated list).
    pub fn defaults() -> Vec<Self> {
        if let Some(list) = std::env::var("BLIT_SURFACE_ENCODERS")
            .ok()
            .and_then(|v| Self::parse_list(&v).ok())
        {
            return list;
        }
        vec![
            // Vulkan Video encoders are not yet stable — enable via
            // BLIT_SURFACE_ENCODERS=av1-vulkan,h264-vulkan,...
            // Self::VulkanVideoAV1,
            // Self::VulkanVideoH264,
            Self::NvencAV1,
            Self::NvencH264,
            Self::AV1Vaapi,
            Self::H264Vaapi,
            Self::H264Software,
            Self::AV1Software,
        ]
    }

    /// Returns true if the given codec_support bitmask allows this encoder.
    /// A codec_support of 0 means "accept anything".
    pub fn supported_by_client(self, codec_support: u8) -> bool {
        if codec_support == 0 {
            return true;
        }
        match self {
            Self::H264Software | Self::H264Vaapi | Self::NvencH264 | Self::VulkanVideoH264 => {
                codec_support & CODEC_SUPPORT_H264 != 0
            }
            Self::AV1Vaapi | Self::AV1Software | Self::NvencAV1 | Self::VulkanVideoAV1 => {
                codec_support & CODEC_SUPPORT_AV1 != 0
            }
        }
    }

    /// Returns true if the client announced 4:4:4 chroma support for this
    /// encoder's codec family.  Legacy clients (codec_support == 0) are
    /// assumed to lack 4:4:4 support since the resulting Professional Profile
    /// bitstreams are not universally decodable.
    pub fn supports_444_by_client(self, codec_support: u8) -> bool {
        if codec_support == 0 {
            return false;
        }
        match self {
            Self::H264Software | Self::H264Vaapi | Self::NvencH264 | Self::VulkanVideoH264 => {
                codec_support & CODEC_SUPPORT_H264_444 != 0
            }
            Self::AV1Vaapi | Self::AV1Software | Self::NvencAV1 | Self::VulkanVideoAV1 => {
                codec_support & CODEC_SUPPORT_AV1_444 != 0
            }
        }
    }

    /// Maximum surface dimensions the encoder can handle.
    /// Returns `None` if there is no practical limit.
    pub fn max_dimensions(self) -> Option<(u16, u16)> {
        match self {
            Self::H264Software | Self::H264Vaapi | Self::NvencH264 | Self::VulkanVideoH264 => {
                Some((H264_MAX_WIDTH, H264_MAX_HEIGHT))
            }
            Self::AV1Vaapi | Self::NvencAV1 | Self::AV1Software | Self::VulkanVideoAV1 => None,
        }
    }

    /// Tightest max dimensions across a list of preferences.
    pub fn max_dimensions_for_list(prefs: &[Self]) -> Option<(u16, u16)> {
        let mut result: Option<(u16, u16)> = None;
        for p in prefs {
            if let Some((w, h)) = p.max_dimensions() {
                result = Some(match result {
                    Some((rw, rh)) => (rw.min(w), rh.min(h)),
                    None => (w, h),
                });
            }
        }
        result
    }

    /// Whether this encoder runs in the compositor via Vulkan Video.
    pub fn is_vulkan_video(self) -> bool {
        matches!(self, Self::VulkanVideoH264 | Self::VulkanVideoAV1)
    }

    /// Vulkan Video codec byte: 0x01 = H.264, 0x02 = AV1.
    pub fn vulkan_codec(self) -> u8 {
        match self {
            Self::VulkanVideoAV1 => 0x02,
            _ => 0x01,
        }
    }

    /// Codec flag matching `SURFACE_FRAME_CODEC_*` constants.
    pub fn codec_flag(self) -> u8 {
        match self {
            Self::H264Software | Self::H264Vaapi | Self::NvencH264 | Self::VulkanVideoH264 => {
                SURFACE_FRAME_CODEC_H264
            }
            Self::AV1Vaapi | Self::AV1Software | Self::NvencAV1 | Self::VulkanVideoAV1 => {
                SURFACE_FRAME_CODEC_AV1
            }
        }
    }
}

/// Chroma subsampling mode.
///
/// - **Cs420** (default): 4:2:0 — U/V at half horizontal and half vertical
///   resolution.  Universally supported, lower bandwidth.
/// - **Cs444**: 4:4:4 — full-resolution chroma.  Eliminates colour fringing
///   on sharp edges (ideal for text / UI), but requires encoder support.
///
/// Set via `BLIT_CHROMA` env var. Default: 444 (fall back to 420 if unsupported).
/// Use `BLIT_CHROMA=420` to force 4:2:0.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChromaSubsampling {
    Cs420,
    #[default]
    Cs444,
}

impl ChromaSubsampling {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cs420 => "4:2:0",
            Self::Cs444 => "4:4:4",
        }
    }

    pub fn from_env() -> Self {
        match std::env::var("BLIT_CHROMA").ok().as_deref() {
            Some("420") => Self::Cs420,
            _ => Self::Cs444,
        }
    }

    pub fn is_444(self) -> bool {
        matches!(self, Self::Cs444)
    }
}

/// Compute the AV1 level index string (e.g. "05") for the given dimensions,
/// assuming 60 fps.  Mirrors the client-side `av1LevelString()`.
pub fn av1_level_for(width: u32, height: u32) -> &'static str {
    let sps = width as u64 * height as u64 * 60;
    // (level_string, max_w, max_h, max_sample_rate)
    const SPECS: &[(&str, u32, u32, u64)] = &[
        ("00", 2048, 1152, 5_529_600),
        ("01", 2816, 1152, 10_454_400),
        ("04", 4352, 2448, 24_969_600),
        ("05", 5504, 3096, 39_938_400),
        ("08", 6144, 3456, 77_856_768),
        ("09", 6144, 3456, 155_713_536),
        ("12", 8192, 4352, 273_715_200),
        ("13", 8192, 4352, 547_430_400),
        ("16", 16384, 8704, 1_176_502_272),
    ];
    for &(level, max_w, max_h, max_rate) in SPECS {
        if width <= max_w && height <= max_h && sps <= max_rate {
            return level;
        }
    }
    "16"
}

/// Video quality preset.  Higher quality uses more CPU / bandwidth.
///
/// - **Low**: speed 10, quantizer 180 — minimal CPU, visibly lossy
/// - **Medium** (default): speed 10, quantizer 120 — good balance
/// - **High**: speed 8, quantizer 80 — sharp, noticeable CPU use
/// - **Ultra**: speed 6, quantizer 1 — near-lossless, heavy CPU
/// - **Custom**: caller-specified AV1 quantizer (10–255)
///
/// Set via `BLIT_SURFACE_QUALITY=low|medium|high|ultra`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SurfaceQuality {
    Low,
    #[default]
    Medium,
    High,
    Ultra,
    /// Caller-specified AV1 quantizer (10–255).  H.264 QP, encoder speed,
    /// and software-encoder bitrate are derived proportionally.
    Custom {
        quantizer: u8,
    },
}

impl SurfaceQuality {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "ultra" | "lossless" => Some(Self::Ultra),
            _ => None,
        }
    }

    /// Decode from the wire `quality` byte in C2S_SURFACE_SUBSCRIBE.
    ///
    /// - 0 → `None` (server default)
    /// - 1–4 → named presets
    /// - 10–255 → `Custom { quantizer: value }`
    /// - 5–9 → reserved, treated as server default
    pub fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Low),
            2 => Some(Self::Medium),
            3 => Some(Self::High),
            4 => Some(Self::Ultra),
            v @ 10..=255 => Some(Self::Custom { quantizer: v }),
            _ => None,
        }
    }

    /// rav1e speed preset (0 = slowest/best, 10 = fastest/worst).
    fn av1_speed(self) -> u8 {
        match self {
            Self::Low => 10,
            Self::Medium => 10,
            Self::High => 8,
            Self::Ultra => 6,
            Self::Custom { quantizer } => {
                if quantizer <= 40 {
                    6
                } else if quantizer <= 80 {
                    8
                } else {
                    10
                }
            }
        }
    }

    /// AV1 quantizer (0 = lossless, 255 = worst).
    /// Also used as VA-API `base_qindex` and NVENC AV1 QP.
    fn av1_quantizer(self) -> usize {
        match self {
            Self::Low => 180,
            Self::Medium => 120,
            Self::High => 80,
            Self::Ultra => 1,
            Self::Custom { quantizer } => quantizer as usize,
        }
    }

    /// rav1e min_quantizer — floor the encoder is allowed to improve to.
    fn av1_min_quantizer(self) -> u8 {
        match self {
            Self::Low => 120,
            Self::Medium => 80,
            Self::High => 40,
            Self::Ultra => 0,
            Self::Custom { quantizer } => quantizer.saturating_sub(40),
        }
    }

    /// H.264 QP for constant-quality mode (0 = best, 51 = worst).
    /// Used by NVENC H.264 and VA-API H.264.
    pub fn h264_qp(self) -> u8 {
        match self {
            Self::Low => 35,
            Self::Medium => 28,
            Self::High => 20,
            Self::Ultra => 10,
            Self::Custom { quantizer } => ((quantizer as u32) * 51 / 255).min(51) as u8,
        }
    }

    /// NVENC AV1 QP for constant-quality mode (0 = best, 255 = worst).
    /// Same scale as `av1_quantizer` / VA-API `base_qindex`.
    pub fn nvenc_av1_qp(self) -> u32 {
        self.av1_quantizer() as u32
    }

    /// AV1 QP for Vulkan Video encode (0 = best, 255 = worst).
    /// Same base_qindex scale as VA-API / NVENC.
    pub fn av1_qp_for_vulkan(self) -> u8 {
        self.av1_quantizer().min(255) as u8
    }

    /// openh264 target bitrate in bits/sec.  Resolution-independent
    /// approximation — openh264 adapts internally.
    fn openh264_bitrate(self) -> u32 {
        match self {
            Self::Low => 500_000,
            Self::Medium => 2_000_000,
            Self::High => 8_000_000,
            Self::Ultra => 20_000_000,
            Self::Custom { quantizer } => {
                // Linear interpolation: quantizer 0 → 20 Mbps, 255 → 500 kbps.
                let q = quantizer as u32;
                20_000_000 - q * (20_000_000 - 500_000) / 255
            }
        }
    }
}

pub struct SurfaceEncoder {
    /// Dimensions the encoder actually operates at (may be padded to even for H.264).
    width: u32,
    height: u32,
    /// Original surface dimensions before any padding.
    source_width: u32,
    source_height: u32,
    kind: SurfaceEncoderKind,
    /// Negotiated chroma subsampling (may differ from requested if backend
    /// does not support 4:4:4).
    chroma: ChromaSubsampling,
}

enum SurfaceEncoderKind {
    H264Software(Box<SoftwareH264Encoder>),
    NvencH264(Box<crate::nvenc_encode::NvencDirectEncoder>),
    NvencAV1(Box<crate::nvenc_encode::NvencDirectEncoder>),
    #[cfg(target_os = "linux")]
    H264Vaapi(Box<crate::vaapi_encode::VaapiDirectEncoder>),
    #[cfg(target_os = "linux")]
    AV1Vaapi(Box<crate::vaapi_encode::VaapiAv1Encoder>),
    AV1Software(Box<SoftwareAV1Encoder>),
}

impl SurfaceEncoder {
    /// Try each preference in order; return the first that succeeds and
    /// the client can decode.  `codec_support` is a bitmask of
    /// `CODEC_SUPPORT_*` (0 = accept anything).
    pub fn new(
        preferences: &[SurfaceEncoderPreference],
        width: u32,
        height: u32,
        vaapi_device: &str,
        quality: SurfaceQuality,
        verbose: bool,
        codec_support: u8,
        chroma: ChromaSubsampling,
    ) -> Result<Self, String> {
        let source_width = width;
        let source_height = height;
        let mut last_err = String::from("no encoders configured");

        // Single pass: for each encoder preference, try 4:4:4 first
        // (if requested and client-supported), then fall back to 4:2:0,
        // before moving to the next encoder.  This ensures e.g.
        // h264-software 4:2:0 beats av1-software 4:4:4.
        let try_444 = chroma.is_444();
        if try_444 && verbose {
            eprintln!(
                "[surface-encoder] 4:4:4 eligible: codec_support={codec_support:#04x} for {source_width}x{source_height}",
            );
        }

        for &pref in preferences {
            if pref.is_vulkan_video() {
                continue;
            }
            if !pref.supported_by_client(codec_support) {
                continue;
            }

            // Try 4:4:4 first for this encoder if the client supports it.
            if try_444 && pref.supports_444_by_client(codec_support) {
                match Self::try_one(
                    pref,
                    width,
                    height,
                    source_width,
                    source_height,
                    vaapi_device,
                    quality,
                    verbose,
                    ChromaSubsampling::Cs444,
                ) {
                    Ok(enc) => {
                        if verbose {
                            eprintln!(
                                "[surface-encoder] using {:?} 4:4:4 for {source_width}x{source_height}",
                                pref
                            );
                        }
                        return Ok(enc);
                    }
                    Err(err) => {
                        if verbose {
                            eprintln!(
                                "[surface-encoder] {:?} 4:4:4 unavailable for {source_width}x{source_height}: {err}",
                                pref
                            );
                        }
                        last_err = err;
                    }
                }
            }

            // Fall back to 4:2:0 for this encoder.
            match Self::try_one(
                pref,
                width,
                height,
                source_width,
                source_height,
                vaapi_device,
                quality,
                verbose,
                ChromaSubsampling::Cs420,
            ) {
                Ok(enc) => {
                    if verbose {
                        eprintln!(
                            "[surface-encoder] using {:?} 4:2:0 for {source_width}x{source_height}",
                            pref
                        );
                    }
                    return Ok(enc);
                }
                Err(err) => {
                    if verbose {
                        eprintln!(
                            "[surface-encoder] {:?} 4:2:0 unavailable for {source_width}x{source_height}: {err}",
                            pref
                        );
                    }
                    last_err = err;
                }
            }
        }
        Err(last_err)
    }

    fn try_one(
        pref: SurfaceEncoderPreference,
        width: u32,
        height: u32,
        source_width: u32,
        source_height: u32,
        vaapi_device: &str,
        quality: SurfaceQuality,
        verbose: bool,
        chroma: ChromaSubsampling,
    ) -> Result<Self, String> {
        let _ = vaapi_device;
        validate_surface_dimensions(width, height, pref)?;

        match pref {
            SurfaceEncoderPreference::VulkanVideoH264
            | SurfaceEncoderPreference::VulkanVideoAV1 => {
                Err("Vulkan Video encoders are managed by the compositor".into())
            }
            SurfaceEncoderPreference::NvencH264 => {
                let (width, height) = ((width + 1) & !1, (height + 1) & !1);
                let qp = quality.h264_qp() as u32;
                Ok(Self {
                    width,
                    height,
                    source_width,
                    source_height,
                    kind: SurfaceEncoderKind::NvencH264(Box::new(
                        crate::nvenc_encode::NvencDirectEncoder::try_new(
                            "h264", width, height, qp, verbose, chroma,
                        )?,
                    )),
                    chroma,
                })
            }
            SurfaceEncoderPreference::NvencAV1 => {
                // AV1 superblocks are 64x64; NVENC requires even dimensions
                // at minimum.  Round up to a multiple of 2 (matching H.264)
                // so chroma planes stay aligned.
                let (width, height) = ((width + 1) & !1, (height + 1) & !1);
                let qp = quality.nvenc_av1_qp();
                Ok(Self {
                    width,
                    height,
                    source_width,
                    source_height,
                    kind: SurfaceEncoderKind::NvencAV1(Box::new(
                        crate::nvenc_encode::NvencDirectEncoder::try_new(
                            "av1", width, height, qp, verbose, chroma,
                        )?,
                    )),
                    chroma,
                })
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderPreference::H264Vaapi => {
                let (width, height) = ((width + 1) & !1, (height + 1) & !1);
                Ok(Self {
                    width,
                    height,
                    source_width,
                    source_height,
                    kind: SurfaceEncoderKind::H264Vaapi(Box::new(
                        crate::vaapi_encode::VaapiDirectEncoder::try_new(
                            width,
                            height,
                            vaapi_device,
                            quality.h264_qp(),
                            verbose,
                            chroma,
                        )?,
                    )),
                    chroma,
                })
            }
            #[cfg(not(target_os = "linux"))]
            SurfaceEncoderPreference::H264Vaapi => Err("VA-API is only available on Unix".into()),
            #[cfg(target_os = "linux")]
            SurfaceEncoderPreference::AV1Vaapi => {
                let (width, height) = (width.div_ceil(64) * 64, height.div_ceil(64) * 64);
                Ok(Self {
                    width,
                    height,
                    source_width,
                    source_height,
                    kind: SurfaceEncoderKind::AV1Vaapi(Box::new(
                        crate::vaapi_encode::VaapiAv1Encoder::try_new(
                            width,
                            height,
                            source_width,
                            source_height,
                            vaapi_device,
                            quality.av1_quantizer() as u8,
                            verbose,
                            chroma,
                        )?,
                    )),
                    chroma,
                })
            }
            #[cfg(not(target_os = "linux"))]
            SurfaceEncoderPreference::AV1Vaapi => Err("VA-API is only available on Linux".into()),
            SurfaceEncoderPreference::AV1Software => Ok(Self {
                width,
                height,
                source_width,
                source_height,
                kind: SurfaceEncoderKind::AV1Software(Box::new(SoftwareAV1Encoder::new(
                    width, height, quality, chroma,
                )?)),
                chroma,
            }),
            SurfaceEncoderPreference::H264Software => {
                if chroma.is_444() {
                    return Err("openh264 does not support 4:4:4".into());
                }
                let (width, height) = ((width + 1) & !1, (height + 1) & !1);
                Ok(Self {
                    width,
                    height,
                    source_width,
                    source_height,
                    kind: SurfaceEncoderKind::H264Software(Box::new(SoftwareH264Encoder::new(
                        quality,
                    )?)),
                    chroma,
                })
            }
        }
    }

    /// The original surface dimensions before any encoder padding.
    pub fn source_dimensions(&self) -> (u32, u32) {
        (self.source_width, self.source_height)
    }

    /// The encoder's padded dimensions (may be larger than source due to
    /// alignment requirements, e.g. AV1 64-pixel superblock alignment).
    pub fn encoder_dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Human-readable name of the active encoder backend, sent to clients
    /// for display in debug panels.  Includes chroma subsampling when 4:4:4.
    pub fn encoder_name(&self) -> &'static str {
        match (&self.kind, self.chroma) {
            (SurfaceEncoderKind::H264Software(_), _) => "h264-software",
            (SurfaceEncoderKind::NvencH264(_), ChromaSubsampling::Cs444) => "h264-nvenc 4:4:4",
            (SurfaceEncoderKind::NvencH264(_), _) => "h264-nvenc",
            (SurfaceEncoderKind::NvencAV1(_), ChromaSubsampling::Cs444) => "av1-nvenc 4:4:4",
            (SurfaceEncoderKind::NvencAV1(_), _) => "av1-nvenc",
            #[cfg(target_os = "linux")]
            (SurfaceEncoderKind::H264Vaapi(_), ChromaSubsampling::Cs444) => "h264-vaapi 4:4:4",
            #[cfg(target_os = "linux")]
            (SurfaceEncoderKind::H264Vaapi(_), _) => "h264-vaapi",
            #[cfg(target_os = "linux")]
            (SurfaceEncoderKind::AV1Vaapi(_), ChromaSubsampling::Cs444) => "av1-vaapi 4:4:4",
            #[cfg(target_os = "linux")]
            (SurfaceEncoderKind::AV1Vaapi(_), _) => "av1-vaapi",
            (SurfaceEncoderKind::AV1Software(_), ChromaSubsampling::Cs444) => "av1-software 4:4:4",
            (SurfaceEncoderKind::AV1Software(_), _) => "av1-software",
        }
    }

    /// WebCodecs codec string for the active encoder.  Sent to the client
    /// so it can configure `VideoDecoder` with the correct profile/level.
    pub fn webcodecs_codec_string(&self) -> String {
        match &self.kind {
            SurfaceEncoderKind::H264Software(_) => {
                if self.chroma.is_444() {
                    "avc1.F4001f".to_string()
                } else {
                    "avc1.42001f".to_string()
                }
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H264Vaapi(_) => {
                if self.chroma.is_444() {
                    "avc1.F4001f".to_string()
                } else {
                    "avc1.640034".to_string()
                }
            }
            SurfaceEncoderKind::NvencH264(_) => "avc1.640034".to_string(),
            SurfaceEncoderKind::NvencAV1(_)
            | SurfaceEncoderKind::AV1Software(_) => {
                let profile = if self.chroma.is_444() { 2 } else { 0 };
                let level = av1_level_for(self.source_width, self.source_height);
                format!("av01.{profile}.{level}M.08")
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::AV1Vaapi(_) => {
                let profile = if self.chroma.is_444() { 2 } else { 0 };
                let level = av1_level_for(self.source_width, self.source_height);
                format!("av01.{profile}.{level}M.08")
            }
        }
    }

    pub fn codec_flag(&self) -> u8 {
        match &self.kind {
            SurfaceEncoderKind::H264Software(_) => SURFACE_FRAME_CODEC_H264,
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H264Vaapi(_) => SURFACE_FRAME_CODEC_H264,
            SurfaceEncoderKind::NvencH264(enc) | SurfaceEncoderKind::NvencAV1(enc) => {
                enc.codec_flag()
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::AV1Vaapi(_) => SURFACE_FRAME_CODEC_AV1,
            SurfaceEncoderKind::AV1Software(_) => SURFACE_FRAME_CODEC_AV1,
        }
    }

    pub fn request_keyframe(&mut self) {
        match &mut self.kind {
            SurfaceEncoderKind::H264Software(enc) => enc.request_keyframe(),
            SurfaceEncoderKind::NvencH264(enc) | SurfaceEncoderKind::NvencAV1(enc) => {
                enc.request_keyframe()
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H264Vaapi(enc) => enc.request_keyframe(),
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::AV1Vaapi(enc) => enc.request_keyframe(),
            SurfaceEncoderKind::AV1Software(enc) => enc.request_keyframe(),
        }
    }

    /// Get GBM-allocated LINEAR BGRA buffers for zero-copy compositor→encoder.
    #[cfg(target_os = "linux")]
    pub fn gbm_buffers(&self) -> &[crate::vaapi_encode::GbmExportedBuffer] {
        match &self.kind {
            SurfaceEncoderKind::H264Vaapi(enc) => enc.gbm_buffers(),
            SurfaceEncoderKind::AV1Vaapi(enc) => enc.gbm_buffers(),
            _ => &[],
        }
    }

    #[cfg(target_os = "linux")]
    pub fn gbm_nv12_buffers(&self) -> &[crate::vaapi_encode::GbmNv12Buffer] {
        match &self.kind {
            SurfaceEncoderKind::H264Vaapi(enc) => enc.gbm_nv12_buffers(),
            SurfaceEncoderKind::AV1Vaapi(enc) => enc.gbm_nv12_buffers(),
            _ => &[],
        }
    }

    #[cfg(target_os = "linux")]
    pub fn allocate_nv12_buffers(&mut self, drm_fd: std::os::fd::RawFd, count: usize) {
        match &mut self.kind {
            SurfaceEncoderKind::H264Vaapi(enc) => {
                if let Some(vpp) = &mut enc.vpp {
                    vpp.allocate_nv12_buffers(drm_fd, count);
                }
            }
            SurfaceEncoderKind::AV1Vaapi(enc) => {
                if let Some(vpp) = &mut enc.vpp {
                    vpp.allocate_nv12_buffers(drm_fd, count);
                }
            }
            _ => {}
        }
    }

    #[cfg(target_os = "linux")]
    pub fn drm_fd_raw(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        match &self.kind {
            SurfaceEncoderKind::H264Vaapi(enc) => enc._drm_fd.as_raw_fd(),
            SurfaceEncoderKind::AV1Vaapi(enc) => enc._drm_fd.as_raw_fd(),
            _ => -1,
        }
    }

    /// Get VA display pointer (as usize).
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    pub fn va_display_usize(&self) -> usize {
        match &self.kind {
            SurfaceEncoderKind::H264Vaapi(enc) => enc.va_display_usize(),
            SurfaceEncoderKind::AV1Vaapi(enc) => enc.va_display_usize(),
            _ => 0,
        }
    }

    pub fn encode(&mut self, rgba: &[u8]) -> Option<(Vec<u8>, bool)> {
        // NVENC handles RGBA→encoder-size padding internally in pinned
        // GPU memory, so pass the original un-padded buffer with source
        // dimensions.  The generic padding below produces enc_w stride
        // which would cause a diagonal-skew artefact when
        // encode_rgba_padded re-interprets it at src_w stride.
        if let SurfaceEncoderKind::NvencH264(enc) | SurfaceEncoderKind::NvencAV1(enc) =
            &mut self.kind
        {
            let (sw, sh) = (self.source_width as usize, self.source_height as usize);
            let mut result = enc.encode_rgba_padded(rgba, sw, sh);
            self.fixup_keyframe(&mut result);
            return result;
        }

        let enc_len = expected_rgba_len(self.width, self.height);
        let enc_len = match enc_len {
            Some(v) => v,
            None => {
                eprintln!(
                    "[surface-encoder] expected_rgba_len overflow {}x{}",
                    self.width, self.height
                );
                return None;
            }
        };
        let rgba = if rgba.len() == enc_len {
            std::borrow::Cow::Borrowed(rgba)
        } else {
            // The source buffer may be smaller when the original surface had
            // odd dimensions (H.264 rounds up to even).  Pad with edge-pixel
            // duplication.
            let total_px = rgba.len() / 4;
            if total_px == 0 {
                return None;
            }
            // Infer source width: try self.width, then self.width - 1
            let src_w = [self.width as usize, (self.width - 1) as usize]
                .into_iter()
                .find(|&w| w > 0 && total_px.is_multiple_of(w))?;
            let src_h = total_px / src_w;
            if src_h == 0 {
                return None;
            }
            let dst_w = self.width as usize;
            let dst_h = self.height as usize;
            let mut padded = vec![0u8; enc_len];
            for row in 0..dst_h {
                let src_row = row.min(src_h - 1);
                for col in 0..dst_w {
                    let src_col = col.min(src_w - 1);
                    let si = (src_row * src_w + src_col) * 4;
                    let di = (row * dst_w + col) * 4;
                    padded[di..di + 4].copy_from_slice(&rgba[si..si + 4]);
                }
            }
            std::borrow::Cow::Owned(padded)
        };

        match &mut self.kind {
            SurfaceEncoderKind::H264Software(encoder) => {
                encoder.encode(&rgba, self.width, self.height)
            }
            // NVENC early-returned above.
            SurfaceEncoderKind::NvencH264(_) | SurfaceEncoderKind::NvencAV1(_) => unreachable!(),
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H264Vaapi(enc) => {
                let mut bgra = rgba.into_owned();
                for px in bgra.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                let (sw, sh) = (self.source_width as usize, self.source_height as usize);
                enc.encode_bgra_padded(&bgra, sw, sh)
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::AV1Vaapi(enc) => {
                let mut bgra = rgba.into_owned();
                for px in bgra.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                let (sw, sh) = (self.source_width as usize, self.source_height as usize);
                enc.encode_bgra_padded(&bgra, sw, sh)
            }
            SurfaceEncoderKind::AV1Software(encoder) => encoder.encode(&rgba),
        }
    }

    /// Encode a frame from native pixel data (BGRA, NV12, RGBA, or DMA-BUF).
    /// Dispatches to the most efficient path for each format.
    pub fn encode_pixels(&mut self, pixels: &PixelData) -> Option<(Vec<u8>, bool)> {
        match pixels {
            PixelData::Nv12 {
                data,
                y_stride,
                uv_stride,
            } => self.encode_nv12(data, *y_stride, *uv_stride),
            PixelData::Bgra(bgra) => self.encode_bgra(bgra),
            PixelData::Rgba(rgba) => self.encode(rgba),
            #[cfg(target_os = "linux")]
            PixelData::DmaBuf {
                fd,
                fourcc,
                modifier,
                stride,
                offset,
                ..
            } => self
                .encode_dmabuf(fd, *fourcc, *modifier, *stride, *offset)
                .or_else(|| {
                    // DMA-BUF import failed (e.g. VAAPI can't import Vulkan
                    // stride).  Fall back to CPU mmap + BGRA encode.
                    let w = self.width;
                    let h = self.height;
                    let rgba = pixels.to_rgba(w, h);
                    if !rgba.is_empty() {
                        self.encode(&rgba)
                    } else {
                        None
                    }
                }),
            #[cfg(not(target_os = "linux"))]
            PixelData::DmaBuf { .. } => None,
            #[cfg(target_os = "linux")]
            PixelData::Nv12DmaBuf {
                fd,
                stride,
                uv_offset,
                width,
                height,
                sync_fd,
            } => {
                // If the compositor exported a sync_fd (tiled NV12 on radv),
                // wait for the GPU to finish the BGRA→NV12 compute before
                // reading.  This runs in spawn_blocking so blocking is fine.
                if let Some(sfd) = sync_fd {
                    use std::os::fd::AsRawFd;
                    let mut pfd = libc::pollfd {
                        fd: sfd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    unsafe { libc::poll(&mut pfd, 1, 5000) };
                }
                self.encode_nv12_dmabuf(fd, *stride, *uv_offset, *width, *height)
            }
            .or_else(|| {
                // VA surface lookup failed — mmap the DMA-BUF and
                // fall back to encode_nv12 (upload path).
                use std::os::fd::AsRawFd;
                let h = *height as usize;
                let s = *stride as usize;
                let uv_off = *uv_offset as usize;
                let raw = fd.as_raw_fd();
                let map_size = uv_off + s * h.div_ceil(2);
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        map_size,
                        libc::PROT_READ,
                        libc::MAP_SHARED,
                        raw,
                        0,
                    )
                };
                if ptr == libc::MAP_FAILED || ptr.is_null() {
                    return None;
                }
                let data = unsafe { std::slice::from_raw_parts(ptr as *const u8, map_size) };
                let result = self.encode_nv12(data, s, s);
                unsafe { libc::munmap(ptr, map_size) };
                result
            }),
            #[cfg(not(target_os = "linux"))]
            PixelData::Nv12DmaBuf { .. } => None,
            PixelData::VaSurface { .. } => None,
            // Vulkan Video pre-encoded — should be handled before reaching
            // SurfaceEncoder.  If it gets here, we can't re-encode.
            PixelData::Encoded { .. } => None,
        }
    }

    /// Encode from a VA-API-allocated NV12 surface (zero-copy).
    /// The compute shader wrote NV12 into the exported DMA-BUF; we look up
    /// the owning VA surface by inode and encode directly — no PRIME import.
    #[cfg(target_os = "linux")]
    fn encode_nv12_dmabuf(
        &mut self,
        fd: &std::sync::Arc<std::os::fd::OwnedFd>,
        _stride: u32,
        _uv_offset: u32,
        _width: u32,
        _height: u32,
    ) -> Option<(Vec<u8>, bool)> {
        use std::os::fd::AsRawFd;
        let raw_fd = fd.as_raw_fd();
        let find_surface = |nv12s: &[crate::vaapi_encode::GbmNv12Buffer]| -> Option<u32> {
            let buf = nv12s.iter().find(|n| n.fd.as_raw_fd() == raw_fd)?;
            // va_surface==0 means GBM fallback — no direct encode, use mmap.
            if buf.va_surface == 0 {
                return None;
            }
            Some(buf.va_surface)
        };
        let mut result = match &mut self.kind {
            SurfaceEncoderKind::AV1Vaapi(enc) => {
                let surf = find_surface(enc.gbm_nv12_buffers())?;
                enc.encode_surface(surf)
            }
            SurfaceEncoderKind::H264Vaapi(enc) => {
                let surf = find_surface(enc.gbm_nv12_buffers())?;
                enc.encode_surface(surf)
            }
            _ => None,
        };
        self.fixup_keyframe(&mut result);
        result
    }

    /// Encode from a DMA-BUF fd — tries zero-copy GPU import first,
    /// falls back to CPU mmap readback if no GPU path is available.
    #[cfg(target_os = "linux")]
    fn encode_dmabuf(
        &mut self,
        fd: &std::os::fd::OwnedFd,
        fourcc: u32,
        modifier: u64,
        stride: u32,
        offset: u32,
    ) -> Option<(Vec<u8>, bool)> {
        use std::os::fd::AsRawFd;

        // The encoder's source dimensions match the DMA-BUF dimensions
        // (both come from last_pixels).
        let src_w = self.source_width;
        let src_h = self.source_height;

        // --- Zero-copy GPU path (NVENC CUDA import) ---
        // VA-API encode uses the Nv12DmaBuf path instead (compute shader
        // writes NV12 into VA-API-exported surfaces, no PRIME import).
        let mut gpu_result = match &mut self.kind {
            SurfaceEncoderKind::NvencH264(enc) | SurfaceEncoderKind::NvencAV1(enc) => enc
                .encode_dmabuf_fd(
                    fd.as_raw_fd(),
                    fourcc,
                    modifier,
                    stride,
                    offset,
                    src_w,
                    src_h,
                ),
            _ => None,
        };
        if gpu_result.is_some() {
            self.fixup_keyframe(&mut gpu_result);
            return gpu_result;
        }

        // --- CPU readback fallback ---
        // Only reached if zero-copy failed (VPP unavailable, or non-VA-API encoder).
        // The GBM BO is created with GBM_BO_USE_LINEAR so mmap reads
        // pixels in the correct linear layout.
        self.encode_dmabuf_cpu_fallback(fd, fourcc, stride, offset)
    }

    /// CPU-side fallback for DMA-BUF encoding: mmap the fd, read pixels,
    /// and encode through the normal BGRA/NV12 path.
    #[cfg(target_os = "linux")]
    fn encode_dmabuf_cpu_fallback(
        &mut self,
        fd: &std::os::fd::OwnedFd,
        fourcc: u32,
        stride: u32,
        _offset: u32,
    ) -> Option<(Vec<u8>, bool)> {
        use std::os::fd::AsRawFd;

        let w = self.source_width as usize;
        let h = self.source_height as usize;
        let stride = stride as usize;
        let raw_fd = fd.as_raw_fd();

        // Determine total mmap size from fd (seek to end).
        let file_size = unsafe { libc::lseek(raw_fd, 0, libc::SEEK_END) };
        if file_size <= 0 {
            return None;
        }
        let map_len = file_size as usize;

        // DMA-BUF sync: start read
        #[repr(C)]
        struct DmaBufSync {
            flags: u64,
        }
        const DMA_BUF_SYNC_READ: u64 = 1;
        const DMA_BUF_SYNC_START: u64 = 0;
        const DMA_BUF_SYNC_END: u64 = 4;
        // ioctl number for DMA_BUF_IOCTL_SYNC — use c_ulong and cast at
        // call sites so this works on both x86_64 (ioctl takes c_ulong)
        // and aarch64 (ioctl takes c_int).
        const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x40086200;

        // Use poll() to check if the DMA-BUF fence is ready before
        // attempting sync.  Anonymous /dmabuf: fds from Vulkan WSI may
        // have implicit GPU fences that block indefinitely on SYNC_START.
        {
            let mut pfd = libc::pollfd {
                fd: raw_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
            if ready <= 0 {
                // Not ready — skip sync, accept possible tearing.
            } else {
                let sync_start = DmaBufSync {
                    flags: DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ,
                };
                unsafe {
                    libc::ioctl(raw_fd, DMA_BUF_IOCTL_SYNC as _, &sync_start);
                }
            }
        }

        // mmap the DMA-BUF for reading.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                raw_fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            let sync_end = DmaBufSync {
                flags: DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ,
            };
            unsafe {
                libc::ioctl(raw_fd, DMA_BUF_IOCTL_SYNC as _, &sync_end);
            }
            return None;
        }
        let plane_data = unsafe { std::slice::from_raw_parts(ptr as *const u8, map_len) };

        // Detect OpenGL FBO-backed DMA-BUFs (anonymous, not /dev/dri/).
        // These have bottom-up row order and must be flipped.
        let is_gl_fbo = {
            let mut link = [0u8; 128];
            let path = format!("/proc/self/fd/{raw_fd}\0");
            let n = unsafe {
                libc::readlink(path.as_ptr() as *const _, link.as_mut_ptr() as *mut _, 127)
            };
            !(n > 0 && link[..n as usize].starts_with(b"/dev/dri/"))
        };

        let result = if fourcc == blit_compositor::drm_fourcc::ARGB8888
            || fourcc == blit_compositor::drm_fourcc::XRGB8888
        {
            // BGRA in memory.
            let mut packed = Vec::with_capacity(w * h * 4);
            for i in 0..h {
                // Flip row order for GL FBO buffers.
                let row = if is_gl_fbo { h - 1 - i } else { i };
                let start = row * stride;
                let end = start + w * 4;
                if end <= plane_data.len() {
                    packed.extend_from_slice(&plane_data[start..end]);
                }
            }
            self.encode_bgra(&packed)
        } else if fourcc == blit_compositor::drm_fourcc::ABGR8888
            || fourcc == blit_compositor::drm_fourcc::XBGR8888
        {
            // RGBA in memory.
            let mut packed = Vec::with_capacity(w * h * 4);
            for i in 0..h {
                let row = if is_gl_fbo { h - 1 - i } else { i };
                let start = row * stride;
                let end = start + w * 4;
                if end <= plane_data.len() {
                    packed.extend_from_slice(&plane_data[start..end]);
                }
            }
            self.encode(&packed)
        } else if fourcc == blit_compositor::drm_fourcc::NV12 {
            // NV12: Y plane at offset 0 with `stride` pitch, UV plane
            // immediately following at y_size offset with the same pitch.
            // For linear single-fd NV12 DMA-BUFs both planes are contiguous.
            let uv_stride = stride; // UV stride matches Y stride for linear NV12
            let y_size = stride * h;
            let uv_h = h.div_ceil(2);
            let uv_size = uv_stride * uv_h;
            if map_len >= y_size + uv_size {
                // Pack Y rows then UV rows tightly (strip stride padding).
                let out_stride = w;
                let mut data = vec![0u8; out_stride * h + out_stride * uv_h];
                for row in 0..h {
                    let src = row * stride;
                    let dst = row * out_stride;
                    if src + w <= plane_data.len() {
                        data[dst..dst + w].copy_from_slice(&plane_data[src..src + w]);
                    }
                }
                let uv_dst_base = out_stride * h;
                for row in 0..uv_h {
                    let src = y_size + row * uv_stride;
                    let dst = uv_dst_base + row * out_stride;
                    if src + w <= plane_data.len() {
                        data[dst..dst + w].copy_from_slice(&plane_data[src..src + w]);
                    }
                }
                self.encode_nv12(&data, out_stride, out_stride)
            } else {
                None
            }
        } else {
            None
        };

        // Unmap and end sync.
        unsafe {
            libc::munmap(ptr, map_len);
        }
        // Only sync end if we did sync start (non-blocking check).
        let sync_end = DmaBufSync {
            flags: DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ,
        };
        unsafe {
            libc::ioctl(raw_fd, DMA_BUF_IOCTL_SYNC as _, &sync_end);
        }

        result
    }

    /// Hardware encoders (NVENC, VA-API) may report the wrong picture type
    /// due to struct layout mismatches.  Re-detect from the bitstream as a
    /// cheap safety net.  This is applied to every encode path so that RGBA,
    /// BGRA, NV12, and DMA-BUF frames all get the same keyframe fixup.
    fn fixup_keyframe(&self, result: &mut Option<(Vec<u8>, bool)>) {
        if let Some((data, is_key)) = result.as_mut()
            && !*is_key
        {
            *is_key = match &self.kind {
                SurfaceEncoderKind::NvencH264(_) => h264_stream_contains_idr(data),
                SurfaceEncoderKind::NvencAV1(_) => av1_stream_contains_keyframe(data),
                #[cfg(target_os = "linux")]
                SurfaceEncoderKind::H264Vaapi(_) => h264_stream_contains_idr(data),
                #[cfg(target_os = "linux")]
                SurfaceEncoderKind::AV1Vaapi(_) => av1_stream_contains_keyframe(data),
                _ => false,
            };
        }
    }

    /// Encode from BGRA pixels — converts directly to YUV, skipping RGBA.
    fn encode_bgra(&mut self, bgra: &[u8]) -> Option<(Vec<u8>, bool)> {
        let enc_w = self.width as usize;
        let enc_h = self.height as usize;
        let src_w = self.source_width as usize;
        let src_h = self.source_height as usize;

        let mut result = match &mut self.kind {
            SurfaceEncoderKind::H264Software(encoder) => {
                let yuv = bgra_to_yuv420_padded(bgra, src_w, src_h, enc_w, enc_h);
                let yuv_buf = YUVBuffer::from_vec(yuv, enc_w, enc_h);
                encoder.encode_yuv(&yuv_buf, self.width, self.height)
            }
            SurfaceEncoderKind::NvencH264(enc) | SurfaceEncoderKind::NvencAV1(enc) => {
                enc.encode_bgra_padded(bgra, src_w, src_h)
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H264Vaapi(enc) => enc.encode_bgra_padded(bgra, src_w, src_h),
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::AV1Vaapi(enc) => enc.encode_bgra_padded(bgra, src_w, src_h),
            SurfaceEncoderKind::AV1Software(encoder) => {
                let yuv = if self.chroma.is_444() {
                    bgra_to_yuv444_padded(bgra, src_w, src_h, enc_w, enc_h)
                } else {
                    bgra_to_yuv420_padded(bgra, src_w, src_h, enc_w, enc_h)
                };
                encoder.encode_yuv_planes(&yuv)
            }
        };
        self.fixup_keyframe(&mut result);
        result
    }

    /// Encode from NV12 data — zero colorspace conversion for VA-API/NVENC,
    /// and only a deinterleave for software encoders.
    fn encode_nv12(
        &mut self,
        data: &[u8],
        y_stride: usize,
        uv_stride: usize,
    ) -> Option<(Vec<u8>, bool)> {
        // NV12 data was captured at source dimensions.
        let src_w = self.source_width as usize;
        let src_h = self.source_height as usize;

        let mut result = match &mut self.kind {
            SurfaceEncoderKind::H264Software(encoder) => {
                let enc_w = self.width as usize;
                let enc_h = self.height as usize;
                if enc_w == src_w && enc_h == src_h {
                    let yuv = nv12_to_yuv420(data, y_stride, uv_stride, src_w, src_h);
                    let yuv_buf = YUVBuffer::from_vec(yuv, enc_w, enc_h);
                    encoder.encode_yuv(&yuv_buf, self.width, self.height)
                } else {
                    let pd = PixelData::Nv12 {
                        data: std::sync::Arc::new(data.to_vec()),
                        y_stride,
                        uv_stride,
                    };
                    let rgba = pd.to_rgba(self.source_width, self.source_height);
                    return self.encode(&rgba);
                }
            }
            SurfaceEncoderKind::NvencH264(enc) | SurfaceEncoderKind::NvencAV1(enc) => {
                // NVENC accepts NV12 natively — upload directly, no conversion.
                enc.encode_nv12(data, y_stride, uv_stride, src_h)
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H264Vaapi(enc) => {
                let uv_offset = y_stride * src_h;
                let y_data = &data[..uv_offset];
                let uv_data = &data[uv_offset..];
                enc.encode_nv12(y_data, uv_data, y_stride, uv_stride)
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::AV1Vaapi(enc) => {
                let uv_offset = y_stride * src_h;
                let y_data = &data[..uv_offset];
                let uv_data = &data[uv_offset..];
                enc.encode_nv12(y_data, uv_data, y_stride, uv_stride)
            }
            SurfaceEncoderKind::AV1Software(encoder) => {
                encoder.encode_nv12(data, y_stride, uv_stride, src_w, src_h)
            }
        };
        self.fixup_keyframe(&mut result);
        result
    }
}

fn validate_surface_dimensions(
    width: u32,
    height: u32,
    _preference: SurfaceEncoderPreference,
) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("surface encoder requires non-zero dimensions".into());
    }
    // Odd dimensions are fine — H.264 constructors pad to even internally,
    // and AV1/rav1e handles odd dimensions natively.
    let _ = expected_rgba_len(width, height)
        .ok_or_else(|| format!("surface encoder dimensions overflow for {width}x{height}"))?;
    Ok(())
}

fn expected_rgba_len(width: u32, height: u32) -> Option<usize> {
    (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)
}

// ---------------------------------------------------------------------------
// Per-pixel math — #[inline(always)] so LLVM sees through the call in the
// hot loop and auto-vectorises the surrounding code.
// ---------------------------------------------------------------------------

#[inline(always)]
fn rgb_to_y(r: i32, g: i32, b: i32) -> u8 {
    ((66 * r + 129 * g + 25 * b + 128) >> 8)
        .wrapping_add(16)
        .clamp(0, 255) as u8
}

#[inline(always)]
fn rgb_to_u(r: i32, g: i32, b: i32) -> u8 {
    ((-38 * r - 74 * g + 112 * b + 128) >> 8)
        .wrapping_add(128)
        .clamp(0, 255) as u8
}

#[inline(always)]
fn rgb_to_v(r: i32, g: i32, b: i32) -> u8 {
    ((112 * r - 94 * g - 18 * b + 128) >> 8)
        .wrapping_add(128)
        .clamp(0, 255) as u8
}

// ---------------------------------------------------------------------------
// Bulk colorspace helpers — written for auto-vectorisation: flat pre-allocated
// output, direct indexing, no branches, no extend_from_slice.
// ---------------------------------------------------------------------------

/// Flat Y-plane pass over packed 4-byte pixels.  `pixel_r/g/b` closures
/// extract R, G, B from the pixel at byte offset `i` (always a multiple of 4).
/// This is shared between RGBA, BGRA, and any other 4-byte packed format.
#[inline(always)]
fn compute_y_plane(
    src: &[u8],
    width: usize,
    height: usize,
    y_plane: &mut [u8],
    r_off: usize,
    g_off: usize,
    b_off: usize,
) {
    let total = width * height;
    for (px, y_out) in y_plane[..total].iter_mut().enumerate() {
        let i = px * 4;
        let r = src[i + r_off] as i32;
        let g = src[i + g_off] as i32;
        let b = src[i + b_off] as i32;
        *y_out = rgb_to_y(r, g, b);
    }
}

/// Flat chroma pass (2x2 subsampling) over packed 4-byte pixels.
#[inline(always)]
fn compute_uv_planes(
    src: &[u8],
    width: usize,
    height: usize,
    u_plane: &mut [u8],
    v_plane: &mut [u8],
    r_off: usize,
    g_off: usize,
    b_off: usize,
) {
    let chroma_w = width.div_ceil(2);
    let chroma_h = height.div_ceil(2);
    for cy in 0..chroma_h {
        for cx in 0..chroma_w {
            let row = cy * 2;
            let col = cx * 2;
            // Average 2x2 block, clamping to source bounds for odd dims.
            let mut u_sum = 0i32;
            let mut v_sum = 0i32;
            for dy in 0..2u32 {
                for dx in 0..2u32 {
                    let sr = (row + dy as usize).min(height - 1);
                    let sc = (col + dx as usize).min(width - 1);
                    let i = (sr * width + sc) * 4;
                    let r = src[i + r_off] as i32;
                    let g = src[i + g_off] as i32;
                    let b = src[i + b_off] as i32;
                    u_sum += rgb_to_u(r, g, b) as i32;
                    v_sum += rgb_to_v(r, g, b) as i32;
                }
            }
            let idx = cy * chroma_w + cx;
            u_plane[idx] = (u_sum / 4) as u8;
            v_plane[idx] = (v_sum / 4) as u8;
        }
    }
}

/// Padded Y-plane: produces `enc_w × enc_h` luma samples from a
/// `src_w × src_h` packed-pixel source, clamping coordinates to source bounds.
#[inline(always)]
fn compute_y_plane_padded(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    enc_w: usize,
    enc_h: usize,
    y_plane: &mut [u8],
    r_off: usize,
    g_off: usize,
    b_off: usize,
) {
    for row in 0..enc_h {
        let sr = row.min(src_h - 1);
        for col in 0..enc_w {
            let sc = col.min(src_w - 1);
            let i = (sr * src_w + sc) * 4;
            let r = src[i + r_off] as i32;
            let g = src[i + g_off] as i32;
            let b = src[i + b_off] as i32;
            y_plane[row * enc_w + col] = rgb_to_y(r, g, b);
        }
    }
}

/// Padded chroma planes: produces `ceil(enc_w/2) × ceil(enc_h/2)` chroma
/// samples with edge-pixel duplication for pixels beyond `src_w × src_h`.
#[inline(always)]
fn compute_uv_planes_padded(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    enc_w: usize,
    enc_h: usize,
    u_plane: &mut [u8],
    v_plane: &mut [u8],
    r_off: usize,
    g_off: usize,
    b_off: usize,
) {
    let chroma_w = enc_w.div_ceil(2);
    let chroma_h = enc_h.div_ceil(2);
    for cy in 0..chroma_h {
        for cx in 0..chroma_w {
            let row = cy * 2;
            let col = cx * 2;
            let mut u_sum = 0i32;
            let mut v_sum = 0i32;
            for dy in 0..2u32 {
                for dx in 0..2u32 {
                    let sr = (row + dy as usize).min(src_h - 1);
                    let sc = (col + dx as usize).min(src_w - 1);
                    let i = (sr * src_w + sc) * 4;
                    let r = src[i + r_off] as i32;
                    let g = src[i + g_off] as i32;
                    let b = src[i + b_off] as i32;
                    u_sum += rgb_to_u(r, g, b) as i32;
                    v_sum += rgb_to_v(r, g, b) as i32;
                }
            }
            let idx = cy * chroma_w + cx;
            u_plane[idx] = (u_sum / 4) as u8;
            v_plane[idx] = (v_sum / 4) as u8;
        }
    }
}

/// Compute full-resolution chroma planes (4:4:4) from packed 4-byte pixels
/// with edge-pixel padding to encoder dimensions.
#[inline(always)]
fn compute_uv_planes_444_padded(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    enc_w: usize,
    enc_h: usize,
    u_plane: &mut [u8],
    v_plane: &mut [u8],
    r_off: usize,
    g_off: usize,
    b_off: usize,
) {
    for row in 0..enc_h {
        let sr = row.min(src_h - 1);
        for col in 0..enc_w {
            let sc = col.min(src_w - 1);
            let i = (sr * src_w + sc) * 4;
            let r = src[i + r_off] as i32;
            let g = src[i + g_off] as i32;
            let b = src[i + b_off] as i32;
            let idx = row * enc_w + col;
            u_plane[idx] = rgb_to_u(r, g, b);
            v_plane[idx] = rgb_to_v(r, g, b);
        }
    }
}

/// BGRA -> I444 (YUV 4:4:4) with edge-pixel padding to encoder dimensions.
fn bgra_to_yuv444_padded(
    bgra: &[u8],
    src_w: usize,
    src_h: usize,
    enc_w: usize,
    enc_h: usize,
) -> Vec<u8> {
    let plane_size = enc_w * enc_h;
    let mut yuv = vec![0u8; plane_size * 3];
    let (y_plane, uv) = yuv.split_at_mut(plane_size);
    let (u_plane, v_plane) = uv.split_at_mut(plane_size);
    // BGRA offsets: B=0, G=1, R=2, A=3
    compute_y_plane_padded(bgra, src_w, src_h, enc_w, enc_h, y_plane, 2, 1, 0);
    compute_uv_planes_444_padded(bgra, src_w, src_h, enc_w, enc_h, u_plane, v_plane, 2, 1, 0);
    yuv
}

/// RGBA -> I444 (YUV 4:4:4).
fn rgba_to_yuv444(rgba: &[u8], width: usize, height: usize) -> Vec<u8> {
    let plane_size = width * height;
    let mut yuv = vec![0u8; plane_size * 3];
    let (y_plane, uv) = yuv.split_at_mut(plane_size);
    let (u_plane, v_plane) = uv.split_at_mut(plane_size);
    // RGBA offsets: R=0, G=1, B=2, A=3
    compute_y_plane(rgba, width, height, y_plane, 0, 1, 2);
    compute_uv_planes_444_padded(
        rgba, width, height, width, height, u_plane, v_plane, 0, 1, 2,
    );
    yuv
}

/// BGRA -> I420 with edge-pixel padding to encoder dimensions.
/// `src_w × src_h` is the actual pixel count in `bgra`.
/// `enc_w × enc_h` is the encoder output dimensions (>= src).
fn bgra_to_yuv420_padded(
    bgra: &[u8],
    src_w: usize,
    src_h: usize,
    enc_w: usize,
    enc_h: usize,
) -> Vec<u8> {
    let y_size = enc_w * enc_h;
    // Use div_ceil to match encode_yuv_planes (rav1e) which expects
    // ceil(w/2) × ceil(h/2) chroma planes.  Truncating division produces
    // a short buffer when enc_w or enc_h is odd (AV1Software doesn't pad),
    // causing a panic in encode_yuv_planes's slice indexing.
    let uv_w = enc_w.div_ceil(2);
    let uv_size = uv_w * enc_h.div_ceil(2);
    let mut yuv = vec![0u8; y_size + uv_size * 2];
    let (y_plane, uv) = yuv.split_at_mut(y_size);
    let (u_plane, v_plane) = uv.split_at_mut(uv_size);
    // BGRA offsets: B=0, G=1, R=2, A=3
    compute_y_plane_padded(bgra, src_w, src_h, enc_w, enc_h, y_plane, 2, 1, 0);
    compute_uv_planes_padded(bgra, src_w, src_h, enc_w, enc_h, u_plane, v_plane, 2, 1, 0);
    yuv
}

/// RGBA -> I420 (Y + U + V planar).
fn rgba_to_yuv420(rgba: &[u8], width: usize, height: usize) -> Vec<u8> {
    let y_size = width * height;
    let uv_w = width.div_ceil(2);
    let uv_size = uv_w * height.div_ceil(2);
    let mut yuv = vec![0u8; y_size + uv_size * 2];
    let (y_plane, uv) = yuv.split_at_mut(y_size);
    let (u_plane, v_plane) = uv.split_at_mut(uv_size);
    // RGBA offsets: R=0, G=1, B=2, A=3
    compute_y_plane(rgba, width, height, y_plane, 0, 1, 2);
    compute_uv_planes(rgba, width, height, u_plane, v_plane, 0, 1, 2);
    yuv
}

/// NV12 -> I420: Y plane memcpy + UV deinterleave.
/// Input: contiguous buffer with Y at data[..y_stride*height],
///        UV at data[y_stride*height..].
fn nv12_to_yuv420(
    data: &[u8],
    y_stride: usize,
    uv_stride: usize,
    width: usize,
    height: usize,
) -> Vec<u8> {
    let y_size = width * height;
    let uv_w = width.div_ceil(2);
    let uv_h = height.div_ceil(2);
    let uv_size = uv_w * uv_h;
    let mut yuv = vec![0u8; y_size + uv_size * 2];
    let (y_out, uv_out) = yuv.split_at_mut(y_size);
    let (u_out, v_out) = uv_out.split_at_mut(uv_size);

    let uv_offset = y_stride * height;

    // Copy Y plane (strip stride padding)
    for row in 0..height {
        let src = row * y_stride;
        let dst = row * width;
        y_out[dst..dst + width].copy_from_slice(&data[src..src + width]);
    }

    // Deinterleave UV -> separate U, V.
    // uv_w may be one more than the source has (odd width), so clamp
    // to the number of pairs actually present in each source row.
    let src_uv_pairs = width / 2;
    for row in 0..uv_h {
        let src_start = uv_offset + row.min(height / 2 - 1) * uv_stride;
        let dst_start = row * uv_w;
        for col in 0..uv_w {
            let sc = col.min(src_uv_pairs.saturating_sub(1));
            u_out[dst_start + col] = data[src_start + sc * 2];
            v_out[dst_start + col] = data[src_start + sc * 2 + 1];
        }
    }

    yuv
}

/// Scan an Annex B H.264 bitstream for an IDR NAL unit (type 5).
fn h264_stream_contains_idr(data: &[u8]) -> bool {
    annex_b_contains_nal(data, |byte| (byte & 0x1f) == 5)
}

/// Walk Annex B start codes and return true if any NAL's first byte satisfies `pred`.
fn annex_b_contains_nal(data: &[u8], pred: impl Fn(u8) -> bool) -> bool {
    let mut i = 0usize;
    while i < data.len() {
        let start_code_len = if data[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };

        let nal_header = i + start_code_len;
        if let Some(&byte) = data.get(nal_header)
            && pred(byte)
        {
            return true;
        }

        i = nal_header.saturating_add(1);
    }

    false
}

/// Check whether an AV1 OBU bitstream contains a sequence header, which
/// NVENC emits only for key frames.  This mirrors `h264_stream_contains_idr`
/// as a cheap bitstream-level safety net.
///
/// NVENC typically prepends a temporal delimiter OBU (type 2) before the
/// sequence header, so we must walk the OBU chain rather than only checking
/// the first byte.
fn av1_stream_contains_keyframe(data: &[u8]) -> bool {
    // OBU header byte: forbidden(1) | obu_type(4) | extension(1) | has_size(1) | reserved(1)
    // OBU types: 1 = SEQUENCE_HEADER, 2 = TEMPORAL_DELIMITER, 3 = FRAME_HEADER,
    //            6 = FRAME (header + tile data).
    let mut pos = 0;
    while pos < data.len() {
        let header = data[pos];
        let obu_type = (header >> 3) & 0xF;
        let has_extension = (header >> 2) & 1;
        let has_size = (header >> 1) & 1;
        pos += 1;

        // Skip optional extension byte.
        if has_extension != 0 {
            if pos >= data.len() {
                break;
            }
            pos += 1;
        }

        // OBU_SEQUENCE_HEADER → this is a key frame.
        if obu_type == 1 {
            return true;
        }

        // If has_size is set, read the LEB128-encoded payload size and
        // skip past the OBU payload to inspect the next OBU.
        if has_size != 0 {
            let mut size: u64 = 0;
            let mut shift = 0u32;
            while pos < data.len() {
                let byte = data[pos];
                pos += 1;
                size |= ((byte & 0x7F) as u64) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
                if shift >= 56 {
                    return false; // malformed LEB128
                }
            }
            pos = pos.saturating_add(size as usize);
        } else {
            // No size field — the rest of the buffer is this OBU's payload;
            // we can't skip past it to find subsequent OBUs.
            break;
        }
    }
    false
}

struct SoftwareH264Encoder {
    encoder: OpenH264Encoder,
}

impl SoftwareH264Encoder {
    fn new(quality: SurfaceQuality) -> Result<Self, String> {
        use openh264::encoder::{EncoderConfig, RateControlMode};
        let config = EncoderConfig::new()
            .set_bitrate_bps(quality.openh264_bitrate())
            .rate_control_mode(RateControlMode::Bitrate);
        let encoder =
            OpenH264Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
                .map_err(|err| format!("failed to create OpenH264 encoder: {err:?}"))?;
        Ok(Self { encoder })
    }

    fn request_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    fn encode(&mut self, rgba: &[u8], width: u32, height: u32) -> Option<(Vec<u8>, bool)> {
        let yuv = rgba_to_yuv420(rgba, width as usize, height as usize);
        let yuv_buf = YUVBuffer::from_vec(yuv, width as usize, height as usize);
        self.encode_yuv(&yuv_buf, width, height)
    }

    /// Encode from a pre-built YUV buffer (avoids redundant conversion).
    fn encode_yuv(
        &mut self,
        yuv_buf: &YUVBuffer,
        width: u32,
        height: u32,
    ) -> Option<(Vec<u8>, bool)> {
        let bitstream = match self.encoder.encode(yuv_buf) {
            Ok(bs) => bs,
            Err(e) => {
                eprintln!("[surface-encoder] openh264 encode failed {width}x{height}: {e:?}");
                return None;
            }
        };
        let nal_data = bitstream.to_vec();
        if nal_data.is_empty() {
            eprintln!("[surface-encoder] openh264 produced empty NAL {width}x{height}");
            return None;
        }
        let is_keyframe = h264_stream_contains_idr(&nal_data);
        Some((nal_data, is_keyframe))
    }
}

// ---------------------------------------------------------------------------
// AV1 (rav1e)
// ---------------------------------------------------------------------------

struct SoftwareAV1Encoder {
    ctx: rav1e::Context<u8>,
    width: usize,
    height: usize,
    force_keyframe: bool,
    chroma: ChromaSubsampling,
}

impl SoftwareAV1Encoder {
    fn new(
        width: u32,
        height: u32,
        quality: SurfaceQuality,
        chroma: ChromaSubsampling,
    ) -> Result<Self, String> {
        use rav1e::prelude::*;

        let chroma_sampling = if chroma.is_444() {
            ChromaSampling::Cs444
        } else {
            ChromaSampling::Cs420
        };
        let mut speed = SpeedSettings::from_preset(quality.av1_speed());
        speed.rdo_lookahead_frames = 1;
        let enc = EncoderConfig {
            width: width as usize,
            height: height as usize,
            chroma_sampling,
            chroma_sample_position: ChromaSamplePosition::Unknown,
            speed_settings: speed,
            low_latency: true,
            min_key_frame_interval: 0,
            max_key_frame_interval: 60,
            quantizer: quality.av1_quantizer(),
            min_quantizer: quality.av1_min_quantizer(),
            bitrate: 0,
            ..Default::default()
        };
        let cfg = Config::new().with_encoder_config(enc);
        let ctx = cfg
            .new_context()
            .map_err(|e| format!("rav1e context creation failed: {e}"))?;
        Ok(Self {
            ctx,
            width: width as usize,
            height: height as usize,
            force_keyframe: false,
            chroma,
        })
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn encode(&mut self, rgba: &[u8]) -> Option<(Vec<u8>, bool)> {
        let yuv = if self.chroma.is_444() {
            rgba_to_yuv444(rgba, self.width, self.height)
        } else {
            rgba_to_yuv420(rgba, self.width, self.height)
        };
        self.encode_yuv_planes(&yuv)
    }

    fn encode_nv12(
        &mut self,
        data: &[u8],
        y_stride: usize,
        uv_stride: usize,
        width: usize,
        height: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let yuv = nv12_to_yuv420(data, y_stride, uv_stride, width, height);
        self.encode_yuv_planes(&yuv)
    }

    /// Encode from pre-converted planar YUV data (Y + U + V contiguous).
    /// Layout depends on chroma: I420 (half-res UV) or I444 (full-res UV).
    fn encode_yuv_planes(&mut self, yuv: &[u8]) -> Option<(Vec<u8>, bool)> {
        let width = self.width;
        let height = self.height;
        let y_size = width * height;
        let (uv_w, uv_size) = if self.chroma.is_444() {
            (width, width * height)
        } else {
            let uv_w = width.div_ceil(2);
            let uv_h = height.div_ceil(2);
            (uv_w, uv_w * uv_h)
        };

        let y_plane = &yuv[..y_size];
        let u_plane = &yuv[y_size..y_size + uv_size];
        let v_plane = &yuv[y_size + uv_size..];

        let mut frame = self.ctx.new_frame();
        frame.planes[0].copy_from_raw_u8(y_plane, width, 1);
        frame.planes[1].copy_from_raw_u8(u_plane, uv_w, 1);
        frame.planes[2].copy_from_raw_u8(v_plane, uv_w, 1);

        self.send_and_receive(frame)
    }

    fn send_and_receive(&mut self, frame: rav1e::Frame<u8>) -> Option<(Vec<u8>, bool)> {
        use rav1e::prelude::*;

        if self.force_keyframe {
            let params = FrameParameters {
                frame_type_override: FrameTypeOverride::Key,
                ..Default::default()
            };
            if self.ctx.send_frame((frame, params)).is_ok() {
                self.force_keyframe = false;
            }
        } else {
            let _ = self.ctx.send_frame(frame);
        }

        match self.ctx.receive_packet() {
            Ok(packet) => {
                let is_key = packet.frame_type == rav1e::prelude::FrameType::KEY;
                Some((packet.data, is_key))
            }
            Err(rav1e::EncoderStatus::Encoded) | Err(rav1e::EncoderStatus::NeedMoreData) => None,
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal AV1 OBU with the given type, has_size=1.
    fn make_obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        // header: forbidden=0, obu_type(4), extension=0, has_size=1, reserved=0
        let header = (obu_type & 0xF) << 3 | 0b10; // has_size=1
        let mut obu = vec![header];
        // LEB128-encode the payload length.
        let mut size = payload.len();
        loop {
            let mut byte = (size & 0x7F) as u8;
            size >>= 7;
            if size > 0 {
                byte |= 0x80;
            }
            obu.push(byte);
            if size == 0 {
                break;
            }
        }
        obu.extend_from_slice(payload);
        obu
    }

    #[test]
    fn av1_keyframe_with_sequence_header_only() {
        // Sequence header OBU (type 1) as the only OBU — keyframe.
        let data = make_obu(1, &[0xAA; 10]);
        assert!(av1_stream_contains_keyframe(&data));
    }

    #[test]
    fn av1_keyframe_with_temporal_delimiter_prefix() {
        // Temporal delimiter (type 2) + sequence header (type 1) — keyframe.
        // This is the typical NVENC output for a keyframe.
        let mut data = make_obu(2, &[]); // temporal delimiter, empty payload
        data.extend(make_obu(1, &[0xBB; 8])); // sequence header
        data.extend(make_obu(6, &[0xCC; 20])); // frame OBU
        assert!(av1_stream_contains_keyframe(&data));
    }

    #[test]
    fn av1_non_keyframe_with_temporal_delimiter() {
        // Temporal delimiter (type 2) + frame (type 6) — not a keyframe.
        let mut data = make_obu(2, &[]);
        data.extend(make_obu(6, &[0xDD; 15]));
        assert!(!av1_stream_contains_keyframe(&data));
    }

    #[test]
    fn av1_non_keyframe_frame_header_only() {
        // Frame header (type 3) — not a keyframe.
        let data = make_obu(3, &[0xEE; 5]);
        assert!(!av1_stream_contains_keyframe(&data));
    }

    #[test]
    fn av1_empty_stream() {
        assert!(!av1_stream_contains_keyframe(&[]));
    }

    #[test]
    fn av1_keyframe_large_leb128_size() {
        // Temporal delimiter with a larger payload needing multi-byte LEB128,
        // followed by a sequence header.
        let mut data = make_obu(2, &[0x00; 200]);
        data.extend(make_obu(1, &[0xFF; 4]));
        assert!(av1_stream_contains_keyframe(&data));
    }
}
