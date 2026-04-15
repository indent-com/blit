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
            cmd_begin_video_coding: load_device!("vkCmdBeginVideoCodingKHR"),
            cmd_end_video_coding: load_device!("vkCmdEndVideoCodingKHR"),
            cmd_control_video_coding: load_device!("vkCmdControlVideoCodingKHR"),
            cmd_encode_video: load_device!("vkCmdEncodeVideoKHR"),
        })
    }
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
}

unsafe impl Send for VulkanVideoEncoder {}

/// Bitstream buffer size (2 MiB -- generous for a single frame).
const BITSTREAM_CAPACITY: u64 = 2 * 1024 * 1024;

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
    ) -> Option<Self> {
        // ---------------------------------------------------------------
        // 1. Video profile
        // ---------------------------------------------------------------
        let mut h264_profile = vk::VideoEncodeH264ProfileInfoKHR::default()
            .std_profile_idc(StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH);

        let profile = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
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
            eprintln!("[vulkan-encode] vkGetPhysicalDeviceVideoCapabilitiesKHR failed: {res:?}",);
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
            "[vulkan-encode] H.264 caps: max_coded={max_coded_w}x{max_coded_h}, max_dpb={max_dpb}, max_level={max_level_idc}, level={level_idc}",
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
            .picture_format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .max_coded_extent(coded_extent)
            .reference_picture_format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
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

        let crop_right = if width_in_mbs * 16 > width {
            (width_in_mbs * 16 - width) / 2
        } else {
            0
        };
        let crop_bottom = if height_in_mbs * 16 > height {
            (height_in_mbs * 16 - height) / 2
        } else {
            0
        };

        let mut sps: StdVideoH264SequenceParameterSet = unsafe { std::mem::zeroed() };
        sps.flags = sps_flags;
        sps.profile_idc = StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH;
        sps.level_idc = level_idc;
        sps.chroma_format_idc = StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_420;
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

        let mut pps_flags: StdVideoH264PpsFlags = unsafe { std::mem::zeroed() };
        pps_flags.set_entropy_coding_mode_flag(1); // CABAC
        pps_flags.set_deblocking_filter_control_present_flag(1);

        let mut pps: StdVideoH264PictureParameterSet = unsafe { std::mem::zeroed() };
        pps.flags = pps_flags;
        pps.seq_parameter_set_id = 0;
        pps.pic_parameter_set_id = 0;
        pps.num_ref_idx_l0_default_active_minus1 = 0;
        pps.weighted_bipred_idc =
            StdVideoH264WeightedBipredIdc_STD_VIDEO_H264_WEIGHTED_BIPRED_IDC_DEFAULT;
        pps.pic_init_qp_minus26 = qp as i8 - 26;

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
                &dpb_slots,
                &session_memory,
                session_params,
                video_session,
            )
        }?;

        // ---------------------------------------------------------------
        // 8. Query pool (encode feedback)
        // ---------------------------------------------------------------
        let mut h264_profile_for_qp = vk::VideoEncodeH264ProfileInfoKHR::default()
            .std_profile_idc(StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH);
        let mut video_profile_for_query = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
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
        })
    }

    /// Request that the next encode produces an IDR frame.
    #[allow(dead_code)]
    pub(crate) fn request_idr(&mut self) {
        self.force_idr = true;
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

        // Inline query for encode feedback.
        let mut inline_query = vk::VideoInlineQueryInfoKHR::default()
            .query_pool(self.query_pool)
            .first_query(0)
            .query_count(1);

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
                .push_next(&mut h264_pic_info)
                .push_next(&mut inline_query);

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
                .push_next(&mut h264_pic_info)
                .push_next(&mut inline_query);

            unsafe { (video_fns.cmd_encode_video)(cb, &encode_info) };
        }

        // End video coding.
        let end_coding = vk::VideoEndCodingInfoKHR::default();
        unsafe { (video_fns.cmd_end_video_coding)(cb, &end_coding) };

        // End command buffer.
        if unsafe { device.end_command_buffer(cb) }.is_err() {
            unsafe { device.free_command_buffers(encode_cmd_pool, &[cb]) };
            return None;
        }

        // Submit.
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cb));
        let fence_info = vk::FenceCreateInfo::default();
        let fence = unsafe { device.create_fence(&fence_info, None).ok()? };
        if unsafe { device.queue_submit(encode_queue, &[submit], fence) }.is_err() {
            unsafe {
                device.destroy_fence(fence, None);
                device.free_command_buffers(encode_cmd_pool, &[cb]);
            }
            return None;
        }

        // Wait for completion.
        let _ = unsafe { device.wait_for_fences(&[fence], true, u64::MAX) };
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

        // Copy bitstream from mapped pointer.
        let bitstream =
            unsafe { std::slice::from_raw_parts(self.bitstream_ptr, encoded_size).to_vec() };

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
        // 64-pixel superblock alignment for AV1.
        let coded_w = width.div_ceil(64) * 64;
        let coded_h = height.div_ceil(64) * 64;

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
            flags: 0,
            bit_depth: 8,
            subsampling_x: 1,
            subsampling_y: 1,
            color_primaries: 2,          // BT.709
            transfer_characteristics: 2, // BT.709
            matrix_coefficients: 2,      // BT.709
            chroma_sample_position: 0,   // Unknown
            _reserved: 0,
        };

        let mut seq_flags = StdVideoAV1SequenceHeaderFlags::new();
        seq_flags.set_enable_order_hint(true);

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

        let order_hint = (self.frame_num & 0x7F) as u8; // 7-bit order hint

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
        // Single tile covering the full frame.
        let mi_cols = self.width.div_ceil(4) as u16; // 4-pixel MI units
        let mi_rows = self.height.div_ceil(4) as u16;
        let sb_cols = self.width.div_ceil(64) as u16; // 64-pixel superblocks
        let sb_rows = self.height.div_ceil(64) as u16;
        let mi_col_starts = [0u16, mi_cols];
        let mi_row_starts = [0u16, mi_rows];
        let width_in_sbs_minus_1 = [sb_cols.saturating_sub(1)];
        let height_in_sbs_minus_1 = [sb_rows.saturating_sub(1)];

        let tile_info = StdVideoAV1TileInfo {
            flags: 0, // uniform_tile_spacing_flag = 0
            tile_cols: 1,
            tile_rows: 1,
            context_update_tile_id: 0,
            tile_size_bytes_minus_1: 3, // 4 bytes per tile size
            _reserved: [0; 7],
            p_mi_col_starts: mi_col_starts.as_ptr(),
            p_mi_row_starts: mi_row_starts.as_ptr(),
            p_width_in_sbs_minus_1: width_in_sbs_minus_1.as_ptr(),
            p_height_in_sbs_minus_1: height_in_sbs_minus_1.as_ptr(),
        };

        let quantization = StdVideoAV1Quantization {
            flags: 0,
            base_q_idx: self.qp,
            delta_q_y_dc: 0,
            delta_q_u_dc: 0,
            delta_q_u_ac: 0,
            delta_q_v_dc: 0,
            delta_q_v_ac: 0,
            qm_y: 0,
            qm_u: 0,
            qm_v: 0,
            _reserved: [0; 3],
        };

        let loop_filter: StdVideoAV1LoopFilter = unsafe { std::mem::zeroed() };
        let cdef: StdVideoAV1CDEF = unsafe { std::mem::zeroed() };
        let loop_restoration = StdVideoAV1LoopRestoration {
            frame_restoration_type: [
                STD_VIDEO_AV1_FRAME_RESTORATION_TYPE_NONE,
                STD_VIDEO_AV1_FRAME_RESTORATION_TYPE_NONE,
                STD_VIDEO_AV1_FRAME_RESTORATION_TYPE_NONE,
            ],
            loop_restoration_size: [0; 3],
        };

        let mut pic_flags = StdVideoEncodeAV1PictureInfoFlags::new();
        pic_flags.set_error_resilient_mode(is_key);
        pic_flags.set_force_integer_mv(is_key);

        let mut ref_frame_idx = [-1i8; 7];
        if !is_key {
            // LAST_FRAME (index 0) points to the ref DPB slot.
            ref_frame_idx[0] = ref_dpb_idx as i8;
        }

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
            render_width_minus_1: (self.width - 1) as u16,
            render_height_minus_1: (self.height - 1) as u16,
            interpolation_filter: 4, // SWITCHABLE
            tx_mode: 2,              // TX_MODE_SELECT
            delta_q_res: 0,
            delta_lf_res: 0,
            _reserved1: [0; 2],
            ref_order_hint: [0; 8],
            ref_frame_idx,
            _reserved2: 0,
            delta_frame_id_minus_1: [0; 7],
            p_tile_info: &tile_info,
            p_quantization: &quantization,
            p_segmentation: ptr::null(),
            p_loop_filter: &loop_filter,
            p_cdef: &cdef,
            p_loop_restoration: &loop_restoration,
            p_global_motion: ptr::null(),
            min_base_qindex: self.qp as u32,
            max_base_qindex: self.qp as u32,
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

        // Inline query for encode feedback.
        let mut inline_query = vk::VideoInlineQueryInfoKHR::default()
            .query_pool(self.query_pool)
            .first_query(0)
            .query_count(1);

        // Build encode info.
        if is_key {
            let mut encode_info = vk::VideoEncodeInfoKHR::default()
                .dst_buffer(self.bitstream_buffer)
                .dst_buffer_offset(0)
                .dst_buffer_range(self.bitstream_capacity)
                .src_picture_resource(src_picture_resource)
                .setup_reference_slot(&setup_slot)
                .push_next(&mut inline_query);

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
                .reference_slots(std::slice::from_ref(&ref_slot2))
                .push_next(&mut inline_query);

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

        // End video coding.
        let end_coding = vk::VideoEndCodingInfoKHR::default();
        unsafe { (video_fns.cmd_end_video_coding)(cb, &end_coding) };

        // End command buffer.
        if unsafe { device.end_command_buffer(cb) }.is_err() {
            unsafe { device.free_command_buffers(encode_cmd_pool, &[cb]) };
            return None;
        }

        // Submit.
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cb));
        let fence_info = vk::FenceCreateInfo::default();
        let fence = unsafe { device.create_fence(&fence_info, None).ok()? };
        if unsafe { device.queue_submit(encode_queue, &[submit], fence) }.is_err() {
            unsafe {
                device.destroy_fence(fence, None);
                device.free_command_buffers(encode_cmd_pool, &[cb]);
            }
            return None;
        }

        // Wait for completion.
        let _ = unsafe { device.wait_for_fences(&[fence], true, u64::MAX) };
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

        let bitstream =
            unsafe { std::slice::from_raw_parts(self.bitstream_ptr, encoded_size).to_vec() };

        // Update state.
        if is_key {
            self.frame_num = 0;
            self.idr_num = self.idr_num.wrapping_add(1);
        }
        self.frame_num = self.frame_num.wrapping_add(1);
        self.cur_dpb_idx = 1 - self.cur_dpb_idx;

        Some((bitstream, is_key))
    }

    /// Destroy all resources.  Must be called before the device is destroyed.
    #[allow(dead_code)]
    pub(crate) unsafe fn destroy(&mut self, device: &ash::Device, video_fns: &VideoFns) {
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
    dpb_slots: &[DpbSlot; 2],
    session_memory: &[vk::DeviceMemory],
    session_params: vk::VideoSessionParametersKHR,
    video_session: vk::VideoSessionKHR,
) -> Option<(vk::Buffer, vk::DeviceMemory, *mut u8)> {
    let buf_info = vk::BufferCreateInfo::default()
        .size(capacity)
        .usage(vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
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
) -> Option<DpbSlot> {
    let mut profile_list =
        vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(profile));

    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
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
        .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
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

/// `VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_SESSION_CREATE_INFO_KHR` = 1000513000.
const VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_SESSION_CREATE_INFO_KHR: i32 = 1_000_513_000;
/// `VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_SESSION_PARAMETERS_CREATE_INFO_KHR`.
const VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_SESSION_PARAMETERS_CREATE_INFO_KHR: i32 = 1_000_513_001;
/// `VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_PICTURE_INFO_KHR`.
const VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_PICTURE_INFO_KHR: i32 = 1_000_513_002;
/// `VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_DPB_SLOT_INFO_KHR`.
const VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_DPB_SLOT_INFO_KHR: i32 = 1_000_513_003;
/// `VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_PROFILE_INFO_KHR`.
const VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_PROFILE_INFO_KHR: i32 = 1_000_513_004;
/// `VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_CAPABILITIES_KHR`.
const VK_STRUCTURE_TYPE_VIDEO_ENCODE_AV1_CAPABILITIES_KHR: i32 = 1_000_513_008;

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
        if v {
            self.bits |= 1 << 7;
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
    color_primaries: u8,
    transfer_characteristics: u8,
    matrix_coefficients: u8,
    chroma_sample_position: u8,
    _reserved: u8,
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

    fn set_error_resilient_mode(&mut self, v: bool) {
        if v {
            self.bits |= 1 << 0;
        }
    }

    fn set_disable_cdf_update(&mut self, _v: bool) {
        // bit 1
    }

    fn set_use_ref_frame_mvs(&mut self, _v: bool) {
        // bit 5
    }

    fn set_force_integer_mv(&mut self, v: bool) {
        if v {
            self.bits |= 1 << 3;
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
    _reserved1: [u8; 2],
    ref_order_hint: [u8; 8], // STD_VIDEO_AV1_NUM_REF_FRAMES
    ref_frame_idx: [i8; 7],  // STD_VIDEO_AV1_REFS_PER_FRAME
    _reserved2: u8,
    delta_frame_id_minus_1: [u32; 7],
    p_tile_info: *const StdVideoAV1TileInfo,
    p_quantization: *const StdVideoAV1Quantization,
    p_segmentation: *const std::ffi::c_void,
    p_loop_filter: *const StdVideoAV1LoopFilter,
    p_cdef: *const StdVideoAV1CDEF,
    p_loop_restoration: *const StdVideoAV1LoopRestoration,
    p_global_motion: *const std::ffi::c_void,
    min_base_qindex: u32,
    max_base_qindex: u32,
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
    _reserved: [u8; 4],
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
    frame_restoration_type: [u16; 3], // MAX_MB_PLANE
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
}

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
#[repr(C)]
struct VideoEncodeAV1ProfileInfoKHR {
    s_type: vk::StructureType,
    p_next: *const std::ffi::c_void,
    std_profile: u32, // StdVideoAV1Profile
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
const STD_VIDEO_AV1_FRAME_RESTORATION_TYPE_NONE: u16 = 0;
