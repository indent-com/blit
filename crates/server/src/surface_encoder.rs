use openh264::encoder::Encoder as OpenH264Encoder;
use openh264::formats::YUVBuffer;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SurfaceH264EncoderPreference {
    #[default]
    Auto,
    Software,
    Vaapi,
}

impl SurfaceH264EncoderPreference {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "software" => Some(Self::Software),
            "vaapi" => Some(Self::Vaapi),
            _ => None,
        }
    }
}

pub struct SurfaceEncoder {
    width: u32,
    height: u32,
    kind: SurfaceEncoderKind,
}

enum SurfaceEncoderKind {
    Software(Box<SoftwareSurfaceEncoder>),
    #[cfg(feature = "vaapi")]
    Vaapi(Box<VaapiSurfaceEncoder>),
}

impl SurfaceEncoder {
    pub fn new(
        preference: SurfaceH264EncoderPreference,
        width: u32,
        height: u32,
        vaapi_device: &str,
        verbose: bool,
    ) -> Result<Self, String> {
        validate_surface_dimensions(width, height)?;
        let _ = vaapi_device;
        let _ = verbose;

        match preference {
            SurfaceH264EncoderPreference::Software => Ok(Self {
                width,
                height,
                kind: SurfaceEncoderKind::Software(Box::new(SoftwareSurfaceEncoder::new()?)),
            }),
            SurfaceH264EncoderPreference::Vaapi => {
                #[cfg(feature = "vaapi")]
                {
                    Ok(Self {
                        width,
                        height,
                        kind: SurfaceEncoderKind::Vaapi(Box::new(VaapiSurfaceEncoder::new(
                            width,
                            height,
                            vaapi_device,
                        )?)),
                    })
                }
                #[cfg(not(feature = "vaapi"))]
                {
                    Err("vaapi encoder requested but blit-server was built without the vaapi feature"
                        .into())
                }
            }
            SurfaceH264EncoderPreference::Auto => {
                #[cfg(feature = "vaapi")]
                {
                    match VaapiSurfaceEncoder::new(width, height, vaapi_device) {
                        Ok(encoder) => {
                            return Ok(Self {
                                width,
                                height,
                                kind: SurfaceEncoderKind::Vaapi(Box::new(encoder)),
                            });
                        }
                        Err(err) => {
                            if verbose {
                                eprintln!(
                                    "[surface-encoder] vaapi unavailable for {width}x{height} on {}: {err}; falling back to software",
                                    vaapi_device
                                );
                            }
                        }
                    }
                }

                Ok(Self {
                    width,
                    height,
                    kind: SurfaceEncoderKind::Software(Box::new(SoftwareSurfaceEncoder::new()?)),
                })
            }
        }
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn kind_name(&self) -> &'static str {
        match &self.kind {
            SurfaceEncoderKind::Software(_) => "software",
            #[cfg(feature = "vaapi")]
            SurfaceEncoderKind::Vaapi(_) => "vaapi",
        }
    }

    pub fn request_keyframe(&mut self) {
        match &mut self.kind {
            SurfaceEncoderKind::Software(encoder) => encoder.request_keyframe(),
            #[cfg(feature = "vaapi")]
            SurfaceEncoderKind::Vaapi(encoder) => encoder.request_keyframe(),
        }
    }

    pub fn encode(&mut self, rgba: &[u8]) -> Option<(Vec<u8>, bool)> {
        if rgba.len() != expected_rgba_len(self.width, self.height)? {
            return None;
        }

        match &mut self.kind {
            SurfaceEncoderKind::Software(encoder) => encoder.encode(rgba, self.width, self.height),
            #[cfg(feature = "vaapi")]
            SurfaceEncoderKind::Vaapi(encoder) => encoder.encode(rgba),
        }
    }
}

fn validate_surface_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("surface encoder requires non-zero dimensions".into());
    }
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(format!(
            "surface encoder requires even dimensions for 4:2:0 H.264 (got {width}x{height})"
        ));
    }
    let _ = expected_rgba_len(width, height)
        .ok_or_else(|| format!("surface encoder dimensions overflow for {width}x{height}"))?;
    Ok(())
}

