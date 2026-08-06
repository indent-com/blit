//! Vulkan Video H.264 encoder using ash 0.38.
//!
//! Ash 0.38 has the raw Vulkan Video types (VideoSessionKHR,
//! VideoEncodeH264*, StdVideoH264*, etc.) but does NOT ship extension
//! function pointer loader modules.  We load the required function
//! pointers manually via `vkGetDeviceProcAddr` / `vkGetInstanceProcAddr`.
//!
//! StdVideo types live in `ash::vk::native::*` (bindgen-generated C
//! structs, not Rust-safe wrappers).  They are zero-initialised with
//! `std::mem::zeroed()` and filled field-by-field.

#![allow(
    dead_code,
    non_upper_case_globals,
    non_snake_case,
    clippy::missing_transmute_annotations,
    clippy::too_many_arguments,
    clippy::missing_safety_doc,
    clippy::manual_div_ceil
)]

use std::ptr;

use ash::vk;
use ash::vk::native::*;

// ===================================================================
// Function pointer table
// ===================================================================

/// Manually-loaded Vulkan Video function pointers.
///
/// Instance-level:
///   - `get_physical_device_video_capabilities`
///
/// Device-level (all others):
///   - `create_video_session`
///   - `destroy_video_session`
///   - `get_video_session_memory_requirements`
///   - `bind_video_session_memory`
///   - `create_video_session_parameters`
///   - `destroy_video_session_parameters`
///   - `cmd_begin_video_coding`
///   - `cmd_end_video_coding`
///   - `cmd_control_video_coding`
///   - `cmd_encode_video`
pub(crate) struct VideoFns {
    pub get_physical_device_video_capabilities: vk::PFN_vkGetPhysicalDeviceVideoCapabilitiesKHR,
    pub create_video_session: vk::PFN_vkCreateVideoSessionKHR,
    pub destroy_video_session: vk::PFN_vkDestroyVideoSessionKHR,
    pub get_video_session_memory_requirements: vk::PFN_vkGetVideoSessionMemoryRequirementsKHR,
    pub bind_video_session_memory: vk::PFN_vkBindVideoSessionMemoryKHR,
    pub create_video_session_parameters: vk::PFN_vkCreateVideoSessionParametersKHR,
    pub destroy_video_session_parameters: vk::PFN_vkDestroyVideoSessionParametersKHR,
    /// Retrieves the encoded SPS/PPS (or AV1 sequence header) bytes.  Vulkan
    /// Video does not put them in the output bitstream itself, so without
    /// this the stream is nothing but slice NALs and no decoder will touch
    /// it.
    pub get_encoded_video_session_parameters: vk::PFN_vkGetEncodedVideoSessionParametersKHR,
    pub cmd_begin_video_coding: vk::PFN_vkCmdBeginVideoCodingKHR,
    pub cmd_end_video_coding: vk::PFN_vkCmdEndVideoCodingKHR,
    pub cmd_control_video_coding: vk::PFN_vkCmdControlVideoCodingKHR,
    pub cmd_encode_video: vk::PFN_vkCmdEncodeVideoKHR,
}

impl VideoFns {
    /// Load all Vulkan Video function pointers.
    ///
    /// `entry` is needed for `vkGetInstanceProcAddr` (instance-level
    /// functions like `vkGetPhysicalDeviceVideoCapabilitiesKHR`).
    /// `instance` + `device` are used for device-level functions via
    /// `vkGetDeviceProcAddr`.
    pub(crate) unsafe fn load(
        entry: &ash::Entry,
        instance: &ash::Instance,
        device: &ash::Device,
    ) -> Option<Self> {
        let dev = device.handle();
        let inst = instance.handle();

        macro_rules! load_device {
            ($name:literal) => {{
                let ptr = unsafe {
                    instance.get_device_proc_addr(dev, concat!($name, "\0").as_ptr().cast())
                };
                if ptr.is_none() {
                    eprintln!(concat!("[vulkan-encode] failed to load ", $name));
                    return None;
                }
                unsafe { std::mem::transmute(ptr.unwrap()) }
            }};
        }

        macro_rules! load_instance {
            ($name:literal) => {{
                let ptr = unsafe {
                    entry.get_instance_proc_addr(inst, concat!($name, "\0").as_ptr().cast())
                };
                if ptr.is_none() {
                    eprintln!(concat!("[vulkan-encode] failed to load ", $name));
                    return None;
                }
                unsafe { std::mem::transmute(ptr.unwrap()) }
            }};
        }

        Some(Self {
            get_physical_device_video_capabilities: load_instance!(
                "vkGetPhysicalDeviceVideoCapabilitiesKHR"
            ),
            create_video_session: load_device!("vkCreateVideoSessionKHR"),
            destroy_video_session: load_device!("vkDestroyVideoSessionKHR"),
            get_video_session_memory_requirements: load_device!(
                "vkGetVideoSessionMemoryRequirementsKHR"
            ),
            bind_video_session_memory: load_device!("vkBindVideoSessionMemoryKHR"),
            create_video_session_parameters: load_device!("vkCreateVideoSessionParametersKHR"),
            destroy_video_session_parameters: load_device!("vkDestroyVideoSessionParametersKHR"),
            get_encoded_video_session_parameters: load_device!(
                "vkGetEncodedVideoSessionParametersKHR"
            ),
            cmd_begin_video_coding: load_device!("vkCmdBeginVideoCodingKHR"),
            cmd_end_video_coding: load_device!("vkCmdEndVideoCodingKHR"),
            cmd_control_video_coding: load_device!("vkCmdControlVideoCodingKHR"),
            cmd_encode_video: load_device!("vkCmdEncodeVideoKHR"),
        })
    }
}

/// Fetch the driver-encoded parameter-set bytes for a session.
///
/// Two-call idiom: once with a null buffer to learn the size, once to fill
/// it.  `codec_get` is the codec-specific selector (which of SPS/PPS, or the
/// AV1 sequence header, to write) and is chained into the get-info struct.
unsafe fn get_encoded_session_parameters<T: vk::ExtendsVideoEncodeSessionParametersGetInfoKHR>(
    device: &ash::Device,
    video_fns: &VideoFns,
    session_params: vk::VideoSessionParametersKHR,
    codec_get: &mut T,
) -> Option<Vec<u8>> {
    let mut feedback = vk::VideoEncodeSessionParametersFeedbackInfoKHR::default();
    let get_info = vk::VideoEncodeSessionParametersGetInfoKHR::default()
        .video_session_parameters(session_params)
        .push_next(codec_get);

    let mut size: usize = 0;
    let res = unsafe {
        (video_fns.get_encoded_video_session_parameters)(
            device.handle(),
            &get_info,
            &mut feedback,
            &mut size,
            ptr::null_mut(),
        )
    };
    if res != vk::Result::SUCCESS || size == 0 {
        // NVIDIA's driver (595.84) fails the pData=NULL *size query* for a
        // High 4:4:4 Predictive PPS with ERROR_OUT_OF_HOST_MEMORY and
        // size=0, but its *writer* works: retry against a caller-sized
        // buffer before declaring the parameter sets unobtainable.
        eprintln!(
            "[vulkan-encode] parameter-set size query failed: {res:?} size={size}; \
             retrying with a fixed-size buffer",
        );
        size = 4096;
    }

    let mut buf = vec![0u8; size];
    let res = unsafe {
        (video_fns.get_encoded_video_session_parameters)(
            device.handle(),
            &get_info,
            &mut feedback,
            &mut size,
            buf.as_mut_ptr().cast(),
        )
    };
    if res != vk::Result::SUCCESS {
        eprintln!("[vulkan-encode] parameter-set fetch failed: {res:?}");
        return None;
    }
    buf.truncate(size);
    Some(buf)
}

// ===================================================================
// DPB slot
// ===================================================================

struct DpbSlot {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

// ===================================================================
// VulkanVideoEncoder
// ===================================================================

/// Codec type for the encoder (determines codec_flag and frame encoding path).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VulkanVideoCodec {
    H264,
    AV1,
}

pub(crate) struct VulkanVideoEncoder {
    width: u32,
    height: u32,
    /// Pre-alignment source dimensions.  `width`/`height` are the coded
    /// extent (superblock/macroblock aligned); the bitstream declares these
    /// so decoders crop the alignment padding — H.264 via SPS cropping,
    /// AV1 via the sequence header's max frame size.
    src_width: u32,
    src_height: u32,
    codec: VulkanVideoCodec,
    video_session: vk::VideoSessionKHR,
    session_params: vk::VideoSessionParametersKHR,
    session_memory: Vec<vk::DeviceMemory>,
    dpb_slots: [DpbSlot; 2],
    cur_dpb_idx: usize,
    bitstream_buffer: vk::Buffer,
    bitstream_memory: vk::DeviceMemory,
    bitstream_ptr: *mut u8,
    bitstream_capacity: u64,
    query_pool: vk::QueryPool,
    frame_num: u32,
    idr_num: u32,
    force_idr: bool,
    qp: u8,
    /// AV1 only: the order hint each decoder-side reference slot holds,
    /// mirrored here so frame headers can state them (`ref_order_hint`).
    /// A keyframe refreshes every slot; a delta refreshes only the slot it
    /// reconstructs into.
    ref_order_hints: [u8; 8],
    /// Encoded SPS/PPS, prepended to every IDR so the stream carries its own
    /// parameter sets.  Vulkan Video does not emit them with the slice data.
    params_bytes: Vec<u8>,
    /// Set when a fence wait timed out. The submission owning that fence is
    /// still running somewhere on the GPU and may still write to
    /// `bitstream_buffer`, so this encoder can never be used again — see
    /// [`encode_fence_timeout_ns`], and `encode` for why nothing rebuilds it.
    poisoned: bool,
}

unsafe impl Send for VulkanVideoEncoder {}

/// Bitstream buffer size (2 MiB -- generous for a single frame).
const BITSTREAM_CAPACITY: u64 = 2 * 1024 * 1024;

/// Largest H.264 quantization parameter the spec defines for 8-bit luma.
const H264_MAX_QP: u8 = 51;

/// How long to wait for an encode submission to complete before giving up.
///
/// This wait used to be `u64::MAX`, on the compositor thread: a driver or GPU
/// that never signalled the fence wedged the whole compositor, and every
/// surface with it, permanently and with no diagnostic.
///
/// Ten seconds is far beyond any real encode — the server's own encode
/// timeout is 5s — so reaching it means the device is not coming back.
/// `BLIT_ENCODE_FENCE_TIMEOUT_MS` overrides it; `0` restores the old
/// wait-forever behaviour for anyone debugging a driver.
pub(crate) fn encode_fence_timeout_ns() -> u64 {
    static V: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
        let ms = std::env::var("BLIT_ENCODE_FENCE_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(10_000);
        if ms == 0 {
            u64::MAX
        } else {
            ms.saturating_mul(1_000_000)
        }
    });
    *V
}

