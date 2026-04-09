#![allow(clippy::too_many_arguments)]

use blit_compositor::PixelData;
#[cfg(target_os = "linux")]
use blit_remote::SURFACE_FRAME_CODEC_H265;

use blit_remote::{
    CODEC_SUPPORT_AV1, CODEC_SUPPORT_H264, CODEC_SUPPORT_H265, SURFACE_FRAME_CODEC_AV1,
    SURFACE_FRAME_CODEC_H264,
};
use openh264::encoder::Encoder as OpenH264Encoder;
use openh264::formats::YUVBuffer;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceEncoderPreference {
    H264Software,
    H264Vaapi,
    H265Vaapi,
    NvencH264,
    NvencH265,
    NvencAV1,
    AV1,
}

// Type alias for backwards compatibility in tests.
pub type SurfaceH264EncoderPreference = SurfaceEncoderPreference;

/// openh264 hard limit: 3840x2160 horizontal or 2160x3840 vertical.
const H264_MAX_WIDTH: u16 = 3840;
const H264_MAX_HEIGHT: u16 = 2160;

impl SurfaceEncoderPreference {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "h264-software" | "software" => Some(Self::H264Software),
            "h264-vaapi" | "vaapi" => Some(Self::H264Vaapi),
            "h265-vaapi" | "hevc-vaapi" => Some(Self::H265Vaapi),
            "nvenc-h264" | "h264-nvenc" => Some(Self::NvencH264),
            "nvenc-h265" | "h265-nvenc" | "nvenc-hevc" | "hevc-nvenc" => Some(Self::NvencH265),
            "nvenc-av1" | "av1-nvenc" => Some(Self::NvencAV1),
            "av1" => Some(Self::AV1),
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

    /// Sensible default: hardware before software, H.265 > H.264 > AV1.
    ///
    /// Override at runtime with `BLIT_SURFACE_ENCODERS=nvenc-h265,h264-software`
    /// (comma-separated list).
    pub fn defaults() -> Vec<Self> {
        if let Some(list) = std::env::var("BLIT_SURFACE_ENCODERS")
            .ok()
            .and_then(|v| Self::parse_list(&v).ok())
        {
            return list;
        }
        vec![
            Self::NvencH265,
            Self::H265Vaapi,
            Self::NvencAV1,
            Self::NvencH264,
            Self::H264Vaapi,
            Self::H264Software,
            Self::AV1,
        ]
    }

    /// Returns true if the given codec_support bitmask allows this encoder.
    /// A codec_support of 0 means "accept anything".
    pub fn supported_by_client(self, codec_support: u8) -> bool {
        if codec_support == 0 {
            return true;
        }
        match self {
            Self::H264Software | Self::H264Vaapi | Self::NvencH264 => {
                codec_support & CODEC_SUPPORT_H264 != 0
            }
            Self::H265Vaapi | Self::NvencH265 => codec_support & CODEC_SUPPORT_H265 != 0,
            Self::AV1 | Self::NvencAV1 => codec_support & CODEC_SUPPORT_AV1 != 0,
        }
    }