fn expected_rgba_len(width: u32, height: u32) -> Option<usize> {
    (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)
}

fn rgba_components(rgba: &[u8], width: usize, row: usize, col: usize) -> (i32, i32, i32) {
    let i = (row * width + col) * 4;
    (rgba[i] as i32, rgba[i + 1] as i32, rgba[i + 2] as i32)
}

fn rgb_to_y(r: i32, g: i32, b: i32) -> u8 {
    ((((66 * r) + (129 * g) + (25 * b) + 128) >> 8) + 16).clamp(0, 255) as u8
}

fn rgb_to_u(r: i32, g: i32, b: i32) -> u8 {
    ((((-38 * r) - (74 * g) + (112 * b) + 128) >> 8) + 128).clamp(0, 255) as u8
}

fn rgb_to_v(r: i32, g: i32, b: i32) -> u8 {
    ((((112 * r) - (94 * g) - (18 * b) + 128) >> 8) + 128).clamp(0, 255) as u8
}

fn rgba_to_yuv420(rgba: &[u8], width: usize, height: usize) -> Vec<u8> {
    let y_size = width * height;
    let uv_width = width / 2;
    let uv_height = height / 2;
    let uv_size = uv_width * uv_height;
    let mut yuv = vec![0u8; y_size + uv_size * 2];
    let (y_plane, uv_planes) = yuv.split_at_mut(y_size);
    let (u_plane, v_plane) = uv_planes.split_at_mut(uv_size);

    for row in 0..height {
        for col in 0..width {
            let (r, g, b) = rgba_components(rgba, width, row, col);
            y_plane[row * width + col] = rgb_to_y(r, g, b);
        }
    }

    for row in (0..height).step_by(2) {
        for col in (0..width).step_by(2) {
            let mut u_sum = 0u32;
            let mut v_sum = 0u32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let (r, g, b) = rgba_components(rgba, width, row + dy, col + dx);
                    u_sum += rgb_to_u(r, g, b) as u32;
                    v_sum += rgb_to_v(r, g, b) as u32;
                }
            }
            let uv_index = (row / 2) * uv_width + (col / 2);
            u_plane[uv_index] = (u_sum / 4) as u8;
            v_plane[uv_index] = (v_sum / 4) as u8;
        }
    }

    yuv
}

#[cfg(any(feature = "vaapi", test))]
fn write_nv12_y_plane(
    rgba: &[u8],
    width: usize,
    height: usize,
    y_plane: &mut [u8],
    y_stride: usize,
) -> Option<()> {
    if y_plane.len() < y_stride.checked_mul(height)? {
        return None;
    }

    for row in 0..height {
        let row_start = row * y_stride;
        let row_end = row_start + width;
        let dst = y_plane.get_mut(row_start..row_end)?;
        for (col, pixel) in dst.iter_mut().enumerate() {
            let (r, g, b) = rgba_components(rgba, width, row, col);
            *pixel = rgb_to_y(r, g, b);
        }
    }

    Some(())
}

#[cfg(any(feature = "vaapi", test))]
fn write_nv12_uv_plane(
    rgba: &[u8],
    width: usize,
    height: usize,
    uv_plane: &mut [u8],
    uv_stride: usize,
) -> Option<()> {
    if uv_plane.len() < uv_stride.checked_mul(height / 2)? {
        return None;
    }

    for row in (0..height).step_by(2) {
        let row_start = (row / 2) * uv_stride;
        let row_end = row_start + width;
        let dst = uv_plane.get_mut(row_start..row_end)?;
        for col in (0..width).step_by(2) {
            let mut u_sum = 0u32;
            let mut v_sum = 0u32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let (r, g, b) = rgba_components(rgba, width, row + dy, col + dx);
                    u_sum += rgb_to_u(r, g, b) as u32;
                    v_sum += rgb_to_v(r, g, b) as u32;
                }
            }
            dst[col] = (u_sum / 4) as u8;
            dst[col + 1] = (v_sum / 4) as u8;
        }
    }

    Some(())
}