impl VulkanVideoEncoder {
    /// Create a Vulkan Video H.264 encoder.
    ///
    /// Returns `None` if the device does not support H.264 encode or any
    /// required step fails.
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn try_new_h264(
        device: &ash::Device,
        instance: &ash::Instance,
        physical_device: vk::PhysicalDevice,
        video_fns: &VideoFns,
        video_queue_family: u32,
        width: u32,
        height: u32,
        qp: u8,
        is_444: bool,
    ) -> Option<Self> {
        // ---------------------------------------------------------------
        // 1. Video profile
        // ---------------------------------------------------------------
        // 4:4:4 is High 4:4:4 Predictive, a distinct profile — not High with
        // a chroma flag flipped — and the picture format changes with it.
        // Whether a device supports it is a runtime question: the RTX 4090
        // does, the Raphael iGPU answers
        // ERROR_VIDEO_PROFILE_OPERATION_NOT_SUPPORTED_KHR to the caps query
        // below, which returns None and lets the caller fall back.
        let mut h264_profile =
            vk::VideoEncodeH264ProfileInfoKHR::default().std_profile_idc(if is_444 {
                StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH_444_PREDICTIVE
            } else {
                StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH
            });

        let picture_format = if is_444 {
            vk::Format::G8_B8R8_2PLANE_444_UNORM
        } else {
            vk::Format::G8_B8R8_2PLANE_420_UNORM
        };

        let profile = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
            .chroma_subsampling(if is_444 {
                vk::VideoChromaSubsamplingFlagsKHR::TYPE_444
            } else {
                vk::VideoChromaSubsamplingFlagsKHR::TYPE_420
            })
            .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .push_next(&mut h264_profile);

        // ---------------------------------------------------------------
        // 2. Query capabilities
        // ---------------------------------------------------------------
        let mut h264_caps = vk::VideoEncodeH264CapabilitiesKHR::default();
        let mut encode_caps = vk::VideoEncodeCapabilitiesKHR::default();
        let mut caps = vk::VideoCapabilitiesKHR::default()
            .push_next(&mut encode_caps)
            .push_next(&mut h264_caps);

        let res = unsafe {
            (video_fns.get_physical_device_video_capabilities)(physical_device, &profile, &mut caps)
        };
        if res != vk::Result::SUCCESS {
            eprintln!(
                "[vulkan-encode] vkGetPhysicalDeviceVideoCapabilitiesKHR failed for {} : {res:?}",
                if is_444 {
                    "H.264 4:4:4 High444Predictive"
                } else {
                    "H.264 4:2:0 High"
                },
            );
            return None;
        }

        // Extract fields from caps before dropping the borrow.
        let std_header_version = caps.std_header_version;
        let max_coded_w = caps.max_coded_extent.width;
        let max_coded_h = caps.max_coded_extent.height;
        let max_dpb = caps.max_dpb_slots;
        // Drop the pNext chain borrow so we can read h264_caps.
        let _ = caps;

        let max_level_idc = h264_caps.max_level_idc;
        let level_idc = compute_level_idc(width, height);
        // Clamp to driver-supported max.
        let level_idc = if level_idc > max_level_idc {
            max_level_idc
        } else {
            level_idc
        };

        eprintln!(
            "[vulkan-encode] H.264 caps: max_coded={max_coded_w}x{max_coded_h}, max_dpb={max_dpb}, max_level={max_level_idc}, level={level_idc}, flags={:#x}, std_syntax={:#x}",
            h264_caps.flags.as_raw(),
            h264_caps.std_syntax_flags.as_raw(),
        );

        // ---------------------------------------------------------------
        // 3. Create video session
        // ---------------------------------------------------------------
        let mut h264_session_create = vk::VideoEncodeH264SessionCreateInfoKHR::default()
            .use_max_level_idc(true)
            .max_level_idc(level_idc);

        let coded_extent = vk::Extent2D { width, height };

        let session_create = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(video_queue_family)
            .video_profile(&profile)
            .picture_format(picture_format)
            .max_coded_extent(coded_extent)
            .reference_picture_format(picture_format)
            .max_dpb_slots(2)
            .max_active_reference_pictures(1)
            .std_header_version(&std_header_version)
            .push_next(&mut h264_session_create);

        let mut video_session = vk::VideoSessionKHR::null();
        let res = unsafe {
            (video_fns.create_video_session)(
                device.handle(),
                &session_create,
                ptr::null(),
                &mut video_session,
            )
        };
        if res != vk::Result::SUCCESS {
            eprintln!("[vulkan-encode] vkCreateVideoSessionKHR failed: {res:?}");
            return None;
        }

        // ---------------------------------------------------------------
        // 4. Query and bind session memory
        // ---------------------------------------------------------------
        let session_memory = unsafe {
            bind_session_memory(device, video_fns, video_session, physical_device, instance)
        }?;

        // ---------------------------------------------------------------
        // 5. Session parameters (SPS / PPS)
        // ---------------------------------------------------------------
        let width_in_mbs = (width + 15) / 16;
        let height_in_mbs = (height + 15) / 16;
        let needs_crop = (width_in_mbs * 16 != width) || (height_in_mbs * 16 != height);

        let mut sps_flags: StdVideoH264SpsFlags = unsafe { std::mem::zeroed() };
        sps_flags.set_frame_mbs_only_flag(1);
        sps_flags.set_direct_8x8_inference_flag(1);
        if needs_crop {
            sps_flags.set_frame_cropping_flag(1);
        }
        // VUI with video_full_range_flag=1: blit's pixels are full-range
        // BT.601 end to end, and a decoder told nothing assumes limited —
        // which would display every black lifted to gray.
        sps_flags.set_vui_parameters_present_flag(1);
        let mut vui_flags: StdVideoH264SpsVuiFlags = unsafe { std::mem::zeroed() };
        vui_flags.set_video_signal_type_present_flag(1);
        vui_flags.set_video_full_range_flag(1);
        let mut vui: StdVideoH264SequenceParameterSetVui = unsafe { std::mem::zeroed() };
        vui.flags = vui_flags;
        vui.video_format = 5; // unspecified

        // Crop offsets are expressed in CropUnitX/CropUnitY, which depend on
        // the chroma format: 2x2 for 4:2:0, but 1x1 for 4:4:4 (and for
        // monochrome).  Dividing by a hardcoded 2 would crop half as many
        // columns and rows as intended on a 4:4:4 stream whose dimensions are
        // not a multiple of 16, leaving a strip of padding visible.
        let crop_unit = if is_444 { 1 } else { 2 };
        let crop_right = if width_in_mbs * 16 > width {
            (width_in_mbs * 16 - width) / crop_unit
        } else {
            0
        };
        let crop_bottom = if height_in_mbs * 16 > height {
            (height_in_mbs * 16 - height) / crop_unit
        } else {
            0
        };

        let mut sps: StdVideoH264SequenceParameterSet = unsafe { std::mem::zeroed() };
        sps.flags = sps_flags;
        sps.profile_idc = if is_444 {
            StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH_444_PREDICTIVE
        } else {
            StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH
        };
        sps.level_idc = level_idc;
        // separate_colour_plane_flag stays 0 (the struct is zeroed), so
        // ChromaArrayType == chroma_format_idc and the two chroma components
        // stay interleaved in one plane — which is what the two-plane
        // G8_B8R8_2PLANE_444_UNORM source provides.
        sps.chroma_format_idc = if is_444 {
            StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_444
        } else {
            StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_420
        };
        sps.seq_parameter_set_id = 0;
        sps.bit_depth_luma_minus8 = 0;
        sps.bit_depth_chroma_minus8 = 0;
        sps.log2_max_frame_num_minus4 = 0; // max_frame_num = 16
        sps.pic_order_cnt_type = StdVideoH264PocType_STD_VIDEO_H264_POC_TYPE_2;
        sps.max_num_ref_frames = 1;
        sps.pic_width_in_mbs_minus1 = width_in_mbs - 1;
        sps.pic_height_in_map_units_minus1 = height_in_mbs - 1;
        sps.frame_crop_right_offset = crop_right;
        sps.frame_crop_bottom_offset = crop_bottom;
        // `vui` is a local: the driver copies it during session-parameters
        // creation and the serializer below reads it before this returns.
        sps.pSequenceParameterSetVui = &vui;

        let mut pps_flags: StdVideoH264PpsFlags = unsafe { std::mem::zeroed() };
        pps_flags.set_entropy_coding_mode_flag(1); // CABAC
        pps_flags.set_deblocking_filter_control_present_flag(1);
        if is_444 {
            // NVENC encodes High 4:4:4 Predictive with 8x8 transforms; a PPS
            // claiming transform_8x8_mode_flag=0 asks the driver's writer to
            // describe a stream the hardware won't produce, and it fails the
            // PPS serialization (ERROR_OUT_OF_HOST_MEMORY, size=0) instead of
            // overriding — the "NVIDIA can't serialize its 4:4:4 PPS" wall.
            pps_flags.set_transform_8x8_mode_flag(1);
        }

        let mut pps: StdVideoH264PictureParameterSet = unsafe { std::mem::zeroed() };
        pps.flags = pps_flags;
        pps.seq_parameter_set_id = 0;
        pps.pic_parameter_set_id = 0;
        pps.num_ref_idx_l0_default_active_minus1 = 0;
        pps.weighted_bipred_idc =
            StdVideoH264WeightedBipredIdc_STD_VIDEO_H264_WEIGHTED_BIPRED_IDC_DEFAULT;
        // H.264 QP is 0..=51 (8-bit luma, so QpBdOffsetY is 0) and the field
        // is an i8 carrying qp-26. The server clamps before it reaches us, so
        // this is unreachable today — but nothing in between enforced it, and
        // past 127 the subtraction overflows the i8 rather than merely
        // producing a stream no decoder accepts.
        pps.pic_init_qp_minus26 = qp.min(H264_MAX_QP) as i8 - 26;

        let add_info = vk::VideoEncodeH264SessionParametersAddInfoKHR::default()
            .std_sp_ss(std::slice::from_ref(&sps))
            .std_pp_ss(std::slice::from_ref(&pps));

        let mut h264_params_create = vk::VideoEncodeH264SessionParametersCreateInfoKHR::default()
            .max_std_sps_count(1)
            .max_std_pps_count(1)
            .parameters_add_info(&add_info);

        let params_create = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(video_session)
            .push_next(&mut h264_params_create);

        let mut session_params = vk::VideoSessionParametersKHR::null();
        let res = unsafe {
            (video_fns.create_video_session_parameters)(
                device.handle(),
                &params_create,
                ptr::null(),
                &mut session_params,
            )
        };
        if res != vk::Result::SUCCESS {
            eprintln!("[vulkan-encode] vkCreateVideoSessionParametersKHR failed: {res:?}");
            for &m in &session_memory {
                unsafe { device.free_memory(m, None) };
            }
            unsafe {
                (video_fns.destroy_video_session)(device.handle(), video_session, ptr::null());
            }
            return None;
        }

        // Retrieve the encoded SPS/PPS.  Vulkan Video never writes parameter
        // sets into the output bitstream — `cmd_encode_video` emits slice
        // NALs only — so without this the stream starts at a coded slice and
        // every decoder rejects it (`ffprobe`: "Invalid data found").  They
        // are fetched once here and prepended to each IDR below, which also
        // lets a viewer that joins mid-stream start decoding at a keyframe.
        let mut h264_get = vk::VideoEncodeH264SessionParametersGetInfoKHR::default()
            .write_std_sps(true)
            .write_std_pps(true);
        let params_bytes = unsafe {
            get_encoded_session_parameters(device, video_fns, session_params, &mut h264_get)
        };
        let params_bytes = params_bytes.unwrap_or_else(|| {
            // The NVIDIA proprietary driver (595.84) advertises H.264 High
            // 4:4:4 Predictive encode caps and accepts the SPS/PPS pair at
            // vkCreateVideoSessionParametersKHR, but its own serializer
            // fails the 4:4:4 PPS with ERROR_OUT_OF_HOST_MEMORY — in both
            // the size-query and buffered forms.  The encode session itself
            // works, so serialize the parameter sets ourselves from the very
            // structs the driver just accepted, the same way the AV1 path
            // writes its sequence header (where no get API exists at all).
            eprintln!(
                "[vulkan-encode] driver could not serialize H.264 {} parameter sets; \
                 serializing them app-side",
                if is_444 { "4:4:4" } else { "4:2:0" },
            );
            h264_parameter_sets(&sps, &pps, is_444)
        });
        eprintln!(
            "[vulkan-encode] H.264 parameter sets: {} bytes",
            params_bytes.len(),
        );

        // ---------------------------------------------------------------
        // 6. DPB images (2x)
        // ---------------------------------------------------------------
        let dpb_slots = unsafe {
            allocate_dpb_slots(
                device,
                instance,
                physical_device,
                video_fns,
                width,
                height,
                video_queue_family,
                &profile,
                picture_format,
                &session_memory,
                session_params,
                video_session,
            )
        }?;

        // ---------------------------------------------------------------
        // 7. Bitstream buffer (host-visible, host-coherent)
        // ---------------------------------------------------------------
        let (bitstream_buffer, bitstream_memory, bitstream_ptr) = unsafe {
            allocate_bitstream_buffer(
                device,
                instance,
                physical_device,
                video_fns,
                BITSTREAM_CAPACITY,
                &profile,
                &dpb_slots,
                &session_memory,
                session_params,
                video_session,
            )
        }?;

        // ---------------------------------------------------------------
        // 8. Query pool (encode feedback)
        // ---------------------------------------------------------------
        // The query pool must be created against the same profile as the
        // session — a 4:4:4 session paired with a hardcoded High/4:2:0 pool
        // is a spec violation the driver merely tolerates.
        let mut h264_profile_for_qp =
            vk::VideoEncodeH264ProfileInfoKHR::default().std_profile_idc(if is_444 {
                StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH_444_PREDICTIVE
            } else {
                StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH
            });
        let mut video_profile_for_query = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
            .chroma_subsampling(if is_444 {
                vk::VideoChromaSubsamplingFlagsKHR::TYPE_444
            } else {
                vk::VideoChromaSubsamplingFlagsKHR::TYPE_420
            })
            .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .push_next(&mut h264_profile_for_qp);
        let query_pool = unsafe {
            create_encode_query_pool(
                device,
                video_fns,
                &mut video_profile_for_query,
                bitstream_buffer,
                bitstream_memory,
                &dpb_slots,
                &session_memory,
                session_params,
                video_session,
            )
        }?;

        eprintln!(
            "[vulkan-encode] initialized H.264 encoder {width}x{height} qp={qp} level={level_idc}",
        );

        Some(Self {
            width,
            height,
            src_width: width,
            src_height: height,
            ref_order_hints: [0; 8],
            codec: VulkanVideoCodec::H264,
            video_session,
            session_params,
            session_memory,
            dpb_slots,
            cur_dpb_idx: 0,
            bitstream_buffer,
            bitstream_memory,
            bitstream_ptr,
            bitstream_capacity: BITSTREAM_CAPACITY,
            query_pool,
            frame_num: 0,
            idr_num: 0,
            force_idr: false,
            qp,
            params_bytes,
            poisoned: false,
        })
    }

    /// Request that the next encode produces an IDR frame.
    #[allow(dead_code)]
    pub(crate) fn request_idr(&mut self) {
        self.force_idr = true;
    }

    /// Retarget the constant quantizer from the next frame onwards.
    ///
    /// Both codecs read `self.qp` per frame — H.264 through the slice's
    /// `constant_qp`, AV1 through `base_q_idx` — so no session rebuild is
    /// needed.  H.264's PPS keeps its original `pic_init_qp_minus26`, which
    /// is harmless because every slice carries an explicit QP.
    #[allow(dead_code)]
    pub(crate) fn set_qp(&mut self, qp: u8) {
        self.qp = qp;
    }

    /// The quantizer currently in effect.
    #[allow(dead_code)]
    pub(crate) fn qp(&self) -> u8 {
        self.qp
    }

    /// Pre-alignment source dimensions the session was built for.  A
    /// bitstream from this session always decodes at this size, whatever
    /// image it was fed.
    pub(crate) fn source_dimensions(&self) -> (u32, u32) {
        (self.src_width, self.src_height)
    }

    /// Codec flag matching `SURFACE_FRAME_CODEC_*` constants.
    /// H.264 = 0x00, AV1 = 0x02.
    pub(crate) fn codec_flag(&self) -> u8 {
        match self.codec {
            VulkanVideoCodec::H264 => 0x00, // SURFACE_FRAME_CODEC_H264
            VulkanVideoCodec::AV1 => 0x02,  // SURFACE_FRAME_CODEC_AV1
        }
    }

    /// Encode one NV12 frame.
    ///
    /// `nv12_image` and `nv12_image_view` must be in
    /// `VK_IMAGE_LAYOUT_VIDEO_ENCODE_SRC_KHR` (or GENERAL).
    ///
    /// Returns `Some((bitstream, is_keyframe))` on success.
    #[allow(clippy::too_many_arguments, dead_code)]
    pub(crate) unsafe fn encode(
        &mut self,
        device: &ash::Device,
        video_fns: &VideoFns,
        encode_queue: vk::Queue,
        encode_cmd_pool: vk::CommandPool,
        nv12_image: vk::Image,
        nv12_image_view: vk::ImageView,
        force_keyframe: bool,
    ) -> Option<(Vec<u8>, bool)> {
        // A previous submission never completed and still owns the bitstream
        // buffer. Refuse rather than submit alongside it.
        //
        // Nothing recovers from here on its own. The server's
        // rebuild-after-repeated-failure path is gated on `needs_new_encoder`,
        // which is hard-`false` whenever a Vulkan encoder exists, so a
        // poisoned one is never torn down — the surface stays black for that
        // client until a resize or resubscribe sends `DestroyVulkanEncoder`.
        // Automatic recovery needs the compositor to tell the server the
        // encoder is dead: "produced no bitstream" is the same signal a
        // warming-up encoder gives, so the server cannot infer it.
        if self.poisoned {
            return None;
        }
        match self.codec {
            VulkanVideoCodec::H264 => unsafe {
                self.encode_h264(
                    device,
                    video_fns,
                    encode_queue,
                    encode_cmd_pool,
                    nv12_image,
                    nv12_image_view,
                    force_keyframe,
                )
            },
            VulkanVideoCodec::AV1 => unsafe {
                self.encode_av1(
                    device,
                    video_fns,
                    encode_queue,
                    encode_cmd_pool,
                    nv12_image,
                    nv12_image_view,
                    force_keyframe,
                )
            },
        }
    }

