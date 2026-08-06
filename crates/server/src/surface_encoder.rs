#![allow(clippy::too_many_arguments)]

use blit_compositor::PixelData;
use blit_remote::{
    CODEC_SUPPORT_AV1, CODEC_SUPPORT_AV1_444, CODEC_SUPPORT_H264, CODEC_SUPPORT_H264_444,
    SURFACE_FRAME_CODEC_AV1, SURFACE_FRAME_CODEC_H264,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
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

/// H.264 dimension cap: 3840x2160 horizontal or 2160x3840 vertical.
/// Shared by all H.264 backends; also bounds software-encode CPU cost.
///
/// Not an H.264 limit — the levels reach 8192x4320 — but the ceiling
/// browser hardware decoders clear reliably.
const H264_MAX_WIDTH: u16 = 3840;
const H264_MAX_HEIGHT: u16 = 2160;

/// Hardware AV1 cap: the 8192x4352 frame size AV1 levels 5.x and up admit,
/// which is also where the NVENC and VA-API AV1 engines stop.  High enough
/// that 5K and 6K panels encode at their native resolution instead of being
/// upscaled from 4K by the browser.
const AV1_HW_MAX_WIDTH: u16 = 8192;
const AV1_HW_MAX_HEIGHT: u16 = 4352;

/// Software AV1 (rav1e) cap.  rav1e imposes no dimension limit of its own,
/// but it is CPU-bound: past 4K even speed preset 10 falls far enough behind
/// that the stream stops being interactive.  Held at the H.264 ceiling so the
/// software fallback degrades into a lower frame rate rather than into a
/// surface that never finishes a frame.
const AV1_SW_MAX_WIDTH: u16 = 3840;
const AV1_SW_MAX_HEIGHT: u16 = 2160;

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

    /// Sensible default: compositor-resident before server-side, hardware
    /// before software.
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
            // Vulkan Video encodes on the compositor's own device with no
            // server-side encode at all, so it leads.  It is skipped unless
            // the surface is at native size and the client would have been
            // served 4:2:0 anyway, and a session that cannot be created — or
            // that stops producing bitstreams — falls through to the entries
            // below, so listing it first costs nothing when it does not apply.
            //
            // H.264 leads the Vulkan tier, against the AV1-first ordering used
            // below it, because `av1-vulkan` cannot yet emit a sequence header
            // and so declines every session.  Refusals are latched per encoder
            // rather than per tier, so a declining `av1-vulkan` no longer
            // disqualifies H.264 — but putting it first would still cost every
            // new subscription a decline round-trip, during which the client is
            // served by a server-side encoder and then switched.  Move it ahead
            // of `h264-vulkan` once it produces a decodable stream.
            Self::VulkanVideoH264,
            Self::VulkanVideoAV1,
            Self::NvencAV1,
            Self::NvencH264,
            Self::AV1Vaapi,
            Self::H264Vaapi,
            Self::H264Software,
            Self::AV1Software,
        ]
    }

    /// A distinct bit per Vulkan Video encoder, for latching which ones the
    /// compositor has already refused on a surface.  `0` for the server-side
    /// encoders, which are not refused this way.
    pub fn vulkan_refusal_bit(self) -> u8 {
        match self {
            Self::VulkanVideoH264 => 1 << 0,
            Self::VulkanVideoAV1 => 1 << 1,
            _ => 0,
        }
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

    /// Whether this backend can encode 4:4:4 at all, independent of what the
    /// client announced.  These are structural limits, not probe results:
    ///
    /// - `H264Vaapi`: libva has no H.264 4:4:4 encode profile — the enum stops
    ///   at `VAProfileH264High422`, so there is nothing to pass to
    ///   `vaCreateConfig`.
    /// - `H264Software`: x264 encodes 4:4:4 (High 4:4:4 Predictive); openh264
    ///   is 4:2:0-only.  A build without the x264 feature is structurally
    ///   4:2:0; a build with it stays a runtime probe, since
    ///   `BLIT_H264_SOFTWARE=openh264` can still pin the 4:2:0-only backend.
    ///
    /// `AV1Vaapi` is deliberately absent: it asks the driver for
    /// `VAProfileAV1Profile1`.  Whether a given device advertises an encode
    /// entrypoint for that profile is a runtime question, so it stays a probe
    /// and falls back to 4:2:0 when the answer is no.
    ///
    /// Checking this up front keeps the encoder chain from running a probe
    /// that can only ever fail, and from logging it as if the host were at
    /// fault.
    // The H264Software arm is a cfg!, so clippy sees per-build literals and
    // suggests a matches! that would bake in one feature combo's answer.
    #[allow(clippy::match_like_matches_macro)]
    pub fn supports_444_by_encoder(self) -> bool {
        match self {
            Self::H264Vaapi => false,
            // Vulkan Video H.264 encodes High 4:4:4 Predictive from a
            // two-plane `G8_B8R8_2PLANE_444_UNORM` source — but only where the
            // driver advertises that profile, which is a runtime question this
            // structural check cannot answer (the RTX 4090 says yes, the
            // Raphael iGPU says no).  The compositor's capability query is the
            // real gate; a refusal there falls through to a server-side
            // encoder.  AV1 through Vulkan has no 4:4:4 path at all.
            Self::VulkanVideoH264 => true,
            Self::VulkanVideoAV1 => false,
            Self::H264Software => cfg!(all(target_os = "linux", feature = "x264")),
            _ => true,
        }
    }

    /// Maximum surface dimensions this encoder can carry.
    ///
    /// Every backend has a real ceiling, and they differ by more than 2x, so
    /// callers have to say which one they mean rather than folding the whole
    /// chain into one number: see [`Self::widest_for_list`] (how large a
    /// surface may be *composited*, given that some subscriber can carry it)
    /// and [`Self::tightest_for_list`] (how large it may be *encoded* when we
    /// don't yet know which backend will win the chain).
    pub fn max_dimensions(self) -> (u16, u16) {
        match self {
            Self::H264Software | Self::H264Vaapi | Self::NvencH264 | Self::VulkanVideoH264 => {
                (H264_MAX_WIDTH, H264_MAX_HEIGHT)
            }
            Self::AV1Vaapi | Self::NvencAV1 | Self::VulkanVideoAV1 => {
                (AV1_HW_MAX_WIDTH, AV1_HW_MAX_HEIGHT)
            }
            Self::AV1Software => (AV1_SW_MAX_WIDTH, AV1_SW_MAX_HEIGHT),
        }
    }

    /// Whether this encoder can carry a `width`x`height` frame.
    pub fn fits(self, width: u32, height: u32) -> bool {
        let (max_w, max_h) = self.max_dimensions();
        width <= max_w as u32 && height <= max_h as u32
    }

    /// Tightest cap across a list of preferences — the size that is safe no
    /// matter which one wins the fallback chain.  `None` for an empty list.
    pub fn tightest_for_list(prefs: &[Self]) -> Option<(u16, u16)> {
        prefs
            .iter()
            .map(|p| p.max_dimensions())
            .reduce(|(aw, ah), (bw, bh)| (aw.min(bw), ah.min(bh)))
    }

    /// Loosest cap across a list of preferences — the size the most capable
    /// of them could carry.  `None` for an empty list.
    pub fn widest_for_list(prefs: &[Self]) -> Option<(u16, u16)> {
        prefs
            .iter()
            .map(|p| p.max_dimensions())
            .reduce(|(aw, ah), (bw, bh)| (aw.max(bw), ah.max(bh)))
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
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

/// AV1 `seq_profile` digit for the WebCodecs codec string, at 8-bit depth.
///
/// 8-bit 4:4:4 is Profile 1 ("High"), not Profile 2 ("Professional") —
/// Profile 2 covers 4:2:2 at 8/10-bit and only reaches 4:4:4 at 12-bit.
/// This has to agree with what the encoders actually emit: rav1e derives
/// `seq_profile = 1` for 8-bit 4:4:4, and the VA-API AV1 encoder writes 1
/// into both its sequence header and `VAEncSequenceParameterBufferAV1`.
/// Advertising 2 here would hand the client's `VideoDecoder` a profile that
/// contradicts the bitstream.
pub fn av1_profile_digit(chroma: ChromaSubsampling) -> u8 {
    if chroma.is_444() { 1 } else { 0 }
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

/// The two independent axes of video encoding: how many bits a surface may
/// spend, and how much CPU/GPU time the encoder may spend producing them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SurfaceEncoding {
    pub bandwidth: SurfaceBandwidth,
    pub speed: SurfaceSpeed,
}

/// Video bandwidth preset.  Higher bandwidth means a sharper picture for more
/// bits; it says nothing about how long the encoder may take.
///
/// - **Low**: quantizer 180 — visibly lossy
/// - **Medium** (default): quantizer 120 — good balance
/// - **High**: quantizer 80 — sharp
/// - **Ultra**: quantizer 1 — maximum fidelity, very expensive
/// - **Custom**: caller-specified AV1 quantizer (10–255)
///
/// Set via `BLIT_SURFACE_BANDWIDTH=low|medium|high|ultra|<10–255>`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SurfaceBandwidth {
    Low,
    #[default]
    Medium,
    High,
    Ultra,
    /// Caller-specified AV1 quantizer (10–255).  H.264 QP and
    /// software-encoder bitrate are derived proportionally.
    Custom {
        quantizer: u8,
    },
}

/// Encoder speed preset.  Faster means less CPU/GPU time per frame and worse
/// compression at the same bandwidth; it does not change the bit budget.
///
/// - **Slow**: most compression per bit, heavy CPU
/// - **Medium**
/// - **Fast**
/// - **Realtime** (default): the cheapest encode every backend offers
/// - **Custom**: caller-specified 10–255 (10 = slowest, 255 = fastest)
///
/// Set via `BLIT_SURFACE_SPEED=slow|medium|fast|realtime|<10–255>`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SurfaceSpeed {
    Slow,
    Medium,
    Fast,
    #[default]
    Realtime,
    /// Caller-specified speed (10 = slowest, 255 = fastest).
    Custom {
        speed: u8,
    },
}