#[cfg(test)]
fn rgba_to_nv12(
    rgba: &[u8],
    width: usize,
    height: usize,
    y_plane: &mut [u8],
    y_stride: usize,
    uv_plane: &mut [u8],
    uv_stride: usize,
) -> Option<()> {
    write_nv12_y_plane(rgba, width, height, y_plane, y_stride)?;
    write_nv12_uv_plane(rgba, width, height, uv_plane, uv_stride)?;
    Some(())
}

fn h264_stream_contains_idr(data: &[u8]) -> bool {
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
            && (byte & 0x1f) == 5
        {
            return true;
        }

        i = nal_header.saturating_add(1);
    }

    false
}

struct SoftwareSurfaceEncoder {
    encoder: OpenH264Encoder,
}

impl SoftwareSurfaceEncoder {
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
        let bitstream = self.encoder.encode(&yuv_buf).ok()?;
        let nal_data = bitstream.to_vec();
        if nal_data.is_empty() {
            return None;
        }
        let is_keyframe = h264_stream_contains_idr(&nal_data);
        Some((nal_data, is_keyframe))
    }
}

#[cfg(feature = "vaapi")]
mod vaapi {
    use std::ffi::CString;
    use std::ptr;

    use ffmpeg_next::codec::Flags as CodecFlags;
    use ffmpeg_next::codec::context::Context as FfmpegContext;
    use ffmpeg_next::codec::encoder::video::Encoder as FfmpegVideoEncoder;
    use ffmpeg_next::error::EAGAIN;
    use ffmpeg_next::packet::Mut as PacketMut;
    use ffmpeg_next::util::picture;
    use ffmpeg_next::{Dictionary, Error as FfmpegError, Packet, ffi, frame};

    pub(super) struct VaapiSurfaceEncoder {
        encoder: FfmpegVideoEncoder,
        sw_frame: frame::Video,
        next_pts: i64,
        force_next_keyframe: bool,
    }

    impl VaapiSurfaceEncoder {
        pub(super) fn new(width: u32, height: u32, vaapi_device: &str) -> Result<Self, String> {
            ffmpeg_next::init().map_err(|err| format!("failed to initialize ffmpeg: {err}"))?;

            let codec = ffmpeg_next::encoder::find_by_name("h264_vaapi")
                .ok_or_else(|| "ffmpeg encoder h264_vaapi not found".to_string())?;
            let context = FfmpegContext::new_with_codec(codec);
            let mut video = context
                .encoder()
                .video()
                .map_err(|err| format!("failed to create ffmpeg video encoder context: {err}"))?;

            video.set_width(width);
            video.set_height(height);
            video.set_format(ffmpeg_next::util::format::Pixel::VAAPI);
            video.set_time_base((1, 60));
            video.set_frame_rate(Some((60, 1)));
            video.set_aspect_ratio((1, 1));
            video.set_gop(60);
            video.set_max_b_frames(0);
            video.set_flags(CodecFlags::LOW_DELAY);

            unsafe {
                (*video.as_mut_ptr()).profile = ffi::FF_PROFILE_H264_CONSTRAINED_BASELINE;
                (*video.as_mut_ptr()).level = 31;
            }

            let mut device_ctx = ptr::null_mut();
            let device_name = CString::new(vaapi_device)
                .map_err(|_| format!("invalid vaapi device path: {vaapi_device}"))?;
            let device_result = unsafe {
                ffi::av_hwdevice_ctx_create(
                    &mut device_ctx,
                    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    device_name.as_ptr(),
                    ptr::null_mut(),
                    0,
                )
            };
            if device_result < 0 {
                return Err(format!(
                    "failed to create vaapi device {vaapi_device}: {}",
                    FfmpegError::from(device_result)
                ));
            }

            let set_frames_result =
                set_hwframe_context(unsafe { video.as_mut_ptr() }, device_ctx, width, height);
            unsafe {
                ffi::av_buffer_unref(&mut device_ctx);
            }
            set_frames_result?;

            let mut options = Dictionary::new();
            options.set("profile", "constrained_baseline");
            options.set("level", "3.1");
            options.set("coder", "cavlc");
            options.set("rc_mode", "CQP");
            options.set("qp", "23");
            options.set("aud", "1");
            options.set("async_depth", "1");
            options.set("idr_interval", "1");

            let encoder = video
                .open_with(options)
                .map_err(|err| format!("failed to open h264_vaapi encoder: {err}"))?;
            let sw_frame = frame::Video::new(ffmpeg_next::util::format::Pixel::NV12, width, height);

            Ok(Self {
                encoder,
                sw_frame,
                next_pts: 0,
                force_next_keyframe: false,
            })
        }