    /// H.264 encode path.
    #[allow(clippy::too_many_arguments, dead_code)]
    unsafe fn encode_h264(
        &mut self,
        device: &ash::Device,
        video_fns: &VideoFns,
        encode_queue: vk::Queue,
        encode_cmd_pool: vk::CommandPool,
        _nv12_image: vk::Image,
        nv12_image_view: vk::ImageView,
        force_keyframe: bool,
    ) -> Option<(Vec<u8>, bool)> {
        let is_idr = self.force_idr || force_keyframe || self.frame_num == 0;
        if is_idr {
            self.force_idr = false;
        }

        // Allocate command buffer.
        let cb_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(encode_cmd_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cbs = unsafe { device.allocate_command_buffers(&cb_alloc).ok()? };
        let cb = cbs[0];

        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        if unsafe { device.begin_command_buffer(cb, &begin) }.is_err() {
            unsafe { device.free_command_buffers(encode_cmd_pool, &[cb]) };
            return None;
        }

        // Reset query pool.
        unsafe { device.cmd_reset_query_pool(cb, self.query_pool, 0, 1) };

        // --- DPB setup ---
        let setup_dpb_idx = self.cur_dpb_idx;
        let ref_dpb_idx = 1 - self.cur_dpb_idx;

        // Reference info for the reconstructed (setup) picture.
        let mut setup_ref_info: StdVideoEncodeH264ReferenceInfo = unsafe { std::mem::zeroed() };
        setup_ref_info.FrameNum = self.frame_num;
        setup_ref_info.PicOrderCnt = (self.frame_num * 2) as i32;
        setup_ref_info.primary_pic_type = if is_idr {
            StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
        } else {
            StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
        };

        let mut setup_dpb_info =
            vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&setup_ref_info);

        let setup_picture_resource = vk::VideoPictureResourceInfoKHR::default()
            .coded_offset(vk::Offset2D { x: 0, y: 0 })
            .coded_extent(vk::Extent2D {
                width: self.width,
                height: self.height,
            })
            .base_array_layer(0)
            .image_view_binding(self.dpb_slots[setup_dpb_idx].view);

        let setup_slot = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(setup_dpb_idx as i32)
            .picture_resource(&setup_picture_resource)
            .push_next(&mut setup_dpb_info);

        // Reference slot for the previous frame (P-frame reference).
        let mut ref_ref_info: StdVideoEncodeH264ReferenceInfo = unsafe { std::mem::zeroed() };
        let ref_picture_resource;
        let mut ref_dpb_info;
        let ref_slot;

        let mut begin_ref_slots: Vec<vk::VideoReferenceSlotInfoKHR<'_>> = Vec::new();
        // Always include the setup slot in begin coding.
        begin_ref_slots.push(setup_slot);

        if !is_idr {
            ref_ref_info.FrameNum = self.frame_num.wrapping_sub(1);
            ref_ref_info.PicOrderCnt = (self.frame_num.wrapping_sub(1) * 2) as i32;
            ref_ref_info.primary_pic_type = StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P;

            ref_dpb_info =
                vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&ref_ref_info);

            ref_picture_resource = vk::VideoPictureResourceInfoKHR::default()
                .coded_offset(vk::Offset2D { x: 0, y: 0 })
                .coded_extent(vk::Extent2D {
                    width: self.width,
                    height: self.height,
                })
                .base_array_layer(0)
                .image_view_binding(self.dpb_slots[ref_dpb_idx].view);

            ref_slot = vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(ref_dpb_idx as i32)
                .picture_resource(&ref_picture_resource)
                .push_next(&mut ref_dpb_info);

            begin_ref_slots.push(ref_slot);
        }