impl SurfaceBandwidth {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "ultra" => Some(Self::Ultra),
            _ => match value.parse::<u8>() {
                Ok(q @ 10..=255) => Some(Self::Custom { quantizer: q }),
                _ => None,
            },
        }
    }

    /// Decode from the wire `bandwidth` byte in C2S_SURFACE_SUBSCRIBE.
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

    /// AV1 quantizer (0 = best, 255 = worst).
    /// Also used as VA-API `base_qindex` and NVENC AV1 QP, and as the
    /// canonical scale the adaptive controller walks.
    pub fn av1_quantizer(self) -> usize {
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

    /// Software H.264 target bitrate in bits/sec.  Resolution-independent
    /// approximation — the backends' rate control adapts internally.
    #[cfg(all(target_os = "linux", any(feature = "x264", feature = "openh264")))]
    fn h264_bitrate(self) -> u32 {
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

impl SurfaceSpeed {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "slow" => Some(Self::Slow),
            "medium" => Some(Self::Medium),
            "fast" => Some(Self::Fast),
            "realtime" => Some(Self::Realtime),
            _ => match value.parse::<u8>() {
                Ok(s @ 10..=255) => Some(Self::Custom { speed: s }),
                _ => None,
            },
        }
    }

    /// Decode from the wire `speed` byte in C2S_SURFACE_SUBSCRIBE.
    ///
    /// - 0 → `None` (server default)
    /// - 1–4 → named presets
    /// - 10–255 → `Custom { speed: value }`
    /// - 5–9 → reserved, treated as server default
    pub fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Slow),
            2 => Some(Self::Medium),
            3 => Some(Self::Fast),
            4 => Some(Self::Realtime),
            v @ 10..=255 => Some(Self::Custom { speed: v }),
            _ => None,
        }
    }

    /// Normalized effort level, 0 = slowest/best compression, 10 = fastest.
    /// Every backend's speed control is derived from this one number.
    pub fn level(self) -> u8 {
        match self {
            Self::Slow => 4,
            Self::Medium => 6,
            Self::Fast => 8,
            Self::Realtime => 10,
            Self::Custom { speed } => (speed.saturating_sub(10) as u32 * 10 / 245) as u8,
        }
    }

    /// rav1e speed preset (0 = slowest/best, 10 = fastest/worst).
    fn av1_speed(self) -> u8 {
        self.level()
    }

    /// x264 preset name.  `ultrafast` is deliberately not on the ladder: it
    /// drops deblocking and CABAC, which costs more picture than the CPU it
    /// saves on top of `superfast` + `zerolatency`.
    #[cfg(all(target_os = "linux", feature = "x264"))]
    fn x264_preset(self) -> &'static std::ffi::CStr {
        match self.level() {
            9..=10 => c"superfast",
            7..=8 => c"veryfast",
            5..=6 => c"faster",
            3..=4 => c"fast",
            _ => c"medium",
        }
    }

    /// openh264 complexity mode.
    ///
    /// The `realtime` default lands on `Low`, where this backend used to be
    /// pinned to `Medium` regardless of the quality setting.  That is
    /// deliberate: every other backend's `realtime` is its fastest setting
    /// (rav1e 10, NVENC P1, VA-API 7), and openh264 is the cheapest software
    /// path, where CPU is the scarce resource.  `BLIT_SURFACE_SPEED=medium`
    /// asks for the old encode, which was not expressible before.
    #[cfg(all(target_os = "linux", feature = "openh264"))]
    fn openh264_complexity(self) -> openh264::encoder::Complexity {
        use openh264::encoder::Complexity;
        match self.level() {
            8..=10 => Complexity::Low,
            4..=7 => Complexity::Medium,
            _ => Complexity::High,
        }
    }

    /// NVENC preset index: 1 = P1 (fastest) … 7 = P7 (slowest).
    pub fn nvenc_preset(self) -> u8 {
        7 - (self.level() as u32 * 6 / 10) as u8
    }

    /// VA-API `quality_level`: 7 = fastest, 1 = slowest.  AMD's radeonsi maps
    /// this onto AMF presets (3–7 = speed, 1–2 = quality, 0 = balanced).
    pub fn vaapi_quality_level(self) -> u32 {
        1 + self.level() as u32 * 6 / 10
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
    /// Encoding this encoder is currently running at.  The bandwidth half
    /// moves at runtime (see `set_bandwidth`); speed is fixed for the
    /// encoder's lifetime because no backend can change it in place.
    encoding: SurfaceEncoding,
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
        encoding: SurfaceEncoding,
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

            // Try 4:4:4 first for this encoder if both the backend and the
            // client support it.
            if try_444
                && pref.supports_444_by_encoder()
                && pref.supports_444_by_client(codec_support)
            {
                match Self::try_one(
                    pref,
                    width,
                    height,
                    source_width,
                    source_height,
                    vaapi_device,
                    encoding,
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
                        // The 4:2:0 fallback below will overwrite last_err
                        // on failure; no need to record this one.
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
                encoding,
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
        encoding: SurfaceEncoding,
        verbose: bool,
        chroma: ChromaSubsampling,
    ) -> Result<Self, String> {
        let _ = vaapi_device;
        validate_surface_dimensions(width, height, pref)?;

        // Fast-fail on encoder families that have already been proven
        // unavailable on this host (no NVENC driver, VA-API device without
        // the requested codec, etc.).  Without this cache, every surface
        // resize walks the full preference chain and re-runs expensive
        // probes — cuInit on systems without CUDA, libva driver open on
        // systems without H.264/AV1 encode — adding multi-second latency
        // to each resize before the actual encoder is created.
        let known = family_status(pref, chroma);
        if let Some(FamilyStatus::Missing(err)) = known {
            return Err(err);
        }

        let result = Self::try_one_inner(
            pref,
            width,
            height,
            source_width,
            source_height,
            vaapi_device,
            encoding,
            verbose,
            chroma,
        );
        // `Missing` returned above, so a family that is known here is one
        // that has already built an encoder — and by elimination anything
        // that goes wrong now is about the frame, not the host.  Nothing
        // more to learn either way.
        if known.is_some() {
            return result;
        }
        match &result {
            Ok(_) => record_family(pref, chroma, FamilyStatus::Works),
            Err(err) => {
                // First word from this family, and it's bad news — so ask it
                // for a frame with nothing unusual about it before believing
                // the worst.  Only a failure that survives that is a fact
                // about the host; anything else belongs to the frame that
                // provoked it — a dock thumbnail under NVENC's minimum
                // encode height, VA-API AV1 declining to pad a 256x54 strip
                // out to 512x512 — and writing it down here would take the
                // encoder away from every other viewer on the machine.
                let (pw, ph) = PROBE_SIZE;
                let probe = Self::try_one_inner(
                    pref,
                    pw,
                    ph,
                    pw,
                    ph,
                    vaapi_device,
                    encoding,
                    false,
                    chroma,
                );
                match probe {
                    Ok(_) => record_family(pref, chroma, FamilyStatus::Works),
                    Err(probe_err) => {
                        // Report the probe's reason, not this frame's: it is
                        // the one that will be replayed at every later size.
                        if verbose && probe_err != *err {
                            eprintln!(
                                "[surface-encoder] {pref:?} unusable on this host: {probe_err}"
                            );
                        }
                        record_family(pref, chroma, FamilyStatus::Missing(probe_err));
                    }
                }
            }
        }
        result
    }

    fn try_one_inner(
        pref: SurfaceEncoderPreference,
        width: u32,
        height: u32,
        source_width: u32,
        source_height: u32,
        vaapi_device: &str,
        encoding: SurfaceEncoding,
        verbose: bool,
        chroma: ChromaSubsampling,
    ) -> Result<Self, String> {
        let _ = vaapi_device;
        match pref {
            SurfaceEncoderPreference::VulkanVideoH264
            | SurfaceEncoderPreference::VulkanVideoAV1 => {
                Err("Vulkan Video encoders are managed by the compositor".into())
            }
            SurfaceEncoderPreference::NvencH264 => {
                let (width, height) = ((width + 1) & !1, (height + 1) & !1);
                let qp = encoding.bandwidth.h264_qp() as u32;
                Ok(Self {
                    width,
                    height,
                    source_width,
                    source_height,
                    kind: SurfaceEncoderKind::NvencH264(Box::new(
                        crate::nvenc_encode::NvencDirectEncoder::try_new(
                            "h264",
                            width,
                            height,
                            qp,
                            encoding.speed.nvenc_preset(),
                            verbose,
                            chroma,
                        )?,
                    )),
                    chroma,
                    encoding,
                })
            }
            SurfaceEncoderPreference::NvencAV1 => {
                // AV1 superblocks are 64x64; NVENC requires even dimensions
                // at minimum.  Round up to a multiple of 2 (matching H.264)
                // so chroma planes stay aligned.
                let (width, height) = ((width + 1) & !1, (height + 1) & !1);
                let qp = encoding.bandwidth.nvenc_av1_qp();
                Ok(Self {
                    width,
                    height,
                    source_width,
                    source_height,
                    kind: SurfaceEncoderKind::NvencAV1(Box::new(
                        crate::nvenc_encode::NvencDirectEncoder::try_new(
                            "av1",
                            width,
                            height,
                            qp,
                            encoding.speed.nvenc_preset(),
                            verbose,
                            chroma,
                        )?,
                    )),
                    chroma,
                    encoding,
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
                            encoding.bandwidth.h264_qp(),
                            encoding.speed.vaapi_quality_level(),
                            verbose,
                            chroma,
                        )?,
                    )),
                    chroma,
                    encoding,
                })
            }
            #[cfg(not(target_os = "linux"))]
            SurfaceEncoderPreference::H264Vaapi => Err("VA-API is only available on Unix".into()),
            #[cfg(target_os = "linux")]
            SurfaceEncoderPreference::AV1Vaapi => {
                // Round up to 64-pixel superblocks.  AMD's AV1 backend
                // rejects `vaCreateContext` with VA_STATUS_ERROR_
                // RESOLUTION_NOT_SUPPORTED (0x13) below a codec-
                // specific minimum; 256 isn't enough on many chips, so
                // we default to 512.  AV1's `render_width/height` in
                // the frame header still carries the actual
                // `source_width/source_height`, so the client's
                // WebCodecs decoder crops back to the requested size.
                //
                // BUT: the encoder encodes every pixel including the
                // padding.  For small thumbnails, the padded area can
                // exceed the source by >10×, turning a bandwidth-
                // saving thumbnail into a bandwidth *amplifier*.  Bail
                // out when padding would waste more than the content
                // area — the fallback encoder chain picks a smaller-
                // friendly backend (e.g. H264Software) instead.
                const VAAPI_AV1_MIN: u32 = 512;
                let enc_w = width.div_ceil(64) * 64;
                let enc_h = height.div_ceil(64) * 64;
                let (width, height) = (enc_w.max(VAAPI_AV1_MIN), enc_h.max(VAAPI_AV1_MIN));
                let source_area = (source_width as u64) * (source_height as u64);
                let padded_area = (width as u64) * (height as u64);
                if padded_area > source_area.saturating_mul(2) {
                    return Err(format!(
                        "AV1Vaapi padding {width}x{height} > 2× source \
                         {source_width}x{source_height} — falling back",
                    ));
                }
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
                            encoding.bandwidth.av1_quantizer() as u8,
                            encoding.speed.vaapi_quality_level(),
                            verbose,
                            chroma,
                        )?,
                    )),
                    chroma,
                    encoding,
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
                    width, height, encoding, chroma,
                )?)),
                chroma,
                encoding,
            }),
            SurfaceEncoderPreference::H264Software => {
                let (width, height) = ((width + 1) & !1, (height + 1) & !1);
                Ok(Self {
                    width,
                    height,
                    source_width,
                    source_height,
                    kind: SurfaceEncoderKind::H264Software(Box::new(SoftwareH264Encoder::new(
                        width, height, encoding, chroma,
                    )?)),
                    chroma,
                    encoding,
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
    #[cfg(target_os = "linux")]
    pub fn encoder_dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Human-readable name of the active encoder backend, sent to clients
    /// for display in debug panels.  Includes chroma subsampling when 4:4:4.
    pub fn encoder_name(&self) -> &'static str {
        match (&self.kind, self.chroma) {
            (SurfaceEncoderKind::H264Software(enc), chroma) => enc.name(chroma),
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

    /// Which preference actually won the fallback chain.  Sizing consults it
    /// so a viewer that landed on AV1 is not held to the H.264 ceiling — and
    /// one that landed on H.264 is.
    pub fn preference(&self) -> SurfaceEncoderPreference {
        match &self.kind {
            SurfaceEncoderKind::H264Software(_) => SurfaceEncoderPreference::H264Software,
            SurfaceEncoderKind::NvencH264(_) => SurfaceEncoderPreference::NvencH264,
            SurfaceEncoderKind::NvencAV1(_) => SurfaceEncoderPreference::NvencAV1,
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H264Vaapi(_) => SurfaceEncoderPreference::H264Vaapi,
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::AV1Vaapi(_) => SurfaceEncoderPreference::AV1Vaapi,
            SurfaceEncoderKind::AV1Software(_) => SurfaceEncoderPreference::AV1Software,
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
            SurfaceEncoderKind::NvencAV1(_) | SurfaceEncoderKind::AV1Software(_) => {
                let level = av1_level_for(self.source_width, self.source_height);
                format!("av01.{}.{level}M.08", av1_profile_digit(self.chroma))
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::AV1Vaapi(_) => {
                let level = av1_level_for(self.source_width, self.source_height);
                format!("av01.{}.{level}M.08", av1_profile_digit(self.chroma))
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

    /// The encoding this encoder is running at.
    pub fn encoding(&self) -> SurfaceEncoding {
        self.encoding
    }

    /// Move the bandwidth this encoder targets without rebuilding it.
    ///
    /// Returns `false` when the backend cannot change rate in place, in
    /// which case the caller has to drop the encoder and build a new one —
    /// which costs a keyframe, so it is worth doing only for large steps.
    /// A rebuild is never required for a *smaller* step on these backends.
    #[must_use]
    pub fn set_bandwidth(&mut self, bandwidth: SurfaceBandwidth) -> bool {
        if self.encoding.bandwidth == bandwidth {
            return true;
        }
        let applied = match &mut self.kind {
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H264Vaapi(enc) => {
                enc.set_qp(bandwidth.h264_qp());
                true
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::AV1Vaapi(enc) => {
                enc.set_base_qindex(bandwidth.av1_quantizer().min(255) as u8);
                true
            }
            SurfaceEncoderKind::NvencH264(enc) => enc.set_qp(bandwidth.h264_qp() as u32),
            SurfaceEncoderKind::NvencAV1(enc) => enc.set_qp(bandwidth.nvenc_av1_qp()),
            SurfaceEncoderKind::H264Software(enc) => enc.set_bandwidth(bandwidth),
            // rav1e freezes quantizer/min_quantizer into the Context at
            // creation and exposes no setter.
            SurfaceEncoderKind::AV1Software(_) => false,
        };
        if applied {
            self.encoding.bandwidth = bandwidth;
        }
        applied
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
                encoder.encode(&rgba, self.width, self.height, self.chroma)
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
    ///
    /// Only the VA-API arm's import actually succeeds. The NVENC one always
    /// fails and takes the fallback; see `NvencDirectEncoder::encode_dmabuf_fd`
    /// for why. Since the fallback is silent, a working picture here says
    /// nothing about whether the GPU import ran.
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
                let yuv = if self.chroma.is_444() {
                    bgra_to_yuv444_padded(bgra, src_w, src_h, enc_w, enc_h)
                } else {
                    bgra_to_yuv420_padded(bgra, src_w, src_h, enc_w, enc_h)
                };
                encoder.encode_yuv(yuv, self.width, self.height)
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
                // NV12 chroma is half-res; a 4:4:4 encoder needs full-res
                // planes, so take the RGBA path (which upsamples) instead.
                if !self.chroma.is_444() && enc_w == src_w && enc_h == src_h {
                    let yuv = nv12_to_yuv420(data, y_stride, uv_stride, src_w, src_h);
                    encoder.encode_yuv(yuv, self.width, self.height)
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
                // NV12 chroma is half-res; a 4:4:4 encoder needs full-res
                // planes, so take the RGBA path (which upsamples) instead.
                if self.chroma.is_444() {
                    let pd = PixelData::Nv12 {
                        data: std::sync::Arc::new(data.to_vec()),
                        y_stride,
                        uv_stride,
                    };
                    let rgba = pd.to_rgba(self.source_width, self.source_height);
                    return self.encode(&rgba);
                }
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
    preference: SurfaceEncoderPreference,
) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("surface encoder requires non-zero dimensions".into());
    }
    // Odd dimensions are fine — H.264 constructors pad to even internally,
    // and AV1/rav1e handles odd dimensions natively.
    let _ = expected_rgba_len(width, height)
        .ok_or_else(|| format!("surface encoder dimensions overflow for {width}x{height}"))?;
    // Refuse a frame this backend cannot carry so the chain moves on to one
    // that can.  Without this an H.264 encoder would happily accept a 5K
    // frame and emit a bitstream above what the client's decoder advertised,
    // which fails in the browser rather than here.  This runs ahead of
    // [`FamilyStatus`], so a size rejection is never mistaken for the host
    // lacking the backend — the same one is retried at a size that fits.
    if !preference.fits(width, height) {
        let (max_w, max_h) = preference.max_dimensions();
        return Err(format!(
            "{width}x{height} exceeds the {max_w}x{max_h} ceiling for {preference:?}"
        ));
    }
    Ok(())
}

/// A frame with nothing unusual about it, used to tell a host's verdict
/// apart from a frame's.  Comfortably above every hardware minimum we know
/// of — NVENC's AV1 engine wants 128 rows — small enough that building one
/// costs nothing, and even on both axes so no backend has to pad it.
pub(crate) const PROBE_SIZE: (u32, u32) = (640, 480);

/// What the *host* has to say about an encoder family, as distinct from
/// what it has to say about one frame.
///
/// Only two things get written here, and neither can depend on the frame: a
/// construction that succeeded, and one that failed and was reproduced at
/// [`PROBE_SIZE`].  A failure the probe does not reproduce is a verdict on
/// the frame and is deliberately *not* recorded — a 256x54 dock thumbnail
/// is under NVENC's minimum encode height, and filing that under "this host
/// has no NVENC" took hardware encoding away from every viewer, at every
/// size, until the server was restarted.  The tell was a 3200x2160 request
/// being refused with a cached message about a 48x54 one.
///
/// Never evicts: drivers don't appear at runtime in practice, and the cost
/// of being wrong is only that a user must restart after plugging in GPU
/// support they didn't have.
#[derive(Clone)]
enum FamilyStatus {
    /// Built an encoder at least once.  Nothing further is ever probed —
    /// every later failure is about the frame, by elimination.
    Works,
    /// Could not build one even at `PROBE_SIZE`.  Later attempts fail fast
    /// with this message instead of re-running `cuInit` or reopening the
    /// libva driver, which is what this cache exists to avoid.
    Missing(String),
}

static ENCODER_FAMILY: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<(SurfaceEncoderPreference, ChromaSubsampling), FamilyStatus>,
    >,
> = std::sync::OnceLock::new();

fn family_map() -> &'static std::sync::Mutex<
    std::collections::HashMap<(SurfaceEncoderPreference, ChromaSubsampling), FamilyStatus>,
> {
    ENCODER_FAMILY.get_or_init(Default::default)
}

fn family_status(
    pref: SurfaceEncoderPreference,
    chroma: ChromaSubsampling,
) -> Option<FamilyStatus> {
    family_map().lock().ok()?.get(&(pref, chroma)).cloned()
}

fn record_family(pref: SurfaceEncoderPreference, chroma: ChromaSubsampling, status: FamilyStatus) {
    if let Ok(mut map) = family_map().lock() {
        map.entry((pref, chroma)).or_insert(status);
    }
}

/// Whether this host has already proven it cannot run `pref` at all.
///
/// Sizing consults this to tell "no backend here can carry a frame this
/// large" from "the one that could just failed" — the first calls for a
/// smaller surface, the second for another try at the same size.
pub fn known_unavailable(pref: SurfaceEncoderPreference, chroma: ChromaSubsampling) -> bool {
    matches!(family_status(pref, chroma), Some(FamilyStatus::Missing(_)))
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

// ---------------------------------------------------------------------------
// H.264 software (x264 / openh264)
// ---------------------------------------------------------------------------

/// Software H.264 encoding, dispatching to whichever backend was compiled
/// in.  Both backends are optional cargo features of blit-server, so a
/// binary can carry either, both, or neither.  With neither — or off Linux,
/// where the compositor that feeds surface encoders does not exist —
/// construction fails and the encoder preference list moves on.
enum SoftwareH264Encoder {
    #[cfg(all(target_os = "linux", feature = "x264"))]
    X264(X264Encoder),
    #[cfg(all(target_os = "linux", feature = "openh264"))]
    OpenH264(Box<OpenH264Encoder>),
}

impl SoftwareH264Encoder {
    /// x264 is preferred when both backends are present; openh264 is the
    /// runtime fallback.  `BLIT_H264_SOFTWARE=x264|openh264` pins one.
    /// Only x264 can encode 4:4:4 (High 4:4:4 Predictive profile).
    fn new(
        width: u32,
        height: u32,
        encoding: SurfaceEncoding,
        chroma: ChromaSubsampling,
    ) -> Result<Self, String> {
        let pinned = std::env::var("BLIT_H264_SOFTWARE").ok();
        let pinned = pinned.as_deref().map(str::trim).filter(|s| !s.is_empty());
        Self::new_with_backend(pinned, width, height, encoding, chroma)
    }

    #[allow(unused_mut)] // mutated only when a backend feature is enabled
    fn new_with_backend(
        pinned: Option<&str>,
        width: u32,
        height: u32,
        encoding: SurfaceEncoding,
        chroma: ChromaSubsampling,
    ) -> Result<Self, String> {
        let mut errors: Vec<String> = Vec::new();
        #[cfg(all(target_os = "linux", feature = "x264"))]
        if pinned.is_none_or(|p| p == "x264") {
            match X264Encoder::new(width, height, encoding, chroma) {
                Ok(enc) => return Ok(Self::X264(enc)),
                Err(err) => errors.push(format!("x264: {err}")),
            }
        }
        #[cfg(all(target_os = "linux", feature = "openh264"))]
        if pinned.is_none_or(|p| p == "openh264") {
            if chroma.is_444() {
                errors.push("openh264: 4:4:4 not supported".into());
            } else {
                match OpenH264Encoder::new(encoding) {
                    Ok(enc) => return Ok(Self::OpenH264(Box::new(enc))),
                    Err(err) => errors.push(format!("openh264: {err}")),
                }
            }
        }
        let _ = (width, height, encoding, chroma);
        if !errors.is_empty() {
            Err(errors.join("; "))
        } else if let Some(p) = pinned {
            Err(format!(
                "H.264 software backend {p:?} (BLIT_H264_SOFTWARE) is not in \
                 this build (`x264`/`openh264` cargo features)"
            ))
        } else {
            Err("no software H.264 encoder in this build \
                 (`x264`/`openh264` cargo features, Linux only)"
                .into())
        }
    }

    /// Backend-qualified name for client debug panels.
    fn name(&self, chroma: ChromaSubsampling) -> &'static str {
        let _ = chroma;
        match self {
            #[cfg(all(target_os = "linux", feature = "x264"))]
            Self::X264(_) => {
                if chroma.is_444() {
                    "h264-software (x264) 4:4:4"
                } else {
                    "h264-software (x264)"
                }
            }
            #[cfg(all(target_os = "linux", feature = "openh264"))]
            Self::OpenH264(_) => "h264-software (openh264)",
            #[cfg(not(all(target_os = "linux", any(feature = "x264", feature = "openh264"))))]
            _ => "h264-software",
        }
    }

    fn request_keyframe(&mut self) {
        match self {
            #[cfg(all(target_os = "linux", feature = "x264"))]
            Self::X264(enc) => enc.request_keyframe(),
            #[cfg(all(target_os = "linux", feature = "openh264"))]
            Self::OpenH264(enc) => enc.request_keyframe(),
            #[cfg(not(all(target_os = "linux", any(feature = "x264", feature = "openh264"))))]
            _ => {}
        }
    }

    /// Retarget the bitrate in place.  x264 accepts a live reconfigure;
    /// openh264's safe API takes its bitrate only at construction, so that
    /// backend reports failure and the caller decides whether to rebuild.
    fn set_bandwidth(&mut self, bandwidth: SurfaceBandwidth) -> bool {
        let _ = bandwidth;
        match self {
            #[cfg(all(target_os = "linux", feature = "x264"))]
            Self::X264(enc) => enc.set_bitrate_kbps((bandwidth.h264_bitrate() / 1000) as i32),
            #[cfg(all(target_os = "linux", feature = "openh264"))]
            Self::OpenH264(_) => false,
            #[cfg(not(all(target_os = "linux", any(feature = "x264", feature = "openh264"))))]
            _ => false,
        }
    }

    fn encode(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        chroma: ChromaSubsampling,
    ) -> Option<(Vec<u8>, bool)> {
        let yuv = if chroma.is_444() {
            rgba_to_yuv444(rgba, width as usize, height as usize)
        } else {
            rgba_to_yuv420(rgba, width as usize, height as usize)
        };
        self.encode_yuv(yuv, width, height)
    }

    /// Encode from a pre-built planar YUV buffer (avoids redundant
    /// conversion).  Layout must match the chroma mode the encoder was
    /// opened with: I420 (half-res UV) or I444 (full-res UV, x264 only).
    fn encode_yuv(&mut self, yuv: Vec<u8>, width: u32, height: u32) -> Option<(Vec<u8>, bool)> {
        match self {
            #[cfg(all(target_os = "linux", feature = "x264"))]
            Self::X264(enc) => enc.encode_yuv(&yuv, width, height),
            #[cfg(all(target_os = "linux", feature = "openh264"))]
            Self::OpenH264(enc) => enc.encode_yuv(yuv, width, height),
            #[cfg(not(all(target_os = "linux", any(feature = "x264", feature = "openh264"))))]
            _ => {
                let _ = (yuv, width, height);
                None
            }
        }
    }
}

/// x264 backend.  System libx264 via pkg-config; GPL-2.0-or-later, which
/// is why it is a build-time choice (see `blit --license`).
#[cfg(all(target_os = "linux", feature = "x264"))]
struct X264Encoder {
    enc: *mut x264_sys::x264_t,
    pts: i64,
    force_keyframe: bool,
    is_444: bool,
}

// SAFETY: the x264 handle is not tied to the thread that created it and is
// only accessed through &mut self.
#[cfg(all(target_os = "linux", feature = "x264"))]
unsafe impl Send for X264Encoder {}

#[cfg(all(target_os = "linux", feature = "x264"))]
impl X264Encoder {
    fn new(
        width: u32,
        height: u32,
        encoding: SurfaceEncoding,
        chroma: ChromaSubsampling,
    ) -> Result<Self, String> {
        use x264_sys::*;
        let is_444 = chroma.is_444();
        unsafe {
            let mut par: x264_param_t = std::mem::zeroed();
            // zerolatency: no B-frames, no lookahead, one frame in, one out.
            let preset = encoding.speed.x264_preset();
            if x264_param_default_preset(&mut par, preset.as_ptr(), c"zerolatency".as_ptr()) < 0 {
                return Err("x264_param_default_preset failed".into());
            }
            par.i_csp = if is_444 { X264_CSP_I444 } else { X264_CSP_I420 } as i32;
            par.i_width = width as i32;
            par.i_height = height as i32;
            // Predictable per-surface CPU cost (zerolatency would otherwise
            // enable sliced threads across all cores).
            par.i_threads = 1;
            par.i_fps_num = 30;
            par.i_fps_den = 1;
            // Same periodic-keyframe cadence as the AV1 software encoder.
            par.i_keyint_max = 60;
            par.i_log_level = X264_LOG_NONE;
            par.rc.i_rc_method = X264_RC_ABR as i32;
            par.rc.i_bitrate = (encoding.bandwidth.h264_bitrate() / 1000) as i32; // kbit/s
            // Profiles match the codec strings sent to clients: Constrained
            // Baseline (avc1.4200) for 4:2:0, High 4:4:4 Predictive
            // (avc1.F400) for 4:4:4 — the only H.264 profile with 4:4:4.
            let profile = if is_444 { c"high444" } else { c"baseline" };
            if x264_param_apply_profile(&mut par, profile.as_ptr()) < 0 {
                return Err("x264_param_apply_profile failed".into());
            }
            let enc = x264_encoder_open(&mut par);
            if enc.is_null() {
                return Err(format!("x264_encoder_open failed for {width}x{height}"));
            }
            Ok(Self {
                enc,
                pts: 0,
                force_keyframe: false,
                is_444,
            })
        }
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    /// Move the ABR target bitrate on the live encoder.  Takes effect on the
    /// next frame with no keyframe; `i_rc_method` is untouched (x264 refuses
    /// to change it).  VBV stays disabled, so the "reconfiguring VBV
    /// generates invalid HRD" caveat in x264.h does not apply here.
    fn set_bitrate_kbps(&mut self, kbps: i32) -> bool {
        use x264_sys::*;
        unsafe {
            // Read back the encoder's *current* params: x264_encoder_open
            // consumed and rewrote the ones passed at construction.
            let mut par: x264_param_t = std::mem::zeroed();
            x264_encoder_parameters(self.enc, &mut par);
            par.rc.i_bitrate = kbps.max(1);
            x264_encoder_reconfig(self.enc, &mut par) == 0
        }
    }

    fn encode_yuv(&mut self, yuv: &[u8], width: u32, height: u32) -> Option<(Vec<u8>, bool)> {
        use x264_sys::*;
        let w = width as usize;
        let h = height as usize;
        let y_len = w * h;
        let (c_len, c_stride) = if self.is_444 {
            (w * h, w)
        } else {
            ((w / 2) * (h / 2), w / 2)
        };
        if yuv.len() < y_len + 2 * c_len {
            eprintln!("[surface-encoder] x264 short YUV buffer {width}x{height}");
            return None;
        }
        unsafe {
            let mut pic_in: x264_picture_t = std::mem::zeroed();
            x264_picture_init(&mut pic_in);
            pic_in.img.i_csp = if self.is_444 {
                X264_CSP_I444
            } else {
                X264_CSP_I420
            } as i32;
            pic_in.img.i_plane = 3;
            // x264 reads but never writes the input planes.
            let base = yuv.as_ptr() as *mut u8;
            pic_in.img.plane[0] = base;
            pic_in.img.plane[1] = base.add(y_len);
            pic_in.img.plane[2] = base.add(y_len + c_len);
            pic_in.img.i_stride[0] = w as i32;
            pic_in.img.i_stride[1] = c_stride as i32;
            pic_in.img.i_stride[2] = c_stride as i32;
            pic_in.i_pts = self.pts;
            self.pts += 1;
            pic_in.i_type = if self.force_keyframe {
                X264_TYPE_IDR as i32
            } else {
                X264_TYPE_AUTO as i32
            };

            let mut nals: *mut x264_nal_t = std::ptr::null_mut();
            let mut num_nals = 0;
            let mut pic_out: x264_picture_t = std::mem::zeroed();
            let size = x264_encoder_encode(
                self.enc,
                &mut nals,
                &mut num_nals,
                &mut pic_in,
                &mut pic_out,
            );
            if size < 0 {
                eprintln!("[surface-encoder] x264 encode failed {width}x{height}");
                return None;
            }
            if size == 0 || num_nals == 0 {
                eprintln!("[surface-encoder] x264 produced no output {width}x{height}");
                return None;
            }
            self.force_keyframe = false;
            // NAL payloads are contiguous in memory starting at the first
            // one; `size` is their total length in bytes.
            let data = std::slice::from_raw_parts((*nals).p_payload, size as usize).to_vec();
            Some((data, pic_out.b_keyframe != 0))
        }
    }
}

#[cfg(all(target_os = "linux", feature = "x264"))]
impl Drop for X264Encoder {
    fn drop(&mut self) {
        unsafe { x264_sys::x264_encoder_close(self.enc) };
    }
}

/// openh264 backend.  BSD-2-Clause, compiled from bundled source with no
/// system dependency.
#[cfg(all(target_os = "linux", feature = "openh264"))]
struct OpenH264Encoder {
    encoder: openh264::encoder::Encoder,
}

#[cfg(all(target_os = "linux", feature = "openh264"))]
impl OpenH264Encoder {
    fn new(encoding: SurfaceEncoding) -> Result<Self, String> {
        use openh264::encoder::{BitRate, Encoder, EncoderConfig, RateControlMode};
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(encoding.bandwidth.h264_bitrate()))
            .rate_control_mode(RateControlMode::Bitrate)
            .complexity(encoding.speed.openh264_complexity());
        let encoder = Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
            .map_err(|err| format!("failed to create encoder: {err:?}"))?;
        Ok(Self { encoder })
    }

    fn request_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    fn encode_yuv(&mut self, yuv: Vec<u8>, width: u32, height: u32) -> Option<(Vec<u8>, bool)> {
        let yuv_buf = openh264::formats::YUVBuffer::from_vec(yuv, width as usize, height as usize);
        let bitstream = match self.encoder.encode(&yuv_buf) {
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
        encoding: SurfaceEncoding,
        chroma: ChromaSubsampling,
    ) -> Result<Self, String> {
        use rav1e::prelude::*;

        let chroma_sampling = if chroma.is_444() {
            ChromaSampling::Cs444
        } else {
            ChromaSampling::Cs420
        };
        let mut speed = SpeedSettings::from_preset(encoding.speed.av1_speed());
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
            quantizer: encoding.bandwidth.av1_quantizer(),
            min_quantizer: encoding.bandwidth.av1_min_quantizer(),
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

    #[test]
    fn encoder_ceilings_differ_by_family() {
        // The two folds have to disagree for the split to mean anything.
        let prefs = SurfaceEncoderPreference::defaults();
        assert_eq!(
            SurfaceEncoderPreference::tightest_for_list(&prefs),
            Some((H264_MAX_WIDTH, H264_MAX_HEIGHT))
        );
        assert_eq!(
            SurfaceEncoderPreference::widest_for_list(&prefs),
            Some((AV1_HW_MAX_WIDTH, AV1_HW_MAX_HEIGHT))
        );
        assert_eq!(SurfaceEncoderPreference::tightest_for_list(&[]), None);
        assert_eq!(SurfaceEncoderPreference::widest_for_list(&[]), None);
    }

    #[test]
    fn hardware_av1_carries_5k_and_h264_does_not() {
        assert!(SurfaceEncoderPreference::AV1Vaapi.fits(5120, 2880));
        assert!(SurfaceEncoderPreference::NvencAV1.fits(5120, 2880));
        assert!(!SurfaceEncoderPreference::H264Vaapi.fits(5120, 2880));
        // Software AV1 has no dimension limit of its own but is CPU-bound,
        // so it is held at the H.264 ceiling deliberately.
        assert!(!SurfaceEncoderPreference::AV1Software.fits(5120, 2880));
        assert!(SurfaceEncoderPreference::AV1Software.fits(3840, 2160));
    }

    /// A backend that cannot carry the frame must be refused here, so the
    /// chain moves on to one that can instead of emitting a bitstream the
    /// client's decoder will reject.
    #[test]
    fn oversized_frames_are_refused_per_backend() {
        assert!(
            validate_surface_dimensions(5120, 2880, SurfaceEncoderPreference::H264Software)
                .is_err()
        );
        assert!(
            validate_surface_dimensions(5120, 2880, SurfaceEncoderPreference::AV1Vaapi).is_ok()
        );
        assert!(
            validate_surface_dimensions(3840, 2160, SurfaceEncoderPreference::H264Software).is_ok()
        );
        // Still rejects the pre-existing cases.
        assert!(validate_surface_dimensions(0, 100, SurfaceEncoderPreference::AV1Vaapi).is_err());
    }

    /// 5K at 60 fps needs a level above the 4K one, and the string is what
    /// the client hands to `VideoDecoder`.
    #[test]
    fn av1_level_climbs_past_4k() {
        assert_eq!(av1_level_for(3840, 2160), "13");
        assert_eq!(av1_level_for(5120, 2880), "16");
    }

    #[test]
    fn bandwidth_and_speed_parse_independently() {
        assert_eq!(
            SurfaceBandwidth::parse("ultra"),
            Some(SurfaceBandwidth::Ultra)
        );
        assert_eq!(
            SurfaceBandwidth::parse("200"),
            Some(SurfaceBandwidth::Custom { quantizer: 200 })
        );
        // "lossless" is gone, and a preset name from the other axis is not
        // silently accepted.
        assert_eq!(SurfaceBandwidth::parse("lossless"), None);
        assert_eq!(SurfaceBandwidth::parse("realtime"), None);
        assert_eq!(SurfaceBandwidth::parse("9"), None);

        assert_eq!(
            SurfaceSpeed::parse("realtime"),
            Some(SurfaceSpeed::Realtime)
        );
        assert_eq!(
            SurfaceSpeed::parse("200"),
            Some(SurfaceSpeed::Custom { speed: 200 })
        );
        assert_eq!(SurfaceSpeed::parse("ultra"), None);
    }

    #[test]
    fn wire_bytes_decode_to_presets_and_custom_ranges() {
        assert_eq!(SurfaceBandwidth::from_wire(0), None);
        assert_eq!(
            SurfaceBandwidth::from_wire(2),
            Some(SurfaceBandwidth::Medium)
        );
        assert_eq!(SurfaceBandwidth::from_wire(7), None);
        assert_eq!(
            SurfaceBandwidth::from_wire(255),
            Some(SurfaceBandwidth::Custom { quantizer: 255 })
        );

        assert_eq!(SurfaceSpeed::from_wire(0), None);
        assert_eq!(SurfaceSpeed::from_wire(3), Some(SurfaceSpeed::Fast));
        assert_eq!(SurfaceSpeed::from_wire(7), None);
        assert_eq!(
            SurfaceSpeed::from_wire(10),
            Some(SurfaceSpeed::Custom { speed: 10 })
        );
    }

    #[test]
    fn bandwidth_derivations_are_monotonic() {
        let ladder = [
            SurfaceBandwidth::Low,
            SurfaceBandwidth::Medium,
            SurfaceBandwidth::High,
            SurfaceBandwidth::Ultra,
        ];
        for pair in ladder.windows(2) {
            assert!(pair[0].av1_quantizer() > pair[1].av1_quantizer());
            assert!(pair[0].av1_min_quantizer() > pair[1].av1_min_quantizer());
            assert!(pair[0].h264_qp() > pair[1].h264_qp());
        }
    }

    #[test]
    fn speed_level_is_monotonic_and_maps_onto_every_backend() {
        assert_eq!(SurfaceSpeed::Slow.level(), 4);
        assert_eq!(SurfaceSpeed::Realtime.level(), 10);
        assert_eq!(SurfaceSpeed::Custom { speed: 10 }.level(), 0);
        assert_eq!(SurfaceSpeed::Custom { speed: 255 }.level(), 10);

        // Fastest end reproduces the values the server hard-coded before the
        // speed knob existed: rav1e speed 10, NVENC P1, VA-API quality 7.
        assert_eq!(SurfaceSpeed::Realtime.av1_speed(), 10);
        assert_eq!(SurfaceSpeed::Realtime.nvenc_preset(), 1);
        assert_eq!(SurfaceSpeed::Realtime.vaapi_quality_level(), 7);

        assert_eq!(SurfaceSpeed::Custom { speed: 10 }.nvenc_preset(), 7);
        assert_eq!(SurfaceSpeed::Custom { speed: 10 }.vaapi_quality_level(), 1);
    }

    /// 8-bit 4:4:4 AV1 is seq_profile 1 ("High"); Profile 2
    /// ("Professional") only reaches 4:4:4 at 12-bit.  The digit here has to
    /// match what rav1e and the VA-API AV1 encoder actually write into the
    /// sequence header, or the client configures `VideoDecoder` with a
    /// profile the bitstream contradicts.
    #[test]
    fn av1_444_advertises_profile_1_not_2() {
        assert_eq!(av1_profile_digit(ChromaSubsampling::Cs444), 1);
        assert_eq!(av1_profile_digit(ChromaSubsampling::Cs420), 0);
    }

    /// 4:4:4 is a structural non-starter for H.264 VA-API (and for
    /// h264-software in builds without x264), so the encoder chain must not
    /// spend a probe on them.  `AV1Vaapi` is excluded from this list on
    /// purpose — it probes for `VAProfileAV1Profile1` at runtime.
    #[test]
    fn encoders_without_any_444_path_are_skipped() {
        assert!(!SurfaceEncoderPreference::H264Vaapi.supports_444_by_encoder());
        assert_eq!(
            SurfaceEncoderPreference::H264Software.supports_444_by_encoder(),
            cfg!(all(target_os = "linux", feature = "x264")),
        );
        assert!(SurfaceEncoderPreference::AV1Vaapi.supports_444_by_encoder());
        assert!(SurfaceEncoderPreference::AV1Software.supports_444_by_encoder());
        assert!(SurfaceEncoderPreference::NvencH264.supports_444_by_encoder());
        assert!(SurfaceEncoderPreference::NvencAV1.supports_444_by_encoder());
    }

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

    #[cfg(all(target_os = "linux", feature = "x264"))]
    #[test]
    fn x264_software_encoder_round_trip() {
        // The dispatcher prefers x264, so this exercises the x264 backend.
        // (new_with_backend(None, ..) keeps the ambient BLIT_H264_SOFTWARE
        // of whoever runs the tests from interfering.)
        let chroma = ChromaSubsampling::Cs420;
        let mut enc =
            SoftwareH264Encoder::new_with_backend(None, 64, 48, SurfaceEncoding::default(), chroma)
                .unwrap();
        assert_eq!(enc.name(chroma), "h264-software (x264)");
        let rgba = vec![128u8; 64 * 48 * 4];
        let (data, key) = enc
            .encode(&rgba, 64, 48, chroma)
            .expect("first frame encodes");
        assert!(key, "first frame is a keyframe");
        assert!(h264_stream_contains_idr(&data));
        let (_, key2) = enc
            .encode(&rgba, 64, 48, chroma)
            .expect("second frame encodes");
        assert!(!key2, "steady-state frame is not a keyframe");
        enc.request_keyframe();
        let (data3, key3) = enc
            .encode(&rgba, 64, 48, chroma)
            .expect("forced keyframe encodes");
        assert!(key3, "request_keyframe forces an IDR");
        assert!(h264_stream_contains_idr(&data3));
    }

    #[cfg(all(target_os = "linux", feature = "x264"))]
    #[test]
    fn x264_software_encoder_444_round_trip() {
        let chroma = ChromaSubsampling::Cs444;
        let mut enc =
            SoftwareH264Encoder::new_with_backend(None, 64, 48, SurfaceEncoding::default(), chroma)
                .unwrap();
        assert_eq!(enc.name(chroma), "h264-software (x264) 4:4:4");
        let rgba = vec![128u8; 64 * 48 * 4];
        let (data, key) = enc
            .encode(&rgba, 64, 48, chroma)
            .expect("first frame encodes");
        assert!(key, "first frame is a keyframe");
        assert!(h264_stream_contains_idr(&data));
        let (_, key2) = enc
            .encode(&rgba, 64, 48, chroma)
            .expect("second frame encodes");
        assert!(!key2, "steady-state frame is not a keyframe");
    }

    #[cfg(all(target_os = "linux", feature = "openh264"))]
    #[test]
    fn openh264_rejects_444() {
        let err = SoftwareH264Encoder::new_with_backend(
            Some("openh264"),
            64,
            48,
            SurfaceEncoding::default(),
            ChromaSubsampling::Cs444,
        )
        .err()
        .expect("openh264 must reject 4:4:4");
        assert!(err.contains("4:4:4"), "unexpected error: {err}");
    }

    #[cfg(all(target_os = "linux", feature = "x264", feature = "openh264"))]
    #[test]
    fn h264_software_backend_pin() {
        let q = SurfaceEncoding::default();
        let chroma = ChromaSubsampling::Cs420;
        let enc =
            SoftwareH264Encoder::new_with_backend(Some("openh264"), 64, 48, q, chroma).unwrap();
        assert_eq!(enc.name(chroma), "h264-software (openh264)");
        let enc = SoftwareH264Encoder::new_with_backend(Some("x264"), 64, 48, q, chroma).unwrap();
        assert_eq!(enc.name(chroma), "h264-software (x264)");
        let enc = SoftwareH264Encoder::new_with_backend(None, 64, 48, q, chroma).unwrap();
        assert_eq!(enc.name(chroma), "h264-software (x264)", "x264 preferred");
        assert!(SoftwareH264Encoder::new_with_backend(Some("nope"), 64, 48, q, chroma).is_err());
    }

    #[cfg(all(target_os = "linux", feature = "openh264"))]
    #[test]
    fn openh264_software_encoder_round_trip() {
        let mut enc = OpenH264Encoder::new(SurfaceEncoding::default()).unwrap();
        let yuv = vec![128u8; 64 * 48 * 3 / 2];
        let (data, key) = enc
            .encode_yuv(yuv.clone(), 64, 48)
            .expect("first frame encodes");
        assert!(key, "first frame is a keyframe");
        assert!(h264_stream_contains_idr(&data));
        let (_, key2) = enc
            .encode_yuv(yuv.clone(), 64, 48)
            .expect("second frame encodes");
        assert!(!key2, "steady-state frame is not a keyframe");
        enc.request_keyframe();
        let (data3, key3) = enc
            .encode_yuv(yuv, 64, 48)
            .expect("forced keyframe encodes");
        assert!(key3, "request_keyframe forces an IDR");
        assert!(h264_stream_contains_idr(&data3));
    }

    #[test]
    fn av1_keyframe_large_leb128_size() {
        // Temporal delimiter with a larger payload needing multi-byte LEB128,
        // followed by a sequence header.
        let mut data = make_obu(2, &[0x00; 200]);
        data.extend(make_obu(1, &[0xFF; 4]));
        assert!(av1_stream_contains_keyframe(&data));
    }

    /// A backend that cannot build an encoder even at [`PROBE_SIZE`] is a
    /// fact about the host, and the chain is entitled to stop asking.
    /// Vulkan Video stands in for one: it refuses at every size because the
    /// compositor owns it, so no GPU is needed to see the latch close.
    #[test]
    fn a_backend_that_fails_the_probe_too_is_written_off() {
        let pref = SurfaceEncoderPreference::VulkanVideoH264;
        assert!(!known_unavailable(pref, ChromaSubsampling::Cs420));
        let err = SurfaceEncoder::try_one(
            pref,
            256,
            54,
            256,
            54,
            "",
            SurfaceEncoding::default(),
            false,
            ChromaSubsampling::Cs420,
        )
        .err()
        .expect("Vulkan Video is never built through this path");
        assert!(err.contains("compositor"), "{err}");
        assert!(known_unavailable(pref, ChromaSubsampling::Cs420));
    }

    /// `known_unavailable` answers for the host, so only a probe failure
    /// sets it — a family that has built an encoder stays usable no matter
    /// how many individual frames it goes on to refuse.
    #[test]
    fn only_a_missing_family_reads_as_unavailable() {
        let pref = SurfaceEncoderPreference::VulkanVideoAV1;
        let chroma = ChromaSubsampling::Cs444;
        record_family(pref, chroma, FamilyStatus::Works);
        assert!(!known_unavailable(pref, chroma));
        // First writer wins: a later failure cannot demote a family that
        // has already proven itself.
        record_family(pref, chroma, FamilyStatus::Missing("nope".into()));
        assert!(!known_unavailable(pref, chroma));
        // …and an untouched key is not a claim either way.
        assert!(!known_unavailable(pref, ChromaSubsampling::Cs420));
    }
}