        pub(super) fn request_keyframe(&mut self) {
            self.force_next_keyframe = true;
        }

        pub(super) fn encode(&mut self, rgba: &[u8]) -> Option<(Vec<u8>, bool)> {
            self.fill_sw_frame(rgba).ok()?;
            if self.force_next_keyframe {
                self.sw_frame.set_kind(picture::Type::I);
            } else {
                self.sw_frame.set_kind(picture::Type::None);
            }

            let mut hw_frame = frame::Video::empty();
            let hw_result = unsafe {
                ffi::av_hwframe_get_buffer(
                    (*self.encoder.as_mut_ptr()).hw_frames_ctx,
                    hw_frame.as_mut_ptr(),
                    0,
                )
            };
            if hw_result < 0 {
                return None;
            }

            let transfer_result = unsafe {
                ffi::av_hwframe_transfer_data(hw_frame.as_mut_ptr(), self.sw_frame.as_ptr(), 0)
            };
            if transfer_result < 0 {
                return None;
            }

            let props_result =
                unsafe { ffi::av_frame_copy_props(hw_frame.as_mut_ptr(), self.sw_frame.as_ptr()) };
            if props_result < 0 {
                return None;
            }
            if self.force_next_keyframe {
                hw_frame.set_kind(picture::Type::I);
            } else {
                hw_frame.set_kind(picture::Type::None);
            }
            self.force_next_keyframe = false;

            self.encoder.send_frame(&hw_frame).ok()?;
            self.next_pts += 1;

            let mut packet = Packet::empty();
            let mut nal_data = Vec::new();
            let mut is_keyframe = false;

            loop {
                match self.encoder.receive_packet(&mut packet) {
                    Ok(()) => {
                        if let Some(data) = packet.data() {
                            nal_data.extend_from_slice(data);
                        }
                        is_keyframe |= packet.is_key();
                        unsafe {
                            ffi::av_packet_unref(packet.as_mut_ptr());
                        }
                    }
                    Err(FfmpegError::Other { errno }) if errno == EAGAIN => break,
                    Err(FfmpegError::Eof) => break,
                    Err(_) => return None,
                }
            }

            if nal_data.is_empty() {
                return None;
            }

            Some((nal_data, is_keyframe))
        }

        fn fill_sw_frame(&mut self, rgba: &[u8]) -> Result<(), String> {
            let make_writable_result =
                unsafe { ffi::av_frame_make_writable(self.sw_frame.as_mut_ptr()) };
            if make_writable_result < 0 {
                return Err(format!(
                    "failed to make ffmpeg software frame writable: {}",
                    FfmpegError::from(make_writable_result)
                ));
            }

            let width = self.sw_frame.width() as usize;
            let height = self.sw_frame.height() as usize;
            let y_stride = self.sw_frame.stride(0);
            let uv_stride = self.sw_frame.stride(1);

            {
                let y_plane = self.sw_frame.data_mut(0);
                super::write_nv12_y_plane(rgba, width, height, y_plane, y_stride).ok_or_else(
                    || {
                        format!(
                            "failed to write NV12 Y plane for {}x{}",
                            self.sw_frame.width(),
                            self.sw_frame.height()
                        )
                    },
                )?;
            }
            {
                let uv_plane = self.sw_frame.data_mut(1);
                super::write_nv12_uv_plane(rgba, width, height, uv_plane, uv_stride).ok_or_else(
                    || {
                        format!(
                            "failed to write NV12 UV plane for {}x{}",
                            self.sw_frame.width(),
                            self.sw_frame.height()
                        )
                    },
                )?;
            }

            self.sw_frame.set_pts(Some(self.next_pts));
            Ok(())
        }
    }