        // ---------------------------------------------------------------
        // Begin video coding scope
        // ---------------------------------------------------------------
        let begin_coding = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.video_session)
            .video_session_parameters(self.session_params)
            .reference_slots(&begin_ref_slots);

        unsafe { (video_fns.cmd_begin_video_coding)(cb, &begin_coding) };

        // On first frame or IDR, reset the video session and set rate
        // control to disabled (CQP mode -- constant QP per slice).
        if is_idr {
            let mut rate_control = vk::VideoEncodeRateControlInfoKHR::default()
                .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::DISABLED);
            let control_info = vk::VideoCodingControlInfoKHR::default()
                .flags(
                    vk::VideoCodingControlFlagsKHR::RESET
                        | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL,
                )
                .push_next(&mut rate_control);
            unsafe { (video_fns.cmd_control_video_coding)(cb, &control_info) };
        }

        // ---------------------------------------------------------------
        // Fill H.264 encode picture info
        // ---------------------------------------------------------------
        let mut pic_flags: StdVideoEncodeH264PictureInfoFlags = unsafe { std::mem::zeroed() };
        if is_idr {
            pic_flags.set_IdrPicFlag(1);
        }
        pic_flags.set_is_reference(1);

        // Reference lists for P-frames.
        let mut ref_lists: StdVideoEncodeH264ReferenceListsInfo = unsafe { std::mem::zeroed() };
        // Fill RefPicList0 with STD_VIDEO_H264_NO_REFERENCE_PICTURE (0xFF).
        ref_lists.RefPicList0 = [0xFF; 32];
        ref_lists.RefPicList1 = [0xFF; 32];
        if !is_idr {
            ref_lists.num_ref_idx_l0_active_minus1 = 0;
            ref_lists.RefPicList0[0] = ref_dpb_idx as u8;
        }

        let mut std_pic_info: StdVideoEncodeH264PictureInfo = unsafe { std::mem::zeroed() };
        std_pic_info.flags = pic_flags;
        std_pic_info.seq_parameter_set_id = 0;
        std_pic_info.pic_parameter_set_id = 0;
        std_pic_info.idr_pic_id = if is_idr { self.idr_num as u16 } else { 0 };
        std_pic_info.primary_pic_type = if is_idr {
            StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
        } else {
            StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
        };
        std_pic_info.frame_num = self.frame_num;
        std_pic_info.PicOrderCnt = (self.frame_num * 2) as i32;
        std_pic_info.pRefLists = if is_idr { ptr::null() } else { &ref_lists };

        // Slice header.
        let mut slice_hdr: StdVideoEncodeH264SliceHeader = unsafe { std::mem::zeroed() };
        slice_hdr.slice_type = if is_idr {
            StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_I
        } else {
            StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_P
        };
        slice_hdr.cabac_init_idc = StdVideoH264CabacInitIdc_STD_VIDEO_H264_CABAC_INIT_IDC_0;
        slice_hdr.disable_deblocking_filter_idc = StdVideoH264DisableDeblockingFilterIdc_STD_VIDEO_H264_DISABLE_DEBLOCKING_FILTER_IDC_DISABLED;

        let nalu_slice = vk::VideoEncodeH264NaluSliceInfoKHR::default()
            .constant_qp(self.qp as i32)
            .std_slice_header(&slice_hdr);

        let mut h264_pic_info = vk::VideoEncodeH264PictureInfoKHR::default()
            .nalu_slice_entries(std::slice::from_ref(&nalu_slice))
            .std_picture_info(&std_pic_info)
            .generate_prefix_nalu(false);

        // Source picture resource (the NV12 input).
        let src_picture_resource = vk::VideoPictureResourceInfoKHR::default()
            .coded_offset(vk::Offset2D { x: 0, y: 0 })
            .coded_extent(vk::Extent2D {
                width: self.width,
                height: self.height,
            })
            .base_array_layer(0)
            .image_view_binding(nv12_image_view);

        // Encode feedback (the encoded byte count) is collected with an
        // ordinary begin/end query around the encode command.
        //
        // This used to chain `VkVideoInlineQueryInfoKHR` into the encode
        // instead, which is only legal with `VK_KHR_video_maintenance1` —
        // an extension this device never enables and never even probes for.
        // Drivers that ignore the unrecognised pNext simply never wrote the
        // query, and the `get_query_pool_results(WAIT)` below then blocked
        // the compositor thread forever: the Wayland socket stopped being
        // serviced and clients died with VK_ERROR_SURFACE_LOST_KHR.  An
        // explicit query needs no extension and works on every driver.
        unsafe { device.cmd_begin_query(cb, self.query_pool, 0, vk::QueryControlFlags::empty()) };

        // Build the encode info.
        //
        // We need separate paths for IDR (no reference slots) vs P-frame
        // (one reference slot) because the `reference_slots` builder
        // captures a slice reference with a lifetime.
        if is_idr {
            let encode_info = vk::VideoEncodeInfoKHR::default()
                .dst_buffer(self.bitstream_buffer)
                .dst_buffer_offset(0)
                .dst_buffer_range(self.bitstream_capacity)
                .src_picture_resource(src_picture_resource)
                .setup_reference_slot(&setup_slot)
                .push_next(&mut h264_pic_info);

            unsafe { (video_fns.cmd_encode_video)(cb, &encode_info) };
        } else {
            // For P-frames we need the ref_slot; it was pushed into
            // begin_ref_slots above.  Re-build it here for the encode
            // info reference_slots field.
            let mut ref_ref_info2: StdVideoEncodeH264ReferenceInfo = unsafe { std::mem::zeroed() };
            ref_ref_info2.FrameNum = self.frame_num.wrapping_sub(1);
            ref_ref_info2.PicOrderCnt = (self.frame_num.wrapping_sub(1) * 2) as i32;
            ref_ref_info2.primary_pic_type = StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P;

            let mut ref_dpb_info2 =
                vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&ref_ref_info2);

            let ref_picture_resource2 = vk::VideoPictureResourceInfoKHR::default()
                .coded_offset(vk::Offset2D { x: 0, y: 0 })
                .coded_extent(vk::Extent2D {
                    width: self.width,
                    height: self.height,
                })
                .base_array_layer(0)
                .image_view_binding(self.dpb_slots[ref_dpb_idx].view);

            let ref_slot2 = vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(ref_dpb_idx as i32)
                .picture_resource(&ref_picture_resource2)
                .push_next(&mut ref_dpb_info2);

            let encode_info = vk::VideoEncodeInfoKHR::default()
                .dst_buffer(self.bitstream_buffer)
                .dst_buffer_offset(0)
                .dst_buffer_range(self.bitstream_capacity)
                .src_picture_resource(src_picture_resource)
                .setup_reference_slot(&setup_slot)
                .reference_slots(std::slice::from_ref(&ref_slot2))
                .push_next(&mut h264_pic_info);

            unsafe { (video_fns.cmd_encode_video)(cb, &encode_info) };
        }

        unsafe { device.cmd_end_query(cb, self.query_pool, 0) };

        // End video coding.
        let end_coding = vk::VideoEndCodingInfoKHR::default();
        unsafe { (video_fns.cmd_end_video_coding)(cb, &end_coding) };

        // End command buffer.
        if let Err(e) = unsafe { device.end_command_buffer(cb) } {
            eprintln!("[vulkan-encode] vkEndCommandBuffer failed: {e:?}");
            unsafe { device.free_command_buffers(encode_cmd_pool, &[cb]) };
            return None;
        }

        // Submit.
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cb));
        let fence_info = vk::FenceCreateInfo::default();
        let fence = unsafe { device.create_fence(&fence_info, None).ok()? };
        if let Err(e) = unsafe { device.queue_submit(encode_queue, &[submit], fence) } {
            eprintln!("[vulkan-encode] vkQueueSubmit failed: {e:?}");
            unsafe {
                device.destroy_fence(fence, None);
                device.free_command_buffers(encode_cmd_pool, &[cb]);
            }
            return None;
        }

        // Wait for completion. A timeout means the submission is still live
        // on the device, so the fence, the command buffer and the bitstream
        // buffer it writes into are all still in use: freeing or reading any
        // of them here would be a use-after-free the validation layers cannot
        // save us from. Leak them and poison the encoder instead — one fence
        // and one command buffer, once, against wedging the compositor.
        if unsafe { device.wait_for_fences(&[fence], true, encode_fence_timeout_ns()) }.is_err() {
            eprintln!(
                "[vulkan-encode] fence wait timed out after {} ms; abandoning encoder",
                encode_fence_timeout_ns() / 1_000_000
            );
            self.poisoned = true;
            return None;
        }
        unsafe { device.destroy_fence(fence, None) };

        // Read query result (encoded size).
        let mut feedback = [0u32; 1];
        let qr = unsafe {
            device.get_query_pool_results(
                self.query_pool,
                0,
                &mut feedback,
                vk::QueryResultFlags::WAIT,
            )
        };
        unsafe { device.free_command_buffers(encode_cmd_pool, &[cb]) };

        if qr.is_err() {
            eprintln!("[vulkan-encode] query pool result failed: {qr:?}");
            return None;
        }

        let encoded_size = feedback[0] as usize;
        if encoded_size == 0 || encoded_size > self.bitstream_capacity as usize {
            eprintln!(
                "[vulkan-encode] bad encoded size: {encoded_size} (capacity={})",
                self.bitstream_capacity,
            );
            return None;
        }

        // Copy bitstream from mapped pointer, prefixing an IDR with the
        // parameter sets so each keyframe is a self-contained entry point.
        let slices = unsafe { std::slice::from_raw_parts(self.bitstream_ptr, encoded_size) };
        let bitstream = if is_idr {
            let mut b = Vec::with_capacity(self.params_bytes.len() + encoded_size);
            b.extend_from_slice(&self.params_bytes);
            b.extend_from_slice(slices);
            b
        } else {
            slices.to_vec()
        };

        // Update state.
        if is_idr {
            self.frame_num = 0;
            self.idr_num = self.idr_num.wrapping_add(1);
        }
        self.frame_num = self.frame_num.wrapping_add(1);
        self.cur_dpb_idx = 1 - self.cur_dpb_idx;

        Some((bitstream, is_idr))
    }

    // ---------------------------------------------------------------
    // AV1 encoder
    // ---------------------------------------------------------------

    /// Create a Vulkan Video AV1 encoder.
    ///
    /// Returns `None` if the device does not support AV1 encode or any
    /// required step fails.  Mirrors `try_new_h264` but uses
    /// `VK_KHR_video_encode_av1` raw FFI types.
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn try_new_av1(
        device: &ash::Device,
        instance: &ash::Instance,
        physical_device: vk::PhysicalDevice,
        video_fns: &VideoFns,
        video_queue_family: u32,
        width: u32,
        height: u32,
        qp: u8,
    ) -> Option<Self> {
        // The source size is used as the coded extent directly, like the
        // H.264 path: the driver pads to whole superblocks internally, and
        // its frame headers then declare the true size — AV1 has no
        // SPS-style cropping to paper over an aligned extent, and a decoder
        // promised an aligned frame renders the padding rows.
        let coded_w = width;
        let coded_h = height;

        // ---------------------------------------------------------------
        // 1. Video profile
        // ---------------------------------------------------------------
        let mut av1_profile_info = VideoEncodeAV1ProfileInfoKHR {
            s_type: vk::StructureType::from_raw(
                VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_PROFILE_INFO_KHR,
            ),
            p_next: ptr::null(),
            std_profile: STD_VIDEO_AV1_PROFILE_MAIN,
        };

        let profile = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(vk::VideoCodecOperationFlagsKHR::from_raw(
                VK_VIDEO_CODEC_OPERATION_ENCODE_AV1_BIT_KHR,
            ))
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
            .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8);
        // Chain the AV1 profile info via raw pNext.
        let profile = {
            let mut p = profile;
            let base = &mut p as *mut _ as *mut vk::BaseOutStructure<'_>;
            unsafe {
                (*base).p_next = &mut av1_profile_info as *mut _ as *mut vk::BaseOutStructure<'_>;
            }
            p
        };

        // ---------------------------------------------------------------
        // 2. Query capabilities
        // ---------------------------------------------------------------
        let mut av1_caps: VideoEncodeAV1CapabilitiesKHR = unsafe { std::mem::zeroed() };
        av1_caps.s_type =
            vk::StructureType::from_raw(VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_CAPABILITIES_KHR);
        let mut encode_caps = vk::VideoEncodeCapabilitiesKHR::default();
        let mut caps = vk::VideoCapabilitiesKHR::default().push_next(&mut encode_caps);

        // Chain av1_caps via raw pNext.
        {
            let base = &mut caps as *mut _ as *mut vk::BaseOutStructure<'_>;
            // Walk to end of pNext chain.
            let mut cur = base;
            unsafe {
                while !(*cur).p_next.is_null() {
                    cur = (*cur).p_next;
                }
                (*cur).p_next = &mut av1_caps as *mut _ as *mut vk::BaseOutStructure<'_>;
            }
        }

        let res = unsafe {
            (video_fns.get_physical_device_video_capabilities)(physical_device, &profile, &mut caps)
        };
        if res != vk::Result::SUCCESS {
            eprintln!(
                "[vulkan-encode] AV1 vkGetPhysicalDeviceVideoCapabilitiesKHR failed: {res:?}"
            );
            return None;
        }

        let std_header_version = caps.std_header_version;
        let max_coded_w = caps.max_coded_extent.width;
        let max_coded_h = caps.max_coded_extent.height;
        let max_dpb = caps.max_dpb_slots;
        let _ = caps;

        let max_level = av1_caps.max_level;

        eprintln!(
            "[vulkan-encode] AV1 caps: max_coded={max_coded_w}x{max_coded_h}, max_dpb={max_dpb}, max_level={max_level}",
        );

        if coded_w > max_coded_w || coded_h > max_coded_h {
            eprintln!(
                "[vulkan-encode] AV1 coded extent {coded_w}x{coded_h} exceeds max {max_coded_w}x{max_coded_h}",
            );
            return None;
        }

        // Pick a level.
        let level = compute_av1_level(coded_w, coded_h).min(max_level);

        // ---------------------------------------------------------------
        // 3. Create video session
        // ---------------------------------------------------------------
        let mut av1_session_create = VideoEncodeAV1SessionCreateInfoKHR {
            s_type: vk::StructureType::from_raw(
                VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_SESSION_CREATE_INFO_KHR,
            ),
            p_next: ptr::null(),
            use_max_level: vk::TRUE,
            max_level: level,
        };

        let coded_extent = vk::Extent2D {
            width: coded_w,
            height: coded_h,
        };

        let mut session_create = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(video_queue_family)
            .video_profile(&profile)
            .picture_format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .max_coded_extent(coded_extent)
            .reference_picture_format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .max_dpb_slots(2)
            .max_active_reference_pictures(1)
            .std_header_version(&std_header_version);

        // Chain av1_session_create via raw pNext.
        {
            let base = &mut session_create as *mut _ as *mut vk::BaseOutStructure<'_>;
            unsafe {
                let mut cur = base;
                while !(*cur).p_next.is_null() {
                    cur = (*cur).p_next;
                }
                (*cur).p_next = &mut av1_session_create as *mut _ as *mut vk::BaseOutStructure<'_>;
            }
        }

        let mut video_session = vk::VideoSessionKHR::null();
        let res = unsafe {
            (video_fns.create_video_session)(
                device.handle(),
                &session_create,
                ptr::null(),
                &mut video_session,
            )
        };
        if res != vk::Result::SUCCESS {
            eprintln!("[vulkan-encode] AV1 vkCreateVideoSessionKHR failed: {res:?}");
            return None;
        }

        // ---------------------------------------------------------------
        // 4. Query and bind session memory
        // ---------------------------------------------------------------
        let session_memory = unsafe {
            bind_session_memory(device, video_fns, video_session, physical_device, instance)
        }?;

        // ---------------------------------------------------------------
        // 5. Session parameters (AV1 sequence header)
        // ---------------------------------------------------------------
        let color_config = StdVideoAV1ColorConfig {
            // Bit 1 = color_range (bit 0 is mono_chrome): full swing.
            // blit's pixels are full-range BT.601 end to end.
            flags: 1 << 1,
            bit_depth: 8,
            subsampling_x: 1,
            subsampling_y: 1,
            _reserved1: 0,
            color_primaries: 2,          // CP_UNSPECIFIED
            transfer_characteristics: 2, // TC_UNSPECIFIED
            matrix_coefficients: 2,      // MC_UNSPECIFIED
            chroma_sample_position: 0,   // Unknown
        };

        let mut seq_flags = StdVideoAV1SequenceHeaderFlags::new();
        seq_flags.set_enable_order_hint(true);
        // NVIDIA's encoder codes per-superblock cdef_idx symbols in the tile
        // data unconditionally.  With CDEF declared off, decoders don't
        // expect those symbols and fail the whole tile ("Failed to decode
        // tile data" in libaom) — so declare it on and let the driver write
        // the frame-level CDEF parameters it actually used (it overrides
        // frame-header fields like loop_filter_level regardless of what the
        // std picture info says).
        seq_flags.set_enable_cdef(true);

        // The sequence header must declare the coded extent: the driver's
        // tile payload covers whole superblocks of it, and a decoder that
        // was promised a smaller frame errors out mid-tile (dav1d rejects
        // every frame).  The source size is carried as AV1 `render_size`
        // instead — the per-frame display hint AV1 uses where H.264 has SPS
        // cropping.
        let w_bits = 32u32.saturating_sub(coded_w.leading_zeros()).max(1);
        let h_bits = 32u32.saturating_sub(coded_h.leading_zeros()).max(1);

        let mut seq_header: StdVideoAV1SequenceHeader = unsafe { std::mem::zeroed() };
        seq_header.flags = seq_flags;
        seq_header.seq_profile = STD_VIDEO_AV1_PROFILE_MAIN;
        seq_header.frame_width_bits_minus_1 = (w_bits - 1) as u8;
        seq_header.frame_height_bits_minus_1 = (h_bits - 1) as u8;
        seq_header.max_frame_width_minus_1 = (coded_w - 1) as u16;
        seq_header.max_frame_height_minus_1 = (coded_h - 1) as u16;
        seq_header.order_hint_bits_minus_1 = 6; // 7-bit order hint
        seq_header.seq_force_integer_mv = 2; // SELECT_INTEGER_MV
        seq_header.seq_force_screen_content_tools = 2; // SELECT_SCREEN_CONTENT_TOOLS
        seq_header.p_color_config = &color_config;
        seq_header.p_timing_info = ptr::null();

        let mut av1_params_create = VideoEncodeAV1SessionParametersCreateInfoKHR {
            s_type: vk::StructureType::from_raw(
                VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_SESSION_PARAMETERS_CREATE_INFO_KHR,
            ),
            p_next: ptr::null(),
            p_std_sequence_header: &seq_header,
            p_std_decoder_model_info: ptr::null(),
            std_operating_point_count: 0,
            p_std_operating_points: ptr::null(),
        };

        let mut params_create =
            vk::VideoSessionParametersCreateInfoKHR::default().video_session(video_session);

        // Chain AV1 params via raw pNext.
        {
            let base = &mut params_create as *mut _ as *mut vk::BaseOutStructure<'_>;
            unsafe {
                let mut cur = base;
                while !(*cur).p_next.is_null() {
                    cur = (*cur).p_next;
                }
                (*cur).p_next = &mut av1_params_create as *mut _ as *mut vk::BaseOutStructure<'_>;
            }
        }

        let mut session_params = vk::VideoSessionParametersKHR::null();
        let res = unsafe {
            (video_fns.create_video_session_parameters)(
                device.handle(),
                &params_create,
                ptr::null(),
                &mut session_params,
            )
        };
        if res != vk::Result::SUCCESS {
            eprintln!("[vulkan-encode] AV1 vkCreateVideoSessionParametersKHR failed: {res:?}");
            for &m in &session_memory {
                unsafe { device.free_memory(m, None) };
            }
            unsafe {
                (video_fns.destroy_video_session)(device.handle(), video_session, ptr::null());
            }
            return None;
        }

        // ---------------------------------------------------------------
        // 6. DPB images (2x)
        // ---------------------------------------------------------------
        let dpb_slots = unsafe {
            allocate_dpb_slots(
                device,
                instance,
                physical_device,
                video_fns,
                coded_w,
                coded_h,
                video_queue_family,
                &profile,
                // AV1 here is 4:2:0 only — see `create_nv12_encode_image`.
                vk::Format::G8_B8R8_2PLANE_420_UNORM,
                &session_memory,
                session_params,
                video_session,
            )
        }?;

        // ---------------------------------------------------------------
        // 7. Bitstream buffer
        // ---------------------------------------------------------------
        let (bitstream_buffer, bitstream_memory, bitstream_ptr) = unsafe {
            allocate_bitstream_buffer(
                device,
                instance,
                physical_device,
                video_fns,
                BITSTREAM_CAPACITY,
                &profile,
                &dpb_slots,
                &session_memory,
                session_params,
                video_session,
            )
        }?;

        // ---------------------------------------------------------------
        // 8. Query pool (encode feedback)
        // ---------------------------------------------------------------
        let mut av1_profile_for_qp = VideoEncodeAV1ProfileInfoKHR {
            s_type: vk::StructureType::from_raw(
                VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_PROFILE_INFO_KHR,
            ),
            p_next: ptr::null(),
            std_profile: STD_VIDEO_AV1_PROFILE_MAIN,
        };
        let mut video_profile_for_query = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(vk::VideoCodecOperationFlagsKHR::from_raw(
                VK_VIDEO_CODEC_OPERATION_ENCODE_AV1_BIT_KHR,
            ))
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
            .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8);
        // Chain av1 profile via raw pNext.
        {
            let base = &mut video_profile_for_query as *mut _ as *mut vk::BaseOutStructure<'_>;
            unsafe {
                let mut cur = base;
                while !(*cur).p_next.is_null() {
                    cur = (*cur).p_next;
                }
                (*cur).p_next = &mut av1_profile_for_qp as *mut _ as *mut vk::BaseOutStructure<'_>;
            }
        }
        let query_pool = unsafe {
            create_encode_query_pool(
                device,
                video_fns,
                &mut video_profile_for_query,
                bitstream_buffer,
                bitstream_memory,
                &dpb_slots,
                &session_memory,
                session_params,
                video_session,
            )
        }?;

        eprintln!(
            "[vulkan-encode] initialized AV1 encoder {coded_w}x{coded_h} (source {width}x{height}) qp={qp} level={level}",
        );

        Some(Self {
            width: coded_w,
            height: coded_h,
            src_width: width,
            src_height: height,
            ref_order_hints: [0; 8],
            codec: VulkanVideoCodec::AV1,
            video_session,
            session_params,
            session_memory,
            dpb_slots,
            cur_dpb_idx: 0,
            bitstream_buffer,
            bitstream_memory,
            bitstream_ptr,
            bitstream_capacity: BITSTREAM_CAPACITY,
            query_pool,
            frame_num: 0,
            idr_num: 0,
            force_idr: false,
            qp,
            // The driver emits frame OBUs only; the sequence header is ours
            // to serialize (from the same values `seq_header` was built
            // with) and gets prepended to every keyframe, mirroring how
            // H.264 prepends its SPS/PPS.
            params_bytes: av1_sequence_header_obu(level, w_bits, h_bits, coded_w, coded_h),
            poisoned: false,
        })
    }

    /// AV1 encode path.
    #[allow(clippy::too_many_arguments, dead_code)]
    unsafe fn encode_av1(
        &mut self,
        device: &ash::Device,
        video_fns: &VideoFns,
        encode_queue: vk::Queue,
        encode_cmd_pool: vk::CommandPool,
        _nv12_image: vk::Image,
        nv12_image_view: vk::ImageView,
        force_keyframe: bool,
    ) -> Option<(Vec<u8>, bool)> {
        let is_key = self.force_idr || force_keyframe || self.frame_num == 0;
        if is_key {
            self.force_idr = false;
        }

        // Allocate command buffer.
        let cb_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(encode_cmd_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cbs = unsafe { device.allocate_command_buffers(&cb_alloc).ok()? };
        let cb = cbs[0];

        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        if unsafe { device.begin_command_buffer(cb, &begin) }.is_err() {
            unsafe { device.free_command_buffers(encode_cmd_pool, &[cb]) };
            return None;
        }

        // Reset query pool.
        unsafe { device.cmd_reset_query_pool(cb, self.query_pool, 0, 1) };

        // 7-bit order hint.  A keyframe restarts the GOP at 0 — `frame_num`
        // is only reset *after* this encode, so without the `is_key` arm a
        // forced keyframe would carry the stale hint and its deltas would
        // then count backwards from it.
        let order_hint = if is_key {
            0
        } else {
            (self.frame_num & 0x7F) as u8
        };

        // --- DPB setup ---
        let setup_dpb_idx = self.cur_dpb_idx;
        let ref_dpb_idx = 1 - self.cur_dpb_idx;

        // AV1 DPB slot info for the reconstructed (setup) picture.
        let setup_ref_info = StdVideoEncodeAV1ReferenceInfo {
            flags: StdVideoEncodeAV1ReferenceInfoFlags { bits: 0 },
            ref_frame_id: self.frame_num,
            frame_type: if is_key {
                STD_VIDEO_AV1_FRAME_TYPE_KEY
            } else {
                STD_VIDEO_AV1_FRAME_TYPE_INTER
            },
            order_hint,
            _reserved: [0; 3],
            p_extension_header: ptr::null(),
        };

        let setup_dpb_info = VideoEncodeAV1DpbSlotInfoKHR {
            s_type: vk::StructureType::from_raw(
                VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_DPB_SLOT_INFO_KHR,
            ),
            p_next: ptr::null(),
            p_std_reference_info: &setup_ref_info,
        };

        let setup_picture_resource = vk::VideoPictureResourceInfoKHR::default()
            .coded_offset(vk::Offset2D { x: 0, y: 0 })
            .coded_extent(vk::Extent2D {
                width: self.width,
                height: self.height,
            })
            .base_array_layer(0)
            .image_view_binding(self.dpb_slots[setup_dpb_idx].view);

        let mut setup_slot = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(setup_dpb_idx as i32)
            .picture_resource(&setup_picture_resource);
        // Chain dpb info via raw pNext.
        {
            let base = &mut setup_slot as *mut _ as *mut vk::BaseOutStructure<'_>;
            unsafe {
                (*base).p_next = &setup_dpb_info as *const _ as *mut vk::BaseOutStructure<'_>;
            }
        }

        let mut begin_ref_slots: Vec<vk::VideoReferenceSlotInfoKHR<'_>> = Vec::new();
        begin_ref_slots.push(setup_slot);

        // Reference slot for previous frame (P-frame reference).
        let ref_ref_info;
        let ref_dpb_info;
        let ref_picture_resource;
        let mut ref_slot;
        if !is_key {
            ref_ref_info = StdVideoEncodeAV1ReferenceInfo {
                flags: StdVideoEncodeAV1ReferenceInfoFlags { bits: 0 },
                ref_frame_id: self.frame_num.wrapping_sub(1),
                frame_type: STD_VIDEO_AV1_FRAME_TYPE_INTER,
                order_hint: ((self.frame_num.wrapping_sub(1)) & 0x7F) as u8,
                _reserved: [0; 3],
                p_extension_header: ptr::null(),
            };
            ref_dpb_info = VideoEncodeAV1DpbSlotInfoKHR {
                s_type: vk::StructureType::from_raw(
                    VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_DPB_SLOT_INFO_KHR,
                ),
                p_next: ptr::null(),
                p_std_reference_info: &ref_ref_info,
            };
            ref_picture_resource = vk::VideoPictureResourceInfoKHR::default()
                .coded_offset(vk::Offset2D { x: 0, y: 0 })
                .coded_extent(vk::Extent2D {
                    width: self.width,
                    height: self.height,
                })
                .base_array_layer(0)
                .image_view_binding(self.dpb_slots[ref_dpb_idx].view);
            ref_slot = vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(ref_dpb_idx as i32)
                .picture_resource(&ref_picture_resource);
            {
                let base = &mut ref_slot as *mut _ as *mut vk::BaseOutStructure<'_>;
                unsafe {
                    (*base).p_next = &ref_dpb_info as *const _ as *mut vk::BaseOutStructure<'_>;
                }
            }
            begin_ref_slots.push(ref_slot);
        }

        // ---------------------------------------------------------------
        // Begin video coding scope
        // ---------------------------------------------------------------
        let begin_coding = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.video_session)
            .video_session_parameters(self.session_params)
            .reference_slots(&begin_ref_slots);

        unsafe { (video_fns.cmd_begin_video_coding)(cb, &begin_coding) };

        // On key frame, reset session and disable rate control (CQP).
        if is_key {
            let mut rate_control = vk::VideoEncodeRateControlInfoKHR::default()
                .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::DISABLED);
            let control_info = vk::VideoCodingControlInfoKHR::default()
                .flags(
                    vk::VideoCodingControlFlagsKHR::RESET
                        | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL,
                )
                .push_next(&mut rate_control);
            unsafe { (video_fns.cmd_control_video_coding)(cb, &control_info) };
        }

        // ---------------------------------------------------------------
        // Fill AV1 encode picture info
        // ---------------------------------------------------------------
        let mut pic_flags = StdVideoEncodeAV1PictureInfoFlags::new();
        pic_flags.set_error_resilient_mode(is_key);
        pic_flags.set_force_integer_mv(is_key);
        pic_flags.set_show_frame(true);
        // Frame size is the coded extent; tell decoders the display size
        // via render_size, AV1's stand-in for H.264 SPS cropping.
        if (self.src_width, self.src_height) != (self.width, self.height) {
            pic_flags.set_render_and_frame_size_different(true);
        }

        let mut ref_frame_idx = [-1i8; 7];
        if !is_key {
            // LAST_FRAME (index 0) points to the ref DPB slot.
            ref_frame_idx[0] = ref_dpb_idx as i8;
        }

        // What each decoder-side reference slot's order hint will be when
        // this frame is decoded — the driver writes these into the frame
        // header, and a decoder cross-checks them against its own slots.
        let ref_order_hint = if is_key {
            [0u8; 8]
        } else {
            self.ref_order_hints
        };

        // Tile layout, quantization, loop filter, CDEF and loop restoration
        // are all left to the driver (null pointers), exactly like NVIDIA's
        // reference encoder does by default: these describe what the
        // hardware *will do*, and hand-built values it does not honor end
        // up in frame headers that contradict the tile data — decoders
        // fail the whole tile.  Same reasoning for `tx_mode` and
        // `interpolation_filter`: zero-initialized, driver's choice.
        let std_pic_info = StdVideoEncodeAV1PictureInfo {
            flags: pic_flags,
            frame_type: if is_key {
                STD_VIDEO_AV1_FRAME_TYPE_KEY
            } else {
                STD_VIDEO_AV1_FRAME_TYPE_INTER
            },
            frame_presentation_time: 0,
            current_frame_id: self.frame_num,
            order_hint,
            primary_ref_frame: if is_key { 7 } else { 0 }, // 7 = PRIMARY_REF_NONE
            refresh_frame_flags: if is_key {
                0xFF
            } else {
                1u8 << (setup_dpb_idx as u8)
            },
            coded_denom: 0,
            render_width_minus_1: (self.src_width - 1) as u16,
            render_height_minus_1: (self.src_height - 1) as u16,
            interpolation_filter: 0, // EIGHTTAP — driver overrides as needed
            tx_mode: 0,
            delta_q_res: 0,
            delta_lf_res: 0,
            ref_order_hint,
            ref_frame_idx,
            _reserved1: [0; 3],
            delta_frame_id_minus_1: [0; 7],
            p_tile_info: ptr::null(),
            p_quantization: ptr::null(),
            p_segmentation: ptr::null(),
            p_loop_filter: ptr::null(),
            p_cdef: ptr::null(),
            p_loop_restoration: ptr::null(),
            p_global_motion: ptr::null(),
            p_extension_header: ptr::null(),
            p_buffer_removal_times: ptr::null(),
        };

        let mut reference_name_slot_indices = [-1i32; 7];
        if !is_key {
            // LAST_FRAME name slot index.
            reference_name_slot_indices[0] = ref_dpb_idx as i32;
        }

        let av1_pic_info = VideoEncodeAV1PictureInfoKHR {
            s_type: vk::StructureType::from_raw(
                VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_PICTURE_INFO_KHR,
            ),
            p_next: ptr::null(),
            prediction_mode: if is_key {
                VK_VIDEO_ENCODE_AV1_PREDICTION_MODE_INTRA_ONLY_KHR
            } else {
                VK_VIDEO_ENCODE_AV1_PREDICTION_MODE_SINGLE_REFERENCE_KHR
            },
            rate_control_group: if is_key {
                VK_VIDEO_ENCODE_AV1_RATE_CONTROL_GROUP_INTRA_KHR
            } else {
                VK_VIDEO_ENCODE_AV1_RATE_CONTROL_GROUP_PREDICTIVE_KHR
            },
            constant_q_index: self.qp as u32,
            p_std_picture_info: &std_pic_info,
            reference_name_slot_indices,
            primary_reference_cdf_only: vk::FALSE,
            generate_obu_extension_header: vk::FALSE,
        };

        // Source picture resource (the NV12 input).
        let src_picture_resource = vk::VideoPictureResourceInfoKHR::default()
            .coded_offset(vk::Offset2D { x: 0, y: 0 })
            .coded_extent(vk::Extent2D {
                width: self.width,
                height: self.height,
            })
            .base_array_layer(0)
            .image_view_binding(nv12_image_view);

        // Explicit begin/end query rather than `VkVideoInlineQueryInfoKHR`,
        // which needs `VK_KHR_video_maintenance1` — see the H.264 path for
        // why relying on it hung the compositor thread.
        unsafe { device.cmd_begin_query(cb, self.query_pool, 0, vk::QueryControlFlags::empty()) };

        // Build encode info.
        if is_key {
            let mut encode_info = vk::VideoEncodeInfoKHR::default()
                .dst_buffer(self.bitstream_buffer)
                .dst_buffer_offset(0)
                .dst_buffer_range(self.bitstream_capacity)
                .src_picture_resource(src_picture_resource)
                .setup_reference_slot(&setup_slot);

            // Chain av1_pic_info via raw pNext.
            {
                let base = &mut encode_info as *mut _ as *mut vk::BaseOutStructure<'_>;
                unsafe {
                    let mut cur = base;
                    while !(*cur).p_next.is_null() {
                        cur = (*cur).p_next;
                    }
                    (*cur).p_next = &av1_pic_info as *const _ as *mut vk::BaseOutStructure<'_>;
                }
            }

            unsafe { (video_fns.cmd_encode_video)(cb, &encode_info) };
        } else {
            // P-frame: rebuild ref_slot for encode info.
            let ref_ref_info2 = StdVideoEncodeAV1ReferenceInfo {
                flags: StdVideoEncodeAV1ReferenceInfoFlags { bits: 0 },
                ref_frame_id: self.frame_num.wrapping_sub(1),
                frame_type: STD_VIDEO_AV1_FRAME_TYPE_INTER,
                order_hint: ((self.frame_num.wrapping_sub(1)) & 0x7F) as u8,
                _reserved: [0; 3],
                p_extension_header: ptr::null(),
            };
            let ref_dpb_info2 = VideoEncodeAV1DpbSlotInfoKHR {
                s_type: vk::StructureType::from_raw(
                    VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_DPB_SLOT_INFO_KHR,
                ),
                p_next: ptr::null(),
                p_std_reference_info: &ref_ref_info2,
            };
            let ref_picture_resource2 = vk::VideoPictureResourceInfoKHR::default()
                .coded_offset(vk::Offset2D { x: 0, y: 0 })
                .coded_extent(vk::Extent2D {
                    width: self.width,
                    height: self.height,
                })
                .base_array_layer(0)
                .image_view_binding(self.dpb_slots[ref_dpb_idx].view);
            let mut ref_slot2 = vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(ref_dpb_idx as i32)
                .picture_resource(&ref_picture_resource2);
            {
                let base = &mut ref_slot2 as *mut _ as *mut vk::BaseOutStructure<'_>;
                unsafe {
                    (*base).p_next = &ref_dpb_info2 as *const _ as *mut vk::BaseOutStructure<'_>;
                }
            }

            let mut encode_info = vk::VideoEncodeInfoKHR::default()
                .dst_buffer(self.bitstream_buffer)
                .dst_buffer_offset(0)
                .dst_buffer_range(self.bitstream_capacity)
                .src_picture_resource(src_picture_resource)
                .setup_reference_slot(&setup_slot)
                .reference_slots(std::slice::from_ref(&ref_slot2));

            // Chain av1_pic_info.
            {
                let base = &mut encode_info as *mut _ as *mut vk::BaseOutStructure<'_>;
                unsafe {
                    let mut cur = base;
                    while !(*cur).p_next.is_null() {
                        cur = (*cur).p_next;
                    }
                    (*cur).p_next = &av1_pic_info as *const _ as *mut vk::BaseOutStructure<'_>;
                }
            }

            unsafe { (video_fns.cmd_encode_video)(cb, &encode_info) };
        }

        unsafe { device.cmd_end_query(cb, self.query_pool, 0) };

        // End video coding.
        let end_coding = vk::VideoEndCodingInfoKHR::default();
        unsafe { (video_fns.cmd_end_video_coding)(cb, &end_coding) };

        // End command buffer.
        if let Err(e) = unsafe { device.end_command_buffer(cb) } {
            eprintln!("[vulkan-encode] vkEndCommandBuffer failed: {e:?}");
            unsafe { device.free_command_buffers(encode_cmd_pool, &[cb]) };
            return None;
        }

        // Submit.
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cb));
        let fence_info = vk::FenceCreateInfo::default();
        let fence = unsafe { device.create_fence(&fence_info, None).ok()? };
        if let Err(e) = unsafe { device.queue_submit(encode_queue, &[submit], fence) } {
            eprintln!("[vulkan-encode] vkQueueSubmit failed: {e:?}");
            unsafe {
                device.destroy_fence(fence, None);
                device.free_command_buffers(encode_cmd_pool, &[cb]);
            }
            return None;
        }

        // Wait for completion. A timeout means the submission is still live
        // on the device, so the fence, the command buffer and the bitstream
        // buffer it writes into are all still in use: freeing or reading any
        // of them here would be a use-after-free the validation layers cannot
        // save us from. Leak them and poison the encoder instead — one fence
        // and one command buffer, once, against wedging the compositor.
        if unsafe { device.wait_for_fences(&[fence], true, encode_fence_timeout_ns()) }.is_err() {
            eprintln!(
                "[vulkan-encode] fence wait timed out after {} ms; abandoning encoder",
                encode_fence_timeout_ns() / 1_000_000
            );
            self.poisoned = true;
            return None;
        }
        unsafe { device.destroy_fence(fence, None) };

        // Read query result.
        let mut feedback = [0u32; 1];
        let qr = unsafe {
            device.get_query_pool_results(
                self.query_pool,
                0,
                &mut feedback,
                vk::QueryResultFlags::WAIT,
            )
        };
        unsafe { device.free_command_buffers(encode_cmd_pool, &[cb]) };

        if qr.is_err() {
            eprintln!("[vulkan-encode] AV1 query pool result failed: {qr:?}");
            return None;
        }

        let encoded_size = feedback[0] as usize;
        if encoded_size == 0 || encoded_size > self.bitstream_capacity as usize {
            eprintln!(
                "[vulkan-encode] AV1 bad encoded size: {encoded_size} (capacity={})",
                self.bitstream_capacity,
            );
            return None;
        }

        // Copy the bitstream out.  The driver emits bare frame OBUs (see
        // `params_bytes` in `try_new_av1`), but the low-overhead bitstream
        // format wants each temporal unit to open with a temporal-delimiter
        // OBU — parsers use it to split units, and dav1d refuses a stream
        // without one.  A keyframe additionally gets the sequence header, so
        // each is a self-contained entry point.
        const TEMPORAL_DELIMITER: [u8; 2] = [0x12, 0x00];
        let slices = unsafe { std::slice::from_raw_parts(self.bitstream_ptr, encoded_size) };
        let mut bitstream = Vec::with_capacity(2 + self.params_bytes.len() + encoded_size);
        bitstream.extend_from_slice(&TEMPORAL_DELIMITER);
        if is_key {
            bitstream.extend_from_slice(&self.params_bytes);
        }
        bitstream.extend_from_slice(slices);

        // Update state.
        if is_key {
            self.frame_num = 0;
            self.idr_num = self.idr_num.wrapping_add(1);
            // refresh_frame_flags was 0xFF: every slot now holds this frame.
            self.ref_order_hints = [order_hint; 8];
        } else {
            self.ref_order_hints[setup_dpb_idx & 7] = order_hint;
        }
        self.frame_num = self.frame_num.wrapping_add(1);
        self.cur_dpb_idx = 1 - self.cur_dpb_idx;

        Some((bitstream, is_key))
    }

    /// Destroy all resources.  Must be called before the device is destroyed.
    ///
    /// A poisoned encoder is the exception: it leaks instead.  See below.
    #[allow(dead_code)]
    pub(crate) unsafe fn destroy(&mut self, device: &ash::Device, video_fns: &VideoFns) {
        // The abandoned submission is still live on the device and still owns
        // the bitstream buffer it writes into, the query pool it reports into,
        // and every DPB image it reads and references.  Freeing them here is
        // the same use-after-free the timeout path leaks a fence and a command
        // buffer to avoid — and it is not hypothetical: tearing this encoder
        // down on a resize or resubscribe is the *only* way a client recovers
        // from a poisoned one, so the recovery path is the trigger.
        //
        // There is no safe point to free them.  A `device_wait_idle` here
        // would wait on the very submission that already failed to signal, so
        // it either hangs — reinstating the wedge this whole change exists to
        // remove — or reports a lost device, after which the frees are moot.
        // So leak, once, per encoder that hit a hang the driver never resolved.
        if self.poisoned {
            eprintln!(
                "[vulkan-encode] leaking the resources of a poisoned encoder: \
                 an abandoned submission still owns them",
            );
            return;
        }
        unsafe {
            device.destroy_query_pool(self.query_pool, None);
            device.unmap_memory(self.bitstream_memory);
            device.free_memory(self.bitstream_memory, None);
            device.destroy_buffer(self.bitstream_buffer, None);
            for slot in &self.dpb_slots {
                destroy_dpb_slot(device, slot);
            }
            (video_fns.destroy_video_session_parameters)(
                device.handle(),
                self.session_params,
                ptr::null(),
            );
            for &m in &self.session_memory {
                device.free_memory(m, None);
            }
            (video_fns.destroy_video_session)(device.handle(), self.video_session, ptr::null());
        }
    }
}