    /// Maximum surface dimensions the encoder can handle.
    /// Returns `None` if there is no practical limit.
    pub fn max_dimensions(self) -> Option<(u16, u16)> {
        match self {
            Self::H264Software | Self::H264Vaapi | Self::NvencH264 => {
                Some((H264_MAX_WIDTH, H264_MAX_HEIGHT))
            }
            Self::H265Vaapi | Self::NvencH265 | Self::NvencAV1 | Self::AV1 => None,
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
}

/// Video quality preset.  Higher quality uses more CPU.
///
/// - **Low**: speed 10, quantizer 180 — minimal CPU, visibly lossy
/// - **Medium** (default): speed 10, quantizer 120 — good balance
/// - **High**: speed 8, quantizer 80 — sharp, noticeable CPU use
/// - **Lossless-ish**: speed 6, quantizer 40 — near-lossless, heavy CPU
///
/// Set via `BLIT_SURFACE_QUALITY=low|medium|high|lossless`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SurfaceQuality {
    Low,
    #[default]
    Medium,
    High,
    Lossless,
}

impl SurfaceQuality {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "lossless" => Some(Self::Lossless),
            _ => None,
        }
    }

    /// rav1e speed preset (0 = slowest/best, 10 = fastest/worst).
    fn av1_speed(self) -> u8 {
        match self {
            Self::Low => 10,
            Self::Medium => 10,
            Self::High => 8,
            Self::Lossless => 6,
        }
    }

    /// rav1e quantizer (0 = lossless, 255 = worst).
    fn av1_quantizer(self) -> usize {
        match self {
            Self::Low => 180,
            Self::Medium => 120,
            Self::High => 80,
            Self::Lossless => 40,
        }
    }

    /// rav1e min_quantizer.
    fn av1_min_quantizer(self) -> u8 {
        match self {
            Self::Low => 120,
            Self::Medium => 80,
            Self::High => 40,
            Self::Lossless => 0,
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
}

enum SurfaceEncoderKind {
    H264Software(Box<SoftwareH264Encoder>),
    NvencH264(Box<crate::nvenc_encode::NvencDirectEncoder>),
    NvencH265(Box<crate::nvenc_encode::NvencDirectEncoder>),
    NvencAV1(Box<crate::nvenc_encode::NvencDirectEncoder>),
    #[cfg(target_os = "linux")]
    H264Vaapi(Box<crate::vaapi_encode::VaapiDirectEncoder>),
    #[cfg(target_os = "linux")]
    H265Vaapi(Box<crate::vaapi_encode::VaapiHevcEncoder>),
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
    ) -> Result<Self, String> {
        let source_width = width;
        let source_height = height;
        let mut last_err = String::from("no encoders configured");

        for &pref in preferences {
            if !pref.supported_by_client(codec_support) {
                continue;
            }
            match Self::try_one(
                pref,
                width,
                height,
                source_width,
                source_height,
                vaapi_device,
                quality,
            ) {
                Ok(enc) => {
                    if verbose {
                        eprintln!(
                            "[surface-encoder] using {:?} for {source_width}x{source_height}",
                            pref
                        );
                    }
                    return Ok(enc);
                }
                Err(err) => {
                    if verbose {
                        eprintln!(
                            "[surface-encoder] {:?} unavailable for {source_width}x{source_height}: {err}",
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
    ) -> Result<Self, String> {
        let _ = vaapi_device;
        validate_surface_dimensions(width, height, pref)?;

        match pref {
            SurfaceEncoderPreference::NvencH264 => {
                let (width, height) = ((width + 1) & !1, (height + 1) & !1);
                Ok(Self {
                    width,
                    height,
                    source_width,
                    source_height,
                    kind: SurfaceEncoderKind::NvencH264(Box::new(
                        crate::nvenc_encode::NvencDirectEncoder::try_new("h264", width, height)?,
                    )),
                })
            }
            SurfaceEncoderPreference::NvencH265 => {
                let (width, height) = ((width + 1) & !1, (height + 1) & !1);
                Ok(Self {
                    width,
                    height,
                    source_width,
                    source_height,
                    kind: SurfaceEncoderKind::NvencH265(Box::new(
                        crate::nvenc_encode::NvencDirectEncoder::try_new("h265", width, height)?,
                    )),
                })
            }
            SurfaceEncoderPreference::NvencAV1 => Ok(Self {
                width,
                height,
                source_width,
                source_height,
                kind: SurfaceEncoderKind::NvencAV1(Box::new(
                    crate::nvenc_encode::NvencDirectEncoder::try_new("av1", width, height)?,
                )),
            }),
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
                        )?,
                    )),
                })
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderPreference::H265Vaapi => {
                let (width, height) = ((width + 1) & !1, (height + 1) & !1);
                Ok(Self {
                    width,
                    height,
                    source_width,
                    source_height,
                    kind: SurfaceEncoderKind::H265Vaapi(Box::new(
                        crate::vaapi_encode::VaapiHevcEncoder::try_new(
                            width,
                            height,
                            vaapi_device,
                        )?,
                    )),
                })
            }
            #[cfg(not(target_os = "linux"))]
            SurfaceEncoderPreference::H264Vaapi | SurfaceEncoderPreference::H265Vaapi => {
                Err("VA-API is only available on Unix".into())
            }
            SurfaceEncoderPreference::AV1 => Ok(Self {
                width,
                height,
                source_width,
                source_height,
                kind: SurfaceEncoderKind::AV1Software(Box::new(SoftwareAV1Encoder::new(
                    width, height, quality,
                )?)),
            }),
            SurfaceEncoderPreference::H264Software => {
                let (width, height) = ((width + 1) & !1, (height + 1) & !1);
                Ok(Self {
                    width,
                    height,
                    source_width,
                    source_height,
                    kind: SurfaceEncoderKind::H264Software(Box::new(SoftwareH264Encoder::new()?)),
                })
            }
        }
    }

    #[allow(dead_code)]
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The original surface dimensions before any encoder padding.
    pub fn source_dimensions(&self) -> (u32, u32) {
        (self.source_width, self.source_height)
    }

    #[allow(dead_code)]
    pub fn kind_name(&self) -> &'static str {
        match &self.kind {
            SurfaceEncoderKind::H264Software(_) => "h264-software",
            SurfaceEncoderKind::NvencH264(_) => "nvenc-h264",
            SurfaceEncoderKind::NvencH265(_) => "nvenc-h265",
            SurfaceEncoderKind::NvencAV1(_) => "nvenc-av1",
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H264Vaapi(_) => "h264-vaapi",
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H265Vaapi(_) => "h265-vaapi",
            SurfaceEncoderKind::AV1Software(_) => "av1-software",
        }
    }

    pub fn codec_flag(&self) -> u8 {
        match &self.kind {
            SurfaceEncoderKind::H264Software(_) => SURFACE_FRAME_CODEC_H264,
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H264Vaapi(_) => SURFACE_FRAME_CODEC_H264,
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H265Vaapi(_) => SURFACE_FRAME_CODEC_H265,
            SurfaceEncoderKind::NvencH264(enc)
            | SurfaceEncoderKind::NvencH265(enc)
            | SurfaceEncoderKind::NvencAV1(enc) => enc.codec_flag(),
            SurfaceEncoderKind::AV1Software(_) => SURFACE_FRAME_CODEC_AV1,
        }
    }

    pub fn request_keyframe(&mut self) {
        match &mut self.kind {
            SurfaceEncoderKind::H264Software(enc) => enc.request_keyframe(),
            SurfaceEncoderKind::NvencH264(enc)
            | SurfaceEncoderKind::NvencH265(enc)
            | SurfaceEncoderKind::NvencAV1(enc) => enc.request_keyframe(),
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H264Vaapi(enc) => enc.request_keyframe(),
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H265Vaapi(enc) => enc.request_keyframe(),
            SurfaceEncoderKind::AV1Software(enc) => enc.request_keyframe(),
        }
    }

    pub fn encode(&mut self, rgba: &[u8]) -> Option<(Vec<u8>, bool)> {
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
            SurfaceEncoderKind::NvencH264(enc)
            | SurfaceEncoderKind::NvencH265(enc)
            | SurfaceEncoderKind::NvencAV1(enc) => {
                let mut bgra = rgba.into_owned();
                for px in bgra.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                enc.encode_bgra(&bgra)
            }
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
            SurfaceEncoderKind::H265Vaapi(enc) => {
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
            } => self.encode_dmabuf(fd, *fourcc, *modifier, *stride, *offset),
            #[cfg(not(target_os = "linux"))]
            PixelData::DmaBuf { .. } => None,
        }
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

        // --- Zero-copy GPU path (VA-API VPP) ---
        // Import the DMA-BUF directly into a VASurface via PRIME_2, convert
        // BGRA→NV12 on the GPU via VPP, then encode.  No CPU mmap needed.
        match &mut self.kind {
            SurfaceEncoderKind::H264Vaapi(enc) => {
                if let Some(result) = enc.encode_dmabuf_fd(
                    fd.as_raw_fd(),
                    fourcc,
                    modifier,
                    stride,
                    offset,
                    src_w,
                    src_h,
                ) {
                    return Some(result);
                }
            }
            SurfaceEncoderKind::H265Vaapi(enc) => {
                if let Some(result) = enc.encode_dmabuf_fd(
                    fd.as_raw_fd(),
                    fourcc,
                    modifier,
                    stride,
                    offset,
                    src_w,
                    src_h,
                ) {
                    return Some(result);
                }
            }
            _ => {}
        }

        // --- CPU readback fallback ---
        // Only reached if zero-copy failed (VPP unavailable, or non-VA-API encoder).
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

        let sync_start = DmaBufSync {
            flags: DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ,
        };
        unsafe {
            libc::ioctl(raw_fd, DMA_BUF_IOCTL_SYNC as _, &sync_start);
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

        let result = if fourcc == blit_compositor::drm_fourcc::ARGB8888
            || fourcc == blit_compositor::drm_fourcc::XRGB8888
        {
            // BGRA in memory — encode directly from the mmap'd buffer.
            if stride == w * 4 && map_len >= w * h * 4 {
                self.encode_bgra(&plane_data[..w * h * 4])
            } else {
                // Pack rows (strip stride padding).
                let mut packed = Vec::with_capacity(w * h * 4);
                for row in 0..h {
                    let start = row * stride;
                    let end = start + w * 4;
                    if end <= plane_data.len() {
                        packed.extend_from_slice(&plane_data[start..end]);
                    }
                }
                self.encode_bgra(&packed)
            }
        } else if fourcc == blit_compositor::drm_fourcc::ABGR8888
            || fourcc == blit_compositor::drm_fourcc::XBGR8888
        {
            // RGBA in memory.
            if stride == w * 4 && map_len >= w * h * 4 {
                self.encode(&plane_data[..w * h * 4])
            } else {
                let mut packed = Vec::with_capacity(w * h * 4);
                for row in 0..h {
                    let start = row * stride;
                    let end = start + w * 4;
                    if end <= plane_data.len() {
                        packed.extend_from_slice(&plane_data[start..end]);
                    }
                }
                self.encode(&packed)
            }
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
        let sync_end = DmaBufSync {
            flags: DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ,
        };
        unsafe {
            libc::ioctl(raw_fd, DMA_BUF_IOCTL_SYNC as _, &sync_end);
        }

        result
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
            SurfaceEncoderKind::NvencH264(enc)
            | SurfaceEncoderKind::NvencH265(enc)
            | SurfaceEncoderKind::NvencAV1(enc) => enc.encode_bgra_padded(bgra, src_w, src_h),
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H264Vaapi(enc) => enc.encode_bgra_padded(bgra, src_w, src_h),
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H265Vaapi(enc) => enc.encode_bgra_padded(bgra, src_w, src_h),
            SurfaceEncoderKind::AV1Software(encoder) => {
                let yuv = bgra_to_yuv420_padded(bgra, src_w, src_h, enc_w, enc_h);
                encoder.encode_yuv_planes(&yuv)
            }
        };
        // Hardware encoders (NVENC, VA-API) may report the wrong picture
        // type due to struct layout mismatches.  Re-detect from the
        // bitstream to be safe — this is a cheap scan.
        if let Some((ref data, ref mut is_key)) = result
            && !*is_key
        {
            *is_key = match &self.kind {
                SurfaceEncoderKind::NvencH264(_) => h264_stream_contains_idr(data),
                SurfaceEncoderKind::NvencH265(_) => h265_stream_contains_idr(data),
                #[cfg(target_os = "linux")]
                SurfaceEncoderKind::H264Vaapi(_) => h264_stream_contains_idr(data),
                #[cfg(target_os = "linux")]
                SurfaceEncoderKind::H265Vaapi(_) => h265_stream_contains_idr(data),
                _ => false,
            };
        }
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

        match &mut self.kind {
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
                    self.encode(&rgba)
                }
            }
            SurfaceEncoderKind::NvencH264(_)
            | SurfaceEncoderKind::NvencH265(_)
            | SurfaceEncoderKind::NvencAV1(_) => {
                // NVENC accepts BGRA; convert NV12→RGBA→BGRA (uncommon path).
                let pd = PixelData::Nv12 {
                    data: std::sync::Arc::new(data.to_vec()),
                    y_stride,
                    uv_stride,
                };
                let rgba = pd.to_rgba(self.source_width, self.source_height);
                self.encode(&rgba)
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H264Vaapi(enc) => {
                let uv_offset = y_stride * src_h;
                let y_data = &data[..uv_offset];
                let uv_data = &data[uv_offset..];
                let mut r = enc.encode_nv12(y_data, uv_data, y_stride, uv_stride);
                if let Some((ref d, ref mut k)) = r
                    && !*k
                {
                    *k = h264_stream_contains_idr(d);
                }
                r
            }
            #[cfg(target_os = "linux")]
            SurfaceEncoderKind::H265Vaapi(enc) => {
                let uv_offset = y_stride * src_h;
                let y_data = &data[..uv_offset];
                let uv_data = &data[uv_offset..];
                let mut r = enc.encode_nv12(y_data, uv_data, y_stride, uv_stride);
                if let Some((ref d, ref mut k)) = r
                    && !*k
                {
                    *k = h265_stream_contains_idr(d);
                }
                r
            }
            SurfaceEncoderKind::AV1Software(encoder) => {
                encoder.encode_nv12(data, y_stride, uv_stride, src_w, src_h)
            }
        }
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
    let chroma_w = width / 2;
    let chroma_h = height / 2;
    for cy in 0..chroma_h {
        for cx in 0..chroma_w {
            let row = cy * 2;
            let col = cx * 2;
            // Average 2x2 block
            let mut u_sum = 0i32;
            let mut v_sum = 0i32;
            for dy in 0..2u32 {
                for dx in 0..2u32 {
                    let i = ((row + dy as usize) * width + col + dx as usize) * 4;
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

/// Padded chroma planes: produces `enc_w/2 × enc_h/2` chroma samples with
/// edge-pixel duplication for pixels beyond `src_w × src_h`.
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
    let chroma_w = enc_w / 2;
    let chroma_h = enc_h / 2;
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
    let uv_w = enc_w / 2;
    let uv_size = uv_w * (enc_h / 2);
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
    let uv_w = width / 2;
    let uv_size = uv_w * (height / 2);
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
    let uv_w = width / 2;
    let uv_h = height / 2;
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

    // Deinterleave UV -> separate U, V
    for row in 0..uv_h {
        let src_start = uv_offset + row * uv_stride;
        let dst_start = row * uv_w;
        for col in 0..uv_w {
            u_out[dst_start + col] = data[src_start + col * 2];
            v_out[dst_start + col] = data[src_start + col * 2 + 1];
        }
    }

    yuv
}

/// Scan an Annex B H.264 bitstream for an IDR NAL unit (type 5).
fn h264_stream_contains_idr(data: &[u8]) -> bool {
    annex_b_contains_nal(data, |byte| (byte & 0x1f) == 5)
}

/// Scan an Annex B H.265 bitstream for an IDR NAL unit (types 19–20).
fn h265_stream_contains_idr(data: &[u8]) -> bool {
    annex_b_contains_nal(data, |byte| {
        let nal_type = (byte >> 1) & 0x3f;
        nal_type == 19 || nal_type == 20 // IDR_W_RADL, IDR_N_LP
    })
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

struct SoftwareH264Encoder {
    encoder: OpenH264Encoder,
}

impl SoftwareH264Encoder {
    fn new() -> Result<Self, String> {
        let encoder = OpenH264Encoder::new()
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
}

impl SoftwareAV1Encoder {
    fn new(width: u32, height: u32, quality: SurfaceQuality) -> Result<Self, String> {
        use rav1e::prelude::*;

        let mut speed = SpeedSettings::from_preset(quality.av1_speed());
        speed.rdo_lookahead_frames = 1;
        let enc = EncoderConfig {
            width: width as usize,
            height: height as usize,
            chroma_sampling: ChromaSampling::Cs420,
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
        })
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn encode(&mut self, rgba: &[u8]) -> Option<(Vec<u8>, bool)> {
        let yuv = rgba_to_yuv420(rgba, self.width, self.height);
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

    /// Encode from pre-converted I420 planar YUV data (Y + U + V contiguous).
    fn encode_yuv_planes(&mut self, yuv: &[u8]) -> Option<(Vec<u8>, bool)> {
        let width = self.width;
        let height = self.height;
        let y_size = width * height;
        let uv_w = width.div_ceil(2);
        let uv_h = height.div_ceil(2);
        let uv_size = uv_w * uv_h;

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