    fn set_hwframe_context(
        codec_ctx: *mut ffi::AVCodecContext,
        device_ctx: *mut ffi::AVBufferRef,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let mut frames_ref = unsafe { ffi::av_hwframe_ctx_alloc(device_ctx) };
        if frames_ref.is_null() {
            return Err("failed to allocate vaapi frames context".into());
        }

        let result = unsafe {
            let frames_ctx = (*frames_ref).data as *mut ffi::AVHWFramesContext;
            if frames_ctx.is_null() {
                Err("ffmpeg returned a null vaapi frames context".into())
            } else {
                (*frames_ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
                (*frames_ctx).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
                (*frames_ctx).width = width as i32;
                (*frames_ctx).height = height as i32;
                (*frames_ctx).initial_pool_size = 8;

                let init_result = ffi::av_hwframe_ctx_init(frames_ref);
                if init_result < 0 {
                    Err(format!(
                        "failed to initialize vaapi frames context: {}",
                        FfmpegError::from(init_result)
                    ))
                } else {
                    (*codec_ctx).hw_frames_ctx = ffi::av_buffer_ref(frames_ref);
                    if (*codec_ctx).hw_frames_ctx.is_null() {
                        Err("failed to retain vaapi frames context".into())
                    } else {
                        Ok(())
                    }
                }
            }
        };

        unsafe {
            ffi::av_buffer_unref(&mut frames_ref);
        }
        result
    }
}

#[cfg(feature = "vaapi")]
use vaapi::VaapiSurfaceEncoder;

#[cfg(test)]
mod tests {
    use super::{
        SurfaceH264EncoderPreference, h264_stream_contains_idr, rgba_to_nv12, rgba_to_yuv420,
    };

    #[test]
    fn surface_encoder_preference_parser_accepts_known_values() {
        assert_eq!(
            SurfaceH264EncoderPreference::parse("auto"),
            Some(SurfaceH264EncoderPreference::Auto)
        );
        assert_eq!(
            SurfaceH264EncoderPreference::parse("software"),
            Some(SurfaceH264EncoderPreference::Software)
        );
        assert_eq!(
            SurfaceH264EncoderPreference::parse("vaapi"),
            Some(SurfaceH264EncoderPreference::Vaapi)
        );
        assert_eq!(SurfaceH264EncoderPreference::parse("bogus"), None);
    }

    #[test]
    fn rgba_to_yuv420_black_pixels_match_video_range_black() {
        let rgba = [0u8, 0, 0, 255].repeat(4);
        let yuv = rgba_to_yuv420(&rgba, 2, 2);
        assert_eq!(yuv, vec![16, 16, 16, 16, 128, 128]);
    }

    #[test]
    fn rgba_to_nv12_black_pixels_match_video_range_black() {
        let rgba = [0u8, 0, 0, 255].repeat(4);
        let mut y_plane = vec![0u8; 4];
        let mut uv_plane = vec![0u8; 2];

        rgba_to_nv12(&rgba, 2, 2, &mut y_plane, 2, &mut uv_plane, 2).unwrap();

        assert_eq!(y_plane, vec![16, 16, 16, 16]);
        assert_eq!(uv_plane, vec![128, 128]);
    }

    #[test]
    fn idr_nal_units_are_detected() {
        assert!(h264_stream_contains_idr(&[0, 0, 0, 1, 0x65, 0x88, 0x84]));
        assert!(h264_stream_contains_idr(&[0, 0, 1, 0x65, 0x00]));
        assert!(!h264_stream_contains_idr(&[0, 0, 0, 1, 0x41, 0x9a]));
    }
}