// ===================================================================
// Helpers
// ===================================================================

/// Find a memory type matching the given type bits and required properties.
fn find_memory_type(
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..mem_props.memory_type_count).find(|&i| {
        (type_bits & (1 << i)) != 0
            && mem_props.memory_types[i as usize]
                .property_flags
                .contains(required)
    })
}

/// Compute the H.264 level IDC for a given resolution.
///
/// Mirrors the logic in the VA-API encoder: pick the lowest level whose
/// MaxFS (max macroblocks per frame) accommodates the coded picture.
fn compute_level_idc(width: u32, height: u32) -> StdVideoH264LevelIdc {
    let width_in_mbs = (width + 15) / 16;
    let height_in_mbs = (height + 15) / 16;
    let max_fs = width_in_mbs * height_in_mbs;

    if max_fs <= 1620 {
        // Level 3.1: 1280x720
        StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_1
    } else if max_fs <= 8192 {
        // Level 4.0: 2048x1080
        StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_0
    } else if max_fs <= 22080 {
        // Level 5.0: 3672x1536
        StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_0
    } else if max_fs <= 36864 {
        // Level 5.1: 4096x2160
        StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_1
    } else {
        // Level 5.2: 4096x2304
        StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_2
    }
}

/// H.264 Exp-Golomb bit writer for hand-serializing parameter sets.
///
/// Used when the driver's own serializer refuses: NVIDIA (595.84) fails
/// `vkGetEncodedVideoSessionParametersKHR` for a High 4:4:4 Predictive PPS
/// with `ERROR_OUT_OF_HOST_MEMORY` in both the size-query and write forms,
/// while the encode session itself works — the same shape as AV1, where no
/// serializer exists at all and the application writes the header itself.
struct H264BitWriter {
    bytes: Vec<u8>,
    used: u8,
}

impl H264BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            used: 0,
        }
    }

    fn u(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            if self.used == 0 {
                self.bytes.push(0);
            }
            let bit = ((v >> i) & 1) as u8;
            *self.bytes.last_mut().unwrap() |= bit << (7 - self.used);
            self.used = (self.used + 1) & 7;
        }
    }

    /// ue(v): Exp-Golomb.
    fn ue(&mut self, v: u32) {
        let cw = v + 1;
        let bits = 32 - cw.leading_zeros();
        self.u(bits - 1, 0);
        self.u(bits, cw);
    }

    /// se(v): signed Exp-Golomb.
    fn se(&mut self, v: i32) {
        let mapped = if v <= 0 {
            (-2 * v) as u32
        } else {
            (2 * v - 1) as u32
        };
        self.ue(mapped);
    }

    /// rbsp_trailing_bits + emulation prevention + Annex B start code and
    /// NAL header.
    fn into_nal(mut self, nal_ref_idc: u8, nal_unit_type: u8) -> Vec<u8> {
        self.u(1, 1); // rbsp_stop_one_bit
        while self.used != 0 {
            self.u(1, 0);
        }
        let mut out = vec![0, 0, 0, 1, (nal_ref_idc << 5) | nal_unit_type];
        let mut zeros = 0u32;
        for &b in &self.bytes {
            if zeros >= 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        out
    }
}

/// Numeric `level_idc` for a `StdVideoH264LevelIdc` enum value (which counts
/// levels in order, not by their H.264 numbering).
fn h264_level_idc_value(level: StdVideoH264LevelIdc) -> u32 {
    const LEVELS: [u32; 19] = [
        10, 11, 12, 13, 20, 21, 22, 30, 31, 32, 40, 41, 42, 50, 51, 52, 60, 61, 62,
    ];
    LEVELS.get(level as usize).copied().unwrap_or(51)
}

/// Serialize the SPS + PPS `try_new_h264` handed the driver, as Annex B
/// NALs.  Field for field the same values as the `StdVideoH264*ParameterSet`
/// structs — keep them in lockstep, exactly like the AV1 sequence header.
fn h264_parameter_sets(
    sps: &StdVideoH264SequenceParameterSet,
    pps: &StdVideoH264PictureParameterSet,
    is_444: bool,
) -> Vec<u8> {
    let mut w = H264BitWriter::new();
    w.u(8, sps.profile_idc);
    w.u(8, 0); // constraint_set*_flag + reserved_zero_2bits
    w.u(8, h264_level_idc_value(sps.level_idc));
    w.ue(sps.seq_parameter_set_id as u32);
    // profile_idc 100/244 branch (both High-family profiles we emit).
    w.ue(sps.chroma_format_idc);
    if sps.chroma_format_idc == StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_444 {
        w.u(1, 0); // separate_colour_plane_flag
    }
    w.ue(sps.bit_depth_luma_minus8 as u32);
    w.ue(sps.bit_depth_chroma_minus8 as u32);
    w.u(1, 0); // qpprime_y_zero_transform_bypass_flag
    w.u(1, 0); // seq_scaling_matrix_present_flag
    w.ue(sps.log2_max_frame_num_minus4 as u32);
    w.ue(sps.pic_order_cnt_type); // 2: nothing further
    w.ue(sps.max_num_ref_frames as u32);
    w.u(1, 0); // gaps_in_frame_num_value_allowed_flag
    w.ue(sps.pic_width_in_mbs_minus1);
    w.ue(sps.pic_height_in_map_units_minus1);
    w.u(1, 1); // frame_mbs_only_flag
    w.u(1, 1); // direct_8x8_inference_flag
    let cropping = sps.frame_crop_right_offset != 0 || sps.frame_crop_bottom_offset != 0;
    w.u(1, cropping as u32);
    if cropping {
        w.ue(0);
        w.ue(sps.frame_crop_right_offset);
        w.ue(0);
        w.ue(sps.frame_crop_bottom_offset);
    }
    let has_vui =
        sps.flags.vui_parameters_present_flag() != 0 && !sps.pSequenceParameterSetVui.is_null();
    w.u(1, has_vui as u32); // vui_parameters_present_flag
    if has_vui {
        let vui = unsafe { &*sps.pSequenceParameterSetVui };
        w.u(1, vui.flags.aspect_ratio_info_present_flag());
        w.u(1, vui.flags.overscan_info_present_flag());
        let signal = vui.flags.video_signal_type_present_flag();
        w.u(1, signal);
        if signal != 0 {
            w.u(3, vui.video_format as u32);
            w.u(1, vui.flags.video_full_range_flag());
            w.u(1, vui.flags.color_description_present_flag());
        }
        w.u(1, vui.flags.chroma_loc_info_present_flag());
        w.u(1, vui.flags.timing_info_present_flag());
        w.u(1, vui.flags.nal_hrd_parameters_present_flag());
        w.u(1, vui.flags.vcl_hrd_parameters_present_flag());
        w.u(1, 0); // pic_struct_present_flag
        w.u(1, vui.flags.bitstream_restriction_flag());
    }
    let mut out = w.into_nal(3, 7);

    let mut w = H264BitWriter::new();
    w.ue(pps.pic_parameter_set_id as u32);
    w.ue(pps.seq_parameter_set_id as u32);
    w.u(1, 1); // entropy_coding_mode_flag (CABAC)
    w.u(1, 0); // bottom_field_pic_order_in_frame_present_flag
    w.ue(0); // num_slice_groups_minus1
    w.ue(pps.num_ref_idx_l0_default_active_minus1 as u32);
    w.ue(pps.num_ref_idx_l1_default_active_minus1 as u32);
    w.u(1, 0); // weighted_pred_flag
    w.u(2, 0); // weighted_bipred_idc (DEFAULT)
    w.se(pps.pic_init_qp_minus26 as i32);
    w.se(0); // pic_init_qs_minus26
    w.se(0); // chroma_qp_index_offset
    w.u(1, 1); // deblocking_filter_control_present_flag
    w.u(1, 0); // constrained_intra_pred_flag
    w.u(1, 0); // redundant_pic_cnt_present_flag
    // High-profile tail — present so transform_8x8_mode_flag can state what
    // the hardware does at 4:4:4 (see the PPS flags in `try_new_h264`).
    w.u(1, is_444 as u32); // transform_8x8_mode_flag
    w.u(1, 0); // pic_scaling_matrix_present_flag
    w.se(0); // second_chroma_qp_index_offset
    out.extend_from_slice(&w.into_nal(3, 8));
    out
}

/// Serialize the `sequence_header_obu()` matching the
/// `StdVideoAV1SequenceHeader` that `try_new_av1` hands the driver.
///
/// Vulkan has no AV1 counterpart to
/// `VkVideoEncodeH264SessionParametersGetInfoKHR` — the spec expects the
/// application to serialize the sequence header itself from the same
/// values it passed to session-parameter creation.  Every field below
/// mirrors that struct; if `try_new_av1` changes what it tells the driver,
/// this must change with it or every frame belongs to a stream no decoder
/// accepts.
///
/// `seq_level_idx` is the `StdVideoAV1Level` value, which is numerically
/// the bitstream's `seq_level_idx` (2.0 = 0 … 6.0 = 16).
fn av1_sequence_header_obu(
    seq_level_idx: u32,
    frame_width_bits: u32,
    frame_height_bits: u32,
    coded_w: u32,
    coded_h: u32,
) -> Vec<u8> {
    // Big-endian bit packer, AV1 f(n) semantics.
    struct BitWriter {
        bytes: Vec<u8>,
        used: u8,
    }
    impl BitWriter {
        fn put(&mut self, n: u32, v: u32) {
            for i in (0..n).rev() {
                if self.used == 0 {
                    self.bytes.push(0);
                }
                let bit = ((v >> i) & 1) as u8;
                *self.bytes.last_mut().unwrap() |= bit << (7 - self.used);
                self.used = (self.used + 1) & 7;
            }
        }
    }
    let mut w = BitWriter {
        bytes: Vec::new(),
        used: 0,
    };
    w.put(3, 0); // seq_profile: Main (4:2:0 8-bit)
    w.put(1, 0); // still_picture
    w.put(1, 0); // reduced_still_picture_header
    w.put(1, 0); // timing_info_present_flag (p_timing_info is null)
    w.put(1, 0); // initial_display_delay_present_flag
    w.put(5, 0); // operating_points_cnt_minus_1
    w.put(12, 0); // operating_point_idc[0]: all temporal/spatial layers
    w.put(5, seq_level_idx);
    if seq_level_idx > 7 {
        w.put(1, 0); // seq_tier[0]: Main tier
    }
    w.put(4, frame_width_bits - 1);
    w.put(4, frame_height_bits - 1);
    w.put(frame_width_bits, coded_w - 1);
    w.put(frame_height_bits, coded_h - 1);
    w.put(1, 0); // frame_id_numbers_present_flag
    w.put(1, 0); // use_128x128_superblock
    w.put(1, 0); // enable_filter_intra
    w.put(1, 0); // enable_intra_edge_filter
    w.put(1, 0); // enable_interintra_compound
    w.put(1, 0); // enable_masked_compound
    w.put(1, 0); // enable_warped_motion
    w.put(1, 0); // enable_dual_filter
    w.put(1, 1); // enable_order_hint
    w.put(1, 0); // enable_jnt_comp
    w.put(1, 0); // enable_ref_frame_mvs
    w.put(1, 1); // seq_choose_screen_content_tools (force = SELECT)
    w.put(1, 1); // seq_choose_integer_mv (force = SELECT)
    w.put(3, 6); // order_hint_bits_minus_1: 7-bit order hint
    w.put(1, 0); // enable_superres
    w.put(1, 1); // enable_cdef — the hardware codes cdef_idx symbols
    w.put(1, 0); // enable_restoration
    // color_config(): 8-bit 4:2:0, no colour description (the std struct's
    // primaries/transfer/matrix are all 2 = unspecified, so nothing is lost
    // by omitting them).
    w.put(1, 0); // high_bitdepth
    w.put(1, 0); // mono_chrome
    w.put(1, 0); // color_description_present_flag
    w.put(1, 1); // color_range: full swing (blit is full-range end to end)
    w.put(2, 0); // chroma_sample_position: unknown
    w.put(1, 0); // separate_uv_delta_q
    w.put(1, 0); // film_grain_params_present
    w.put(1, 1); // trailing_one_bit (zero-padded to a byte by the packer)
    let payload = w.bytes;

    // obu_header: type OBU_SEQUENCE_HEADER, obu_has_size_field, then the
    // payload size as leb128.
    let mut obu = Vec::with_capacity(payload.len() + 2);
    obu.push(0x0A);
    let mut size = payload.len();
    loop {
        let mut byte = (size & 0x7F) as u8;
        size >>= 7;
        if size != 0 {
            byte |= 0x80;
        }
        obu.push(byte);
        if size == 0 {
            break;
        }
    }
    obu.extend_from_slice(&payload);
    obu
}

/// Compute the AV1 level for a given coded resolution.
///
/// Based on AV1 spec Table A.3 — pick the lowest level whose MaxPicSize
/// can accommodate the coded picture.
fn compute_av1_level(width: u32, height: u32) -> u32 {
    let pic_size = (width as u64) * (height as u64);
    // StdVideoAV1Level values: 2_0 = 0, 2_1 = 1, ... 5_1 = 13, 6_0 = 16, 6_3 = 19
    if pic_size <= 147_456 {
        0 // 2.0: 426x240
    } else if pic_size <= 278_784 {
        1 // 2.1
    } else if pic_size <= 665_856 {
        4 // 3.0: 1024x768
    } else if pic_size <= 1_065_024 {
        5 // 3.1: 1280x720+
    } else if pic_size <= 2_359_296 {
        8 // 4.0: 1920x1080
    } else if pic_size <= 4_718_592 {
        9 // 4.1: 2048x1152+
    } else if pic_size <= 8_912_896 {
        STD_VIDEO_AV1_LEVEL_5_1 // 5.1: 3840x2160
    } else {
        STD_VIDEO_AV1_LEVEL_6_0 // 6.0: 7680x4320
    }
}

/// Clean up session resources on error during `try_new_h264`.
unsafe fn cleanup_session(
    device: &ash::Device,
    video_fns: &VideoFns,
    dpb_slots: &[DpbSlot],
    session_memory: &[vk::DeviceMemory],
    session_params: vk::VideoSessionParametersKHR,
    video_session: vk::VideoSessionKHR,
) {
    unsafe {
        for slot in dpb_slots {
            destroy_dpb_slot(device, slot);
        }
        (video_fns.destroy_video_session_parameters)(device.handle(), session_params, ptr::null());
        for &m in session_memory {
            device.free_memory(m, None);
        }
        (video_fns.destroy_video_session)(device.handle(), video_session, ptr::null());
    }
}

/// Query and bind memory for a video session.
///
/// Calls `vkGetVideoSessionMemoryRequirementsKHR`, allocates device-local
/// memory for each requirement, and binds it via `vkBindVideoSessionMemoryKHR`.
/// On failure, cleans up any partially-allocated memory and destroys the
/// video session.
unsafe fn bind_session_memory(
    device: &ash::Device,
    video_fns: &VideoFns,
    session: vk::VideoSessionKHR,
    physical_device: vk::PhysicalDevice,
    instance: &ash::Instance,
) -> Option<Vec<vk::DeviceMemory>> {
    let mut mem_req_count = 0u32;
    let res = unsafe {
        (video_fns.get_video_session_memory_requirements)(
            device.handle(),
            session,
            &mut mem_req_count,
            ptr::null_mut(),
        )
    };
    if res != vk::Result::SUCCESS {
        eprintln!("[vulkan-encode] vkGetVideoSessionMemoryRequirementsKHR(count) failed: {res:?}",);
        unsafe {
            (video_fns.destroy_video_session)(device.handle(), session, ptr::null());
        }
        return None;
    }

    let mut mem_reqs: Vec<vk::VideoSessionMemoryRequirementsKHR<'_>> =
        vec![vk::VideoSessionMemoryRequirementsKHR::default(); mem_req_count as usize];
    let res = unsafe {
        (video_fns.get_video_session_memory_requirements)(
            device.handle(),
            session,
            &mut mem_req_count,
            mem_reqs.as_mut_ptr(),
        )
    };
    if res != vk::Result::SUCCESS {
        eprintln!("[vulkan-encode] vkGetVideoSessionMemoryRequirementsKHR(data) failed: {res:?}",);
        unsafe {
            (video_fns.destroy_video_session)(device.handle(), session, ptr::null());
        }
        return None;
    }

    let mem_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };

    let mut session_memory = Vec::new();
    let mut bind_infos = Vec::new();
    for req in &mem_reqs[..mem_req_count as usize] {
        let mr = &req.memory_requirements;
        let mem_type_idx = find_memory_type(
            &mem_props,
            mr.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .or_else(|| {
            find_memory_type(
                &mem_props,
                mr.memory_type_bits,
                vk::MemoryPropertyFlags::empty(),
            )
        });
        let Some(mem_type_idx) = mem_type_idx else {
            eprintln!("[vulkan-encode] no suitable memory type for session memory");
            for &m in &session_memory {
                unsafe { device.free_memory(m, None) };
            }
            unsafe {
                (video_fns.destroy_video_session)(device.handle(), session, ptr::null());
            }
            return None;
        };
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(mr.size)
            .memory_type_index(mem_type_idx);
        let memory = match unsafe { device.allocate_memory(&alloc, None) } {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[vulkan-encode] session memory alloc failed: {e:?}");
                for &m in &session_memory {
                    unsafe { device.free_memory(m, None) };
                }
                unsafe {
                    (video_fns.destroy_video_session)(device.handle(), session, ptr::null());
                }
                return None;
            }
        };
        session_memory.push(memory);
        bind_infos.push(
            vk::BindVideoSessionMemoryInfoKHR::default()
                .memory_bind_index(req.memory_bind_index)
                .memory(memory)
                .memory_offset(0)
                .memory_size(mr.size),
        );
    }

    if !bind_infos.is_empty() {
        let res = unsafe {
            (video_fns.bind_video_session_memory)(
                device.handle(),
                session,
                bind_infos.len() as u32,
                bind_infos.as_ptr(),
            )
        };
        if res != vk::Result::SUCCESS {
            eprintln!("[vulkan-encode] vkBindVideoSessionMemoryKHR failed: {res:?}");
            for &m in &session_memory {
                unsafe { device.free_memory(m, None) };
            }
            unsafe {
                (video_fns.destroy_video_session)(device.handle(), session, ptr::null());
            }
            return None;
        }
    }

    Some(session_memory)
}

/// Allocate two DPB (Decoded Picture Buffer) slots for video encode.
///
/// Each slot gets a `G8_B8R8_2PLANE_420_UNORM` image with `VIDEO_ENCODE_DPB`
/// usage plus an image view.  On failure, cleans up partially-created slots
/// and the full session (params + memory + session).
unsafe fn allocate_dpb_slots(
    device: &ash::Device,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    video_fns: &VideoFns,
    width: u32,
    height: u32,
    video_queue_family: u32,
    profile: &vk::VideoProfileInfoKHR<'_>,
    format: vk::Format,
    session_memory: &[vk::DeviceMemory],
    session_params: vk::VideoSessionParametersKHR,
    video_session: vk::VideoSessionKHR,
) -> Option<[DpbSlot; 2]> {
    let mut dpb_slots_vec = Vec::new();
    for i in 0..2 {
        let dpb = unsafe {
            create_dpb_image(
                device,
                instance,
                physical_device,
                width,
                height,
                video_queue_family,
                profile,
                format,
            )
        };
        let Some(dpb) = dpb else {
            eprintln!("[vulkan-encode] DPB image {i} creation failed");
            for slot in &dpb_slots_vec {
                unsafe { destroy_dpb_slot(device, slot) };
            }
            unsafe {
                (video_fns.destroy_video_session_parameters)(
                    device.handle(),
                    session_params,
                    ptr::null(),
                );
            }
            for &m in session_memory {
                unsafe { device.free_memory(m, None) };
            }
            unsafe {
                (video_fns.destroy_video_session)(device.handle(), video_session, ptr::null());
            }
            return None;
        };
        dpb_slots_vec.push(dpb);
    }
    Some([dpb_slots_vec.remove(0), dpb_slots_vec.remove(0)])
}

/// Allocate a host-visible, host-coherent mapped buffer for encoded bitstream
/// output.
///
/// Returns `(buffer, memory, mapped_ptr)`.  On failure, cleans up and returns
/// `None`.
unsafe fn allocate_bitstream_buffer(
    device: &ash::Device,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    video_fns: &VideoFns,
    capacity: u64,
    profile: &vk::VideoProfileInfoKHR,
    dpb_slots: &[DpbSlot; 2],
    session_memory: &[vk::DeviceMemory],
    session_params: vk::VideoSessionParametersKHR,
    video_session: vk::VideoSessionKHR,
) -> Option<(vk::Buffer, vk::DeviceMemory, *mut u8)> {
    // A VIDEO_ENCODE_DST buffer must name the profiles it will be used
    // with (VUID-VkBufferCreateInfo-usage-04814).  NVIDIA tolerates the
    // omission for High 4:2:0 sessions but enforces it for High 4:4:4
    // Predictive: the encode records fine and then vkEndCommandBuffer
    // fails with ERROR_INITIALIZATION_FAILED.
    let profiles = [*profile];
    let mut profile_list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
    let buf_info = vk::BufferCreateInfo::default()
        .size(capacity)
        .usage(vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .push_next(&mut profile_list);
    let bitstream_buffer = match unsafe { device.create_buffer(&buf_info, None) } {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[vulkan-encode] bitstream buffer create failed: {e:?}");
            unsafe {
                cleanup_session(
                    device,
                    video_fns,
                    dpb_slots,
                    session_memory,
                    session_params,
                    video_session,
                );
            }
            return None;
        }
    };
    let buf_reqs = unsafe { device.get_buffer_memory_requirements(bitstream_buffer) };
    let mem_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };
    let buf_mem_type = find_memory_type(
        &mem_props,
        buf_reqs.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    );
    let Some(buf_mem_type) = buf_mem_type else {
        eprintln!("[vulkan-encode] no host-visible memory for bitstream buffer");
        unsafe {
            device.destroy_buffer(bitstream_buffer, None);
            cleanup_session(
                device,
                video_fns,
                dpb_slots,
                session_memory,
                session_params,
                video_session,
            );
        }
        return None;
    };
    let buf_alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(buf_reqs.size)
        .memory_type_index(buf_mem_type);
    let bitstream_memory = match unsafe { device.allocate_memory(&buf_alloc, None) } {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[vulkan-encode] bitstream memory alloc failed: {e:?}");
            unsafe { device.destroy_buffer(bitstream_buffer, None) };
            unsafe {
                cleanup_session(
                    device,
                    video_fns,
                    dpb_slots,
                    session_memory,
                    session_params,
                    video_session,
                );
            }
            return None;
        }
    };
    if unsafe { device.bind_buffer_memory(bitstream_buffer, bitstream_memory, 0) }.is_err() {
        eprintln!("[vulkan-encode] bind bitstream buffer memory failed");
        unsafe {
            device.free_memory(bitstream_memory, None);
            device.destroy_buffer(bitstream_buffer, None);
            cleanup_session(
                device,
                video_fns,
                dpb_slots,
                session_memory,
                session_params,
                video_session,
            );
        }
        return None;
    }
    let bitstream_ptr = match unsafe {
        device.map_memory(
            bitstream_memory,
            0,
            vk::WHOLE_SIZE,
            vk::MemoryMapFlags::empty(),
        )
    } {
        Ok(p) => p as *mut u8,
        Err(e) => {
            eprintln!("[vulkan-encode] map bitstream memory failed: {e:?}");
            unsafe {
                device.free_memory(bitstream_memory, None);
                device.destroy_buffer(bitstream_buffer, None);
            }
            unsafe {
                cleanup_session(
                    device,
                    video_fns,
                    dpb_slots,
                    session_memory,
                    session_params,
                    video_session,
                );
            }
            return None;
        }
    };

    Some((bitstream_buffer, bitstream_memory, bitstream_ptr))
}

/// Create a query pool for video encode feedback.
///
/// `profile_for_query` must already have codec-specific profile info
/// chained via pNext before being passed here.
unsafe fn create_encode_query_pool(
    device: &ash::Device,
    video_fns: &VideoFns,
    profile_for_query: &mut vk::VideoProfileInfoKHR<'_>,
    bitstream_buffer: vk::Buffer,
    bitstream_memory: vk::DeviceMemory,
    dpb_slots: &[DpbSlot; 2],
    session_memory: &[vk::DeviceMemory],
    session_params: vk::VideoSessionParametersKHR,
    video_session: vk::VideoSessionKHR,
) -> Option<vk::QueryPool> {
    let mut encode_feedback_info = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default()
        .encode_feedback_flags(vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN);
    let qp_info = vk::QueryPoolCreateInfo::default()
        .query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR)
        .query_count(1)
        .push_next(&mut encode_feedback_info)
        .push_next(profile_for_query);
    let query_pool = match unsafe { device.create_query_pool(&qp_info, None) } {
        Ok(q) => q,
        Err(e) => {
            eprintln!("[vulkan-encode] query pool create failed: {e:?}");
            unsafe {
                device.unmap_memory(bitstream_memory);
                device.free_memory(bitstream_memory, None);
                device.destroy_buffer(bitstream_buffer, None);
                cleanup_session(
                    device,
                    video_fns,
                    dpb_slots,
                    session_memory,
                    session_params,
                    video_session,
                );
            }
            return None;
        }
    };
    Some(query_pool)
}

/// Create a DPB (Decoded Picture Buffer) image + view.
unsafe fn create_dpb_image(
    device: &ash::Device,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    width: u32,
    height: u32,
    queue_family: u32,
    profile: &vk::VideoProfileInfoKHR<'_>,
    // Must match the session's `reference_picture_format`.
    format: vk::Format,
) -> Option<DpbSlot> {
    let mut profile_list =
        vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(profile));

    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .queue_family_indices(std::slice::from_ref(&queue_family))
        .push_next(&mut profile_list);

    let image = unsafe { device.create_image(&image_info, None).ok()? };
    let mem_reqs = unsafe { device.get_image_memory_requirements(image) };
    let mem_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };
    let mem_type_idx = find_memory_type(
        &mem_props,
        mem_reqs.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .or_else(|| {
        find_memory_type(
            &mem_props,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::empty(),
        )
    })?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(mem_type_idx);
    let memory = match unsafe { device.allocate_memory(&alloc, None) } {
        Ok(m) => m,
        Err(_) => {
            unsafe { device.destroy_image(image, None) };
            return None;
        }
    };
    if unsafe { device.bind_image_memory(image, memory, 0) }.is_err() {
        unsafe {
            device.free_memory(memory, None);
            device.destroy_image(image, None);
        }
        return None;
    }

    let view_info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        });
    let view = match unsafe { device.create_image_view(&view_info, None) } {
        Ok(v) => v,
        Err(_) => {
            unsafe {
                device.free_memory(memory, None);
                device.destroy_image(image, None);
            }
            return None;
        }
    };

    Some(DpbSlot {
        image,
        memory,
        view,
    })
}

/// Destroy a DPB slot (view, image, memory).
unsafe fn destroy_dpb_slot(device: &ash::Device, slot: &DpbSlot) {
    unsafe {
        device.destroy_image_view(slot.view, None);
        device.destroy_image(slot.image, None);
        device.free_memory(slot.memory, None);
    }
}

// ===================================================================
// VK_KHR_video_encode_av1 — Raw FFI definitions
//
// Ash 0.38 (Vulkan 1.3.281) predates VK_KHR_video_encode_av1.
// We define the minimal set of types and constants needed for
// all-intra (single tile, profile 0) AV1 encoding.
// ===================================================================

/// `VK_VIDEO_CODEC_OPERATION_ENCODE_AV1_BIT_KHR` (0x00040000).
const VK_VIDEO_CODEC_OPERATION_ENCODE_AV1_BIT_KHR: u32 = 0x0004_0000;

// The AV1 encode structure types are NOT numbered in declaration order —
// CAPABILITIES is the first value and SESSION_CREATE_INFO the tenth — so
// transcribing them by position gets three of the six wrong, which is what
// happened here: PROFILE_INFO was 004 (the real value is 005),
// SESSION_CREATE_INFO was 000 (that is CAPABILITIES) and CAPABILITIES was 008
// (that is QUALITY_LEVEL_PROPERTIES).  A wrong sType on the profile struct is
// not a loud failure: the driver simply does not recognise the chained struct,
// sees a codec operation with no AV1 profile behind it, and answers every
// capability query with ERROR_VIDEO_PROFILE_FORMAT_NOT_SUPPORTED_KHR — which
// reads exactly like "this GPU cannot encode AV1".
//
// Values checked against vulkan_core.h 1.4.350.0.
/// `VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_CAPABILITIES_KHR`.
const VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_CAPABILITIES_KHR: i32 = 1_000_513_000;
/// `VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_SESSION_PARAMETERS_CREATE_INFO_KHR`.
const VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_SESSION_PARAMETERS_CREATE_INFO_KHR: i32 = 1_000_513_001;
/// `VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_PICTURE_INFO_KHR`.
const VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_PICTURE_INFO_KHR: i32 = 1_000_513_002;
/// `VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_DPB_SLOT_INFO_KHR`.
const VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_DPB_SLOT_INFO_KHR: i32 = 1_000_513_003;
/// `VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_PROFILE_INFO_KHR`.
const VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_PROFILE_INFO_KHR: i32 = 1_000_513_005;
/// `VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_SESSION_CREATE_INFO_KHR`.
const VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_SESSION_CREATE_INFO_KHR: i32 = 1_000_513_009;

// --- StdVideo AV1 types (encode-specific, not in ash 0.38) ---

/// StdVideoAV1Profile — matches vulkan_video_codec_av1std.h.
const STD_VIDEO_AV1_PROFILE_MAIN: u32 = 0;

/// StdVideoAV1Level — subset of levels we care about.
const STD_VIDEO_AV1_LEVEL_5_1: u32 = 13;
const STD_VIDEO_AV1_LEVEL_6_0: u32 = 16;

/// Minimal `StdVideoAV1SequenceHeader` for all-intra encode.
///
/// The full struct has many fields; we zero-init and fill the
/// essential ones.  The driver validates and ignores unknown-zero
/// fields gracefully for encode-only sessions.
#[repr(C)]
#[derive(Copy, Clone)]
struct StdVideoAV1SequenceHeaderFlags {
    bits: u32,
}

impl StdVideoAV1SequenceHeaderFlags {
    fn new() -> Self {
        Self { bits: 0 }
    }

    fn set_enable_order_hint(&mut self, v: bool) {
        // Bit 9 — count the bitfield in vulkan_video_codec_av1std.h
        // (still_picture is bit 0).  Bit 7 is enable_warped_motion.
        if v {
            self.bits |= 1 << 9;
        }
    }

    fn set_enable_cdef(&mut self, v: bool) {
        // Bit 14, same counting rule as above.
        if v {
            self.bits |= 1 << 14;
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
struct StdVideoAV1ColorConfig {
    flags: u32,
    bit_depth: u8,
    subsampling_x: u8,
    subsampling_y: u8,
    _reserved1: u8,
    // The four colour fields are C enums — 4 bytes each, not u8.
    color_primaries: u32,
    transfer_characteristics: u32,
    matrix_coefficients: u32,
    chroma_sample_position: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct StdVideoAV1TimingInfo {
    flags: u32,
    num_units_in_display_tick: u32,
    time_scale: u32,
    num_ticks_per_picture_minus_1: u32,
}

/// Minimal `StdVideoAV1SequenceHeader`.
/// Zero-init is safe; we fill seq_profile, max_frame_width/height,
/// color_config, and the order_hint fields.
#[repr(C)]
#[derive(Copy, Clone)]
struct StdVideoAV1SequenceHeader {
    flags: StdVideoAV1SequenceHeaderFlags,
    seq_profile: u32, // StdVideoAV1Profile
    frame_width_bits_minus_1: u8,
    frame_height_bits_minus_1: u8,
    max_frame_width_minus_1: u16,
    max_frame_height_minus_1: u16,
    delta_frame_id_length_minus_2: u8,
    additional_frame_id_length_minus_1: u8,
    order_hint_bits_minus_1: u8,
    seq_force_integer_mv: u8,
    seq_force_screen_content_tools: u8,
    _reserved1: [u8; 5],
    p_color_config: *const StdVideoAV1ColorConfig,
    p_timing_info: *const StdVideoAV1TimingInfo,
}

/// `StdVideoAV1FrameType` — key (0), inter (1), intra-only (2), switch (3).
const STD_VIDEO_AV1_FRAME_TYPE_KEY: u32 = 0;
const STD_VIDEO_AV1_FRAME_TYPE_INTER: u32 = 1;

/// StdVideoEncodeAV1PictureInfoFlags — bitfield.
#[repr(C)]
#[derive(Copy, Clone)]
struct StdVideoEncodeAV1PictureInfoFlags {
    bits: u32,
}

impl StdVideoEncodeAV1PictureInfoFlags {
    fn new() -> Self {
        Self { bits: 0 }
    }

    // Bit positions come from counting the bitfield in
    // vulkan_video_codec_av1std_encode.h — do not guess: a wrong bit here
    // lands in a *different* flag the driver happily encodes (bit 3 is
    // render_and_frame_size_different, not force_integer_mv).

    fn set_error_resilient_mode(&mut self, v: bool) {
        if v {
            self.bits |= 1 << 0;
        }
    }

    fn set_force_integer_mv(&mut self, v: bool) {
        if v {
            self.bits |= 1 << 6;
        }
    }

    fn set_render_and_frame_size_different(&mut self, v: bool) {
        if v {
            self.bits |= 1 << 3;
        }
    }

    fn set_show_frame(&mut self, v: bool) {
        // Without this the driver writes `show_frame = 0` into every frame
        // header: decoders decode the stream and present nothing.
        if v {
            self.bits |= 1 << 27;
        }
    }
}

/// Minimal `StdVideoEncodeAV1PictureInfo`.
#[repr(C)]
#[derive(Copy, Clone)]
struct StdVideoEncodeAV1PictureInfo {
    flags: StdVideoEncodeAV1PictureInfoFlags,
    frame_type: u32, // StdVideoAV1FrameType
    frame_presentation_time: u32,
    current_frame_id: u32,
    order_hint: u8,
    primary_ref_frame: u8,
    refresh_frame_flags: u8,
    coded_denom: u8,
    render_width_minus_1: u16,
    render_height_minus_1: u16,
    interpolation_filter: u32,
    tx_mode: u32,
    delta_q_res: u8,
    delta_lf_res: u8,
    // No padding before these arrays — vulkan_video_codec_av1std_encode.h
    // packs ref_order_hint directly after delta_lf_res, with reserved1[3]
    // after ref_frame_idx bringing delta_frame_id_minus_1 to alignment.
    ref_order_hint: [u8; 8], // STD_VIDEO_AV1_NUM_REF_FRAMES
    ref_frame_idx: [i8; 7],  // STD_VIDEO_AV1_REFS_PER_FRAME
    _reserved1: [u8; 3],
    delta_frame_id_minus_1: [u32; 7],
    p_tile_info: *const StdVideoAV1TileInfo,
    p_quantization: *const StdVideoAV1Quantization,
    p_segmentation: *const std::ffi::c_void,
    p_loop_filter: *const StdVideoAV1LoopFilter,
    p_cdef: *const StdVideoAV1CDEF,
    p_loop_restoration: *const StdVideoAV1LoopRestoration,
    p_global_motion: *const std::ffi::c_void,
    p_extension_header: *const std::ffi::c_void,
    p_buffer_removal_times: *const u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct StdVideoAV1TileInfo {
    flags: u32,
    tile_cols: u8,
    tile_rows: u8,
    context_update_tile_id: u16,
    tile_size_bytes_minus_1: u8,
    _reserved: [u8; 7],
    p_mi_col_starts: *const u16,
    p_mi_row_starts: *const u16,
    p_width_in_sbs_minus_1: *const u16,
    p_height_in_sbs_minus_1: *const u16,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct StdVideoAV1Quantization {
    flags: u32,
    base_q_idx: u8,
    delta_q_y_dc: i8,
    delta_q_u_dc: i8,
    delta_q_u_ac: i8,
    delta_q_v_dc: i8,
    delta_q_v_ac: i8,
    qm_y: u8,
    qm_u: u8,
    qm_v: u8,
    _reserved: [u8; 3],
}

#[repr(C)]
#[derive(Copy, Clone)]
struct StdVideoAV1LoopFilter {
    flags: u32,
    loop_filter_level: [u8; 4],
    loop_filter_sharpness: u8,
    update_ref_delta: u8,
    loop_filter_ref_deltas: [i8; 8],
    update_mode_delta: u8,
    loop_filter_mode_deltas: [i8; 2],
}

#[repr(C)]
#[derive(Copy, Clone)]
struct StdVideoAV1CDEF {
    cdef_damping_minus_3: u8,
    cdef_bits: u8,
    cdef_y_pri_strength: [u8; 8],
    cdef_y_sec_strength: [u8; 8],
    cdef_uv_pri_strength: [u8; 8],
    cdef_uv_sec_strength: [u8; 8],
}

#[repr(C)]
#[derive(Copy, Clone)]
struct StdVideoAV1LoopRestoration {
    // StdVideoAV1FrameRestorationType is a C enum — 4 bytes per entry.
    frame_restoration_type: [u32; 3], // STD_VIDEO_AV1_MAX_NUM_PLANES
    loop_restoration_size: [u16; 3],
}

/// StdVideoEncodeAV1ReferenceInfo — per-DPB-slot reference metadata.
#[repr(C)]
#[derive(Copy, Clone)]
struct StdVideoEncodeAV1ReferenceInfoFlags {
    bits: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct StdVideoEncodeAV1ReferenceInfo {
    flags: StdVideoEncodeAV1ReferenceInfoFlags,
    ref_frame_id: u32,
    frame_type: u32, // StdVideoAV1FrameType
    order_hint: u8,
    _reserved: [u8; 3],
    p_extension_header: *const std::ffi::c_void,
}

// Layout guards for the hand-rolled std-header mirrors above.  Sizes are
// from vulkan_video_codec_av1std*.h compiled on x86_64 — a mismatch here
// means fields have drifted, which the driver reports as
// ERROR_INVALID_VIDEO_STD_PARAMETERS_KHR at best and reads as garbage
// pointers at worst.
const _: () = {
    assert!(std::mem::size_of::<StdVideoAV1ColorConfig>() == 24);
    assert!(std::mem::size_of::<StdVideoAV1TimingInfo>() == 16);
    assert!(std::mem::size_of::<StdVideoAV1SequenceHeader>() == 40);
    assert!(std::mem::size_of::<StdVideoAV1TileInfo>() == 48);
    assert!(std::mem::size_of::<StdVideoAV1Quantization>() == 16);
    assert!(std::mem::size_of::<StdVideoAV1LoopFilter>() == 24);
    assert!(std::mem::size_of::<StdVideoAV1LoopRestoration>() == 20);
    assert!(std::mem::size_of::<StdVideoEncodeAV1PictureInfo>() == 152);
    assert!(std::mem::size_of::<StdVideoEncodeAV1ReferenceInfo>() == 24);
};

// --- Vulkan structs ---

/// `VkVideoEncodeAV1SessionCreateInfoKHR`.
#[repr(C)]
struct VideoEncodeAV1SessionCreateInfoKHR {
    s_type: vk::StructureType,
    p_next: *const std::ffi::c_void,
    use_max_level: vk::Bool32,
    max_level: u32, // StdVideoAV1Level
}

/// `VkVideoEncodeAV1SessionParametersCreateInfoKHR`.
#[repr(C)]
struct VideoEncodeAV1SessionParametersCreateInfoKHR {
    s_type: vk::StructureType,
    p_next: *const std::ffi::c_void,
    p_std_sequence_header: *const StdVideoAV1SequenceHeader,
    p_std_decoder_model_info: *const std::ffi::c_void,
    std_operating_point_count: u32,
    p_std_operating_points: *const std::ffi::c_void,
}

/// `VkVideoEncodeAV1ProfileInfoKHR`.
///
/// `pub(crate)` because the encode-source image in `vulkan_render.rs` has to
/// be created against the very same profile the session uses, and ash 0.38
/// has no definition of its own to share.
#[repr(C)]
pub(crate) struct VideoEncodeAV1ProfileInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const std::ffi::c_void,
    pub std_profile: u32, // StdVideoAV1Profile
}

/// Build the AV1 encode profile, with its leaf struct chained in.
///
/// The caller owns `leaf` so it outlives the returned borrow; `pNext` is
/// walked by hand because ash 0.38 predates `VK_KHR_video_encode_av1` and so
/// has no `push_next` impl that accepts our stand-in struct.
pub(crate) fn av1_encode_profile(
    leaf: &mut VideoEncodeAV1ProfileInfoKHR,
) -> vk::VideoProfileInfoKHR<'_> {
    *leaf = VideoEncodeAV1ProfileInfoKHR {
        s_type: vk::StructureType::from_raw(VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_PROFILE_INFO_KHR),
        p_next: ptr::null(),
        std_profile: STD_VIDEO_AV1_PROFILE_MAIN,
    };
    let mut profile = vk::VideoProfileInfoKHR::default()
        .video_codec_operation(vk::VideoCodecOperationFlagsKHR::from_raw(
            VK_VIDEO_CODEC_OPERATION_ENCODE_AV1_BIT_KHR,
        ))
        // AV1 through this path is 4:2:0 only.
        .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8);
    let base = &mut profile as *mut _ as *mut vk::BaseOutStructure<'_>;
    unsafe {
        (*base).p_next = leaf as *mut _ as *mut vk::BaseOutStructure<'_>;
    }
    profile
}

/// `VkVideoEncodeAV1CapabilitiesKHR`.
#[repr(C)]
struct VideoEncodeAV1CapabilitiesKHR {
    s_type: vk::StructureType,
    p_next: *mut std::ffi::c_void,
    flags: u32,
    max_level: u32,
    coded_picture_alignment: vk::Extent2D,
    max_tiles: vk::Extent2D,
    min_tile_size: vk::Extent2D,
    max_tile_size: vk::Extent2D,
    superblock_sizes: u32,
    max_single_reference_count: u32,
    single_reference_name_mask: u32,
    max_unidirectional_compound_reference_count: u32,
    max_unidirectional_compound_group1_reference_count: u32,
    unidirectional_compound_reference_name_mask: u32,
    max_bidirectional_compound_reference_count: u32,
    max_bidirectional_compound_group1_reference_count: u32,
    max_bidirectional_compound_group2_reference_count: u32,
    bidirectional_compound_reference_name_mask: u32,
    max_temporal_layer_count: u32,
    max_spatial_layer_count: u32,
    max_operating_points: u32,
    min_q_index: u32,
    max_q_index: u32,
    prefers_gop_remaining_frames: vk::Bool32,
    requires_gop_remaining_frames: vk::Bool32,
    max_gop_frame_count: u32,
}

/// `VkVideoEncodeAV1PictureInfoKHR`.
#[repr(C)]
struct VideoEncodeAV1PictureInfoKHR {
    s_type: vk::StructureType,
    p_next: *const std::ffi::c_void,
    prediction_mode: u32,
    rate_control_group: u32,
    constant_q_index: u32,
    p_std_picture_info: *const StdVideoEncodeAV1PictureInfo,
    reference_name_slot_indices: [i32; 7],
    primary_reference_cdf_only: vk::Bool32,
    generate_obu_extension_header: vk::Bool32,
}

/// `VkVideoEncodeAV1DpbSlotInfoKHR`.
#[repr(C)]
struct VideoEncodeAV1DpbSlotInfoKHR {
    s_type: vk::StructureType,
    p_next: *const std::ffi::c_void,
    p_std_reference_info: *const StdVideoEncodeAV1ReferenceInfo,
}

/// AV1 prediction modes.
const VK_VIDEO_ENCODE_AV1_PREDICTION_MODE_INTRA_ONLY_KHR: u32 = 0;
const VK_VIDEO_ENCODE_AV1_PREDICTION_MODE_SINGLE_REFERENCE_KHR: u32 = 1;

/// AV1 rate control groups.
const VK_VIDEO_ENCODE_AV1_RATE_CONTROL_GROUP_INTRA_KHR: u32 = 0;
const VK_VIDEO_ENCODE_AV1_RATE_CONTROL_GROUP_PREDICTIVE_KHR: u32 = 1;

/// `STD_VIDEO_AV1_FRAME_RESTORATION_TYPE_NONE`.
const STD_VIDEO_AV1_FRAME_RESTORATION_TYPE_NONE: u32 = 0;
