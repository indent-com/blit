//! Direct Vulkan Video camera decoder.
//!
//! Codec parsing and DPB/reference-picture state come from `cros-codecs`.
//! This module implements its stateless H.264 and AV1 backend contracts using
//! `VK_KHR_video_decode_*`; FFmpeg is not involved.  Each camera worker owns
//! one decoder and therefore one Vulkan device/queue.  Submissions are
//! synchronous, which keeps resource retirement local to that worker and
//! prevents a failed camera driver path from poisoning the compositor's
//! Vulkan device.

#![cfg(target_os = "linux")]
#![allow(non_snake_case, clippy::missing_safety_doc, clippy::too_many_arguments)]

use anyhow::{Context as _, anyhow, bail};
use ash::vk;
use ash::vk::native::*;
use cros_codecs::codec::av1::parser::{
    BitDepth, FrameHeaderObu, MAX_SEGMENTS, MAX_TILE_COLS, MAX_TILE_ROWS, NUM_REF_FRAMES,
    Profile as Av1Profile, SEG_LVL_MAX, SUPERRES_DENOM_MIN, SequenceHeaderObu,
    StreamInfo as Av1StreamInfo, TileGroupObu,
};
use cros_codecs::codec::h264::dpb::{Dpb, DpbEntry};
use cros_codecs::codec::h264::parser::{Pps, Slice, SliceHeader, Sps};
use cros_codecs::codec::h264::picture::{Field, IsIdr, PictureData, Reference};
use cros_codecs::decoder::stateless::av1::{Av1, StatelessAV1DecoderBackend};
use cros_codecs::decoder::stateless::h264::{H264, StatelessH264DecoderBackend};
use cros_codecs::decoder::stateless::{
    NewPictureError, NewPictureResult, StatelessBackendResult, StatelessDecoder,
    StatelessDecoderBackend, StatelessDecoderBackendPicture, StatelessVideoDecoder,
};
use cros_codecs::decoder::{BlockingMode, DecodedHandle, DecoderEvent, StreamInfo};
use cros_codecs::video_frame::{ReadMapping, VideoFrame, WriteMapping};
use cros_codecs::{DecodedFormat, Fourcc, Resolution};
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fmt;
use std::ptr;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

const MAX_ENCODED_BYTES: usize = 4 * 1024 * 1024;
const MAX_DIMENSION: u32 = 16_384;
const MAX_PIXELS: u64 = 67_108_864;
const DEFAULT_FENCE_TIMEOUT_NS: u64 = 10_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    H264,
    Av1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Chroma {
    Cs420,
    Cs444,
}

impl Chroma {
    fn flags(self) -> vk::VideoChromaSubsamplingFlagsKHR {
        match self {
            Self::Cs420 => vk::VideoChromaSubsamplingFlagsKHR::TYPE_420,
            Self::Cs444 => vk::VideoChromaSubsamplingFlagsKHR::TYPE_444,
        }
    }

    fn format(self) -> vk::Format {
        match self {
            Self::Cs420 => vk::Format::G8_B8R8_2PLANE_420_UNORM,
            Self::Cs444 => vk::Format::G8_B8R8_2PLANE_444_UNORM,
        }
    }

    fn chroma_height(self, height: u32) -> u32 {
        match self {
            Self::Cs420 => height.div_ceil(2),
            Self::Cs444 => height,
        }
    }

    fn chroma_width(self, width: u32) -> u32 {
        match self {
            Self::Cs420 => width.div_ceil(2),
            Self::Cs444 => width,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Cs420 => "4:2:0",
            Self::Cs444 => "4:4:4",
        }
    }
}

#[derive(Debug)]
pub(crate) enum Error {
    Unavailable(String),
    Invalid(String),
    Resource(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(s) => write!(f, "Vulkan Video unavailable: {s}"),
            Self::Invalid(s) => write!(f, "invalid video bitstream: {s}"),
            Self::Resource(s) => write!(f, "Vulkan Video resource failure: {s}"),
        }
    }
}

impl std::error::Error for Error {}

fn vk_unavailable(operation: &str, result: vk::Result) -> Error {
    Error::Unavailable(format!("{operation}: {result:?}"))
}

fn vk_resource(operation: &str, result: vk::Result) -> Error {
    Error::Resource(format!("{operation}: {result:?}"))
}

fn validate_dimensions(chroma: Chroma, width: u32, height: u32) -> Result<(), Error> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(Error::Invalid(format!(
            "unsupported dimensions {width}x{height}"
        )));
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(Error::Resource(format!(
            "dimensions {width}x{height} exceed pixel limit"
        )));
    }
    if chroma == Chroma::Cs420 && (!width.is_multiple_of(2) || !height.is_multiple_of(2)) {
        return Err(Error::Invalid("4:2:0 dimensions must be even".into()));
    }
    Ok(())
}

/// The CPU-visible object cros-codecs associates with one decoded handle.
/// Vulkan images live next to it in `VkDecodedPicture`; this only carries the
/// final RGBA bytes consumed by PipeWire.
#[derive(Debug)]
pub(crate) struct CpuFrame {
    resolution: Resolution,
    rgba: Mutex<Option<Vec<u8>>>,
}

impl CpuFrame {
    fn new(width: u32, height: u32) -> Self {
        Self {
            resolution: (width, height).into(),
            rgba: Mutex::new(None),
        }
    }

    fn set_rgba(&self, rgba: Vec<u8>) -> anyhow::Result<()> {
        *self
            .rgba
            .lock()
            .map_err(|_| anyhow!("RGBA frame lock poisoned"))? = Some(rgba);
        Ok(())
    }

    fn clone_rgba(&self) -> anyhow::Result<Vec<u8>> {
        self.rgba
            .lock()
            .map_err(|_| anyhow!("RGBA frame lock poisoned"))?
            .clone()
            .ok_or_else(|| anyhow!("decoded frame is not ready"))
    }
}

impl VideoFrame for CpuFrame {
    fn fourcc(&self) -> Fourcc {
        Fourcc::from(b"I444")
    }

    fn resolution(&self) -> Resolution {
        self.resolution
    }

    fn decoded_format(&self) -> Result<DecodedFormat, String> {
        Ok(DecodedFormat::I444)
    }

    fn get_plane_size(&self) -> Vec<usize> {
        vec![(self.resolution.width as usize) * (self.resolution.height as usize); 3]
    }

    fn get_plane_pitch(&self) -> Vec<usize> {
        vec![self.resolution.width as usize; 3]
    }

    fn map<'a>(&'a self) -> Result<Box<dyn ReadMapping<'a> + 'a>, String> {
        Err("Vulkan camera frames are consumed as RGBA through the backend handle".into())
    }

    fn map_mut<'a>(&'a mut self) -> Result<Box<dyn WriteMapping<'a> + 'a>, String> {
        Err("Vulkan camera frames are written by the GPU".into())
    }
}

struct VulkanCore {
    _entry: ash::Entry,
    instance: ash::Instance,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    queue_family: u32,
    queue: vk::Queue,
    video_instance: ash::khr::video_queue::Instance,
    video_device: ash::khr::video_queue::Device,
    decode_device: ash::khr::video_decode_queue::Device,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    transfer_alignment: u64,
}

unsafe impl Send for VulkanCore {}
unsafe impl Sync for VulkanCore {}

impl VulkanCore {
    fn new(codec: Codec, chroma: Chroma) -> Result<Arc<Self>, Error> {
        // SAFETY: ash loads the process Vulkan loader and owns it for the
        // lifetime of Entry.
        let entry = unsafe { ash::Entry::load() }
            .map_err(|e| Error::Unavailable(format!("load Vulkan loader: {e}")))?;
        let app_name = CString::new("blit-camera-decode").expect("static string");
        let app = vk::ApplicationInfo::default()
            .application_name(&app_name)
            .api_version(vk::API_VERSION_1_3);
        let create = vk::InstanceCreateInfo::default().application_info(&app);
        // SAFETY: create points to stack data valid for the call.
        let instance = unsafe { entry.create_instance(&create, None) }
            .map_err(|e| vk_unavailable("vkCreateInstance", e))?;
        let video_instance = ash::khr::video_queue::Instance::new(&entry, &instance);

        // SAFETY: instance is live.
        let devices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| vk_unavailable("vkEnumeratePhysicalDevices", e))?;
        let selector = std::env::var("BLIT_MEDIA_CAMERA_VULKAN_DEVICE").ok();
        let mut selected = None;
        for (index, physical_device) in devices.iter().copied().enumerate() {
            // SAFETY: handle belongs to this instance.
            let properties = unsafe { instance.get_physical_device_properties(physical_device) };
            let name = unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }.to_string_lossy();
            if let Some(selector) = selector.as_deref() {
                let index_matches = selector.parse::<usize>().ok() == Some(index);
                let name_matches = name
                    .to_ascii_lowercase()
                    .contains(&selector.to_ascii_lowercase());
                if !index_matches && !name_matches {
                    continue;
                }
            }

            // SAFETY: handle belongs to this instance.
            let extensions =
                unsafe { instance.enumerate_device_extension_properties(physical_device) }
                    .map_err(|e| vk_unavailable("vkEnumerateDeviceExtensionProperties", e))?;
            let has = |wanted: &CStr| {
                extensions.iter().any(|extension| unsafe {
                    CStr::from_ptr(extension.extension_name.as_ptr()) == wanted
                })
            };
            let codec_extension = match codec {
                Codec::H264 => ash::khr::video_decode_h264::NAME,
                Codec::Av1 => ash::khr::video_decode_av1::NAME,
            };
            if !has(ash::khr::video_queue::NAME)
                || !has(ash::khr::video_decode_queue::NAME)
                || !has(codec_extension)
            {
                continue;
            }

            // Video decode support is codec-specific per queue family.  The
            // legacy properties call only reports VIDEO_DECODE, so chain the
            // video properties and require this codec operation explicitly.
            let family_count = unsafe {
                instance.get_physical_device_queue_family_properties2_len(physical_device)
            };
            let mut video_families =
                vec![vk::QueueFamilyVideoPropertiesKHR::default(); family_count];
            let mut families = vec![vk::QueueFamilyProperties2::default(); family_count];
            for (family, video) in families.iter_mut().zip(video_families.iter_mut()) {
                family.p_next = (video as *mut vk::QueueFamilyVideoPropertiesKHR<'_>).cast();
            }
            unsafe {
                instance
                    .get_physical_device_queue_family_properties2(physical_device, &mut families)
            };
            let operation = match codec {
                Codec::H264 => vk::VideoCodecOperationFlagsKHR::DECODE_H264,
                Codec::Av1 => vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
            };
            let queue_family = families
                .iter()
                .zip(&video_families)
                .position(|(family, video)| {
                    family.queue_family_properties.queue_count != 0
                        && family
                            .queue_family_properties
                            .queue_flags
                            .contains(vk::QueueFlags::VIDEO_DECODE_KHR)
                        && family
                            .queue_family_properties
                            .queue_flags
                            .contains(vk::QueueFlags::TRANSFER)
                        && video.video_codec_operations.contains(operation)
                });
            let Some(queue_family) = queue_family else {
                continue;
            };

            if query_caps(
                &video_instance,
                physical_device,
                codec,
                chroma,
                default_profile(chroma),
            )
            .is_err()
            {
                continue;
            }
            selected = Some((physical_device, queue_family as u32));
            break;
        }

        let Some((physical_device, queue_family)) = selected else {
            // SAFETY: no child device exists.
            unsafe { instance.destroy_instance(None) };
            return Err(Error::Unavailable(format!(
                "no selected device supports direct {:?} {} decode with transfer readback",
                codec,
                chroma.label()
            )));
        };

        let priorities = [1.0f32];
        let queue = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities);
        let codec_extension = match codec {
            Codec::H264 => ash::khr::video_decode_h264::NAME,
            Codec::Av1 => ash::khr::video_decode_av1::NAME,
        };
        let extensions = [
            ash::khr::video_queue::NAME.as_ptr(),
            ash::khr::video_decode_queue::NAME.as_ptr(),
            codec_extension.as_ptr(),
        ];
        let queues = [queue];
        let create = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&extensions);
        // SAFETY: queue/extension arrays outlive the call.
        let device = match unsafe { instance.create_device(physical_device, &create, None) } {
            Ok(device) => device,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(vk_unavailable("vkCreateDevice", e));
            }
        };
        // SAFETY: queue 0 was requested above.
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let video_device = ash::khr::video_queue::Device::new(&instance, &device);
        let decode_device = ash::khr::video_decode_queue::Device::new(&instance, &device);
        // SAFETY: physical device is live.
        let memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };
        let properties = unsafe { instance.get_physical_device_properties(physical_device) };

        Ok(Arc::new(Self {
            _entry: entry,
            instance,
            physical_device,
            device,
            queue_family,
            queue,
            video_instance,
            video_device,
            decode_device,
            memory_properties,
            transfer_alignment: properties
                .limits
                .optimal_buffer_copy_offset_alignment
                .max(1),
        }))
    }

    fn memory_type(&self, bits: u32, required: vk::MemoryPropertyFlags) -> Option<u32> {
        self.memory_properties.memory_types[..self.memory_properties.memory_type_count as usize]
            .iter()
            .enumerate()
            .find_map(|(index, memory)| {
                ((bits & (1 << index)) != 0 && memory.property_flags.contains(required))
                    .then_some(index as u32)
            })
    }
}

impl Drop for VulkanCore {
    fn drop(&mut self) {
        // SAFETY: all users retain an Arc<Self>, so no child resources remain
        // when this final Arc is dropped.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[derive(Clone, Copy)]
struct ProfileChoice {
    h264: StdVideoH264ProfileIdc,
    av1: StdVideoAV1Profile,
}

fn default_profile(chroma: Chroma) -> ProfileChoice {
    ProfileChoice {
        h264: if chroma == Chroma::Cs444 {
            StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH_444_PREDICTIVE
        } else {
            StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH
        },
        av1: if chroma == Chroma::Cs444 {
            StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_HIGH
        } else {
            StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN
        },
    }
}

struct QueriedCaps {
    min_coded_extent: vk::Extent2D,
    max_coded_extent: vk::Extent2D,
    max_dpb_slots: u32,
    max_active_reference_pictures: u32,
    min_bitstream_buffer_size_alignment: u64,
    std_header_version: vk::ExtensionProperties,
    coincide: bool,
}

fn with_profile<R>(
    codec: Codec,
    chroma: Chroma,
    profile: ProfileChoice,
    f: impl FnOnce(&vk::VideoProfileInfoKHR<'_>) -> R,
) -> R {
    let mut h264 = vk::VideoDecodeH264ProfileInfoKHR::default()
        .std_profile_idc(profile.h264)
        .picture_layout(vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE);
    let mut av1 = vk::VideoDecodeAV1ProfileInfoKHR::default()
        .std_profile(profile.av1)
        .film_grain_support(false);
    let base = vk::VideoProfileInfoKHR::default()
        .video_codec_operation(match codec {
            Codec::H264 => vk::VideoCodecOperationFlagsKHR::DECODE_H264,
            Codec::Av1 => vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
        })
        .chroma_subsampling(chroma.flags())
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8);
    match codec {
        Codec::H264 => f(&base.push_next(&mut h264)),
        Codec::Av1 => f(&base.push_next(&mut av1)),
    }
}

fn query_caps(
    video: &ash::khr::video_queue::Instance,
    physical_device: vk::PhysicalDevice,
    codec: Codec,
    chroma: Chroma,
    profile: ProfileChoice,
) -> Result<QueriedCaps, Error> {
    with_profile(codec, chroma, profile, |profile_info| {
        let mut h264_caps = vk::VideoDecodeH264CapabilitiesKHR::default();
        let mut av1_caps = vk::VideoDecodeAV1CapabilitiesKHR::default();
        // Set the chain manually.  Ash's safe builder intentionally ties the
        // base structure's lifetime to every pNext node; only scalar fields
        // escape this query, so a raw call-scoped chain is clearer here.
        let codec_caps = match codec {
            Codec::H264 => (&mut h264_caps as *mut vk::VideoDecodeH264CapabilitiesKHR<'_>).cast(),
            Codec::Av1 => (&mut av1_caps as *mut vk::VideoDecodeAV1CapabilitiesKHR<'_>).cast(),
        };
        let mut decode_caps = vk::VideoDecodeCapabilitiesKHR {
            p_next: codec_caps,
            ..Default::default()
        };
        let mut caps = vk::VideoCapabilitiesKHR {
            p_next: (&mut decode_caps as *mut vk::VideoDecodeCapabilitiesKHR<'_>).cast(),
            ..Default::default()
        };
        // SAFETY: all pNext data remains alive for the call.
        let result = unsafe {
            (video.fp().get_physical_device_video_capabilities_khr)(
                physical_device,
                profile_info,
                &mut caps,
            )
        };
        if result != vk::Result::SUCCESS {
            return Err(vk_unavailable(
                "vkGetPhysicalDeviceVideoCapabilitiesKHR",
                result,
            ));
        }
        let format = chroma.format();
        let decode_flags = decode_caps.flags;
        let coincide =
            decode_flags.contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE);
        let distinct =
            decode_flags.contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT);
        if !coincide && !distinct {
            return Err(Error::Unavailable(
                "driver supports neither coincident nor distinct decode output".into(),
            ));
        }
        let output_usage =
            vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR | vk::ImageUsageFlags::TRANSFER_SRC;
        let coincidence_formats = coincide
            && query_format(
                video,
                physical_device,
                profile_info,
                output_usage | vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR,
                format,
            )
            .is_ok();
        let distinct_formats = distinct
            && query_format(video, physical_device, profile_info, output_usage, format).is_ok()
            && query_format(
                video,
                physical_device,
                profile_info,
                vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR,
                format,
            )
            .is_ok();
        if !coincidence_formats && !distinct_formats {
            return Err(Error::Unavailable(format!(
                "driver has no exact {} decode/output/transfer image format combination",
                chroma.label()
            )));
        }
        Ok(QueriedCaps {
            min_coded_extent: caps.min_coded_extent,
            max_coded_extent: caps.max_coded_extent,
            max_dpb_slots: caps.max_dpb_slots,
            max_active_reference_pictures: caps.max_active_reference_pictures,
            min_bitstream_buffer_size_alignment: caps.min_bitstream_buffer_size_alignment,
            std_header_version: caps.std_header_version,
            coincide: coincidence_formats,
        })
    })
}

fn query_format(
    video: &ash::khr::video_queue::Instance,
    physical_device: vk::PhysicalDevice,
    profile: &vk::VideoProfileInfoKHR<'_>,
    usage: vk::ImageUsageFlags,
    wanted: vk::Format,
) -> Result<(), Error> {
    let profiles = [*profile];
    let mut profile_list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
    let info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
        .image_usage(usage)
        .push_next(&mut profile_list);
    let mut count = 0;
    // SAFETY: output pointer is null for the count query.
    let result = unsafe {
        (video.fp().get_physical_device_video_format_properties_khr)(
            physical_device,
            &info,
            &mut count,
            ptr::null_mut(),
        )
    };
    if result != vk::Result::SUCCESS || count == 0 {
        return Err(vk_unavailable(
            "vkGetPhysicalDeviceVideoFormatPropertiesKHR(count)",
            result,
        ));
    }
    let mut properties = vec![vk::VideoFormatPropertiesKHR::default(); count as usize];
    // SAFETY: properties has room for count initialized records.
    let result = unsafe {
        (video.fp().get_physical_device_video_format_properties_khr)(
            physical_device,
            &info,
            &mut count,
            properties.as_mut_ptr(),
        )
    };
    if result != vk::Result::SUCCESS && result != vk::Result::INCOMPLETE {
        return Err(vk_unavailable(
            "vkGetPhysicalDeviceVideoFormatPropertiesKHR",
            result,
        ));
    }
    if properties[..count as usize]
        .iter()
        .any(|p| p.format == wanted)
    {
        Ok(())
    } else {
        Err(Error::Unavailable(format!(
            "driver does not expose format {} for usage {:#x}",
            wanted.as_raw(),
            usage.as_raw(),
        )))
    }
}

fn align_up(value: u64, alignment: u64) -> u64 {
    value.div_ceil(alignment.max(1)) * alignment.max(1)
}

struct Buffer {
    core: Arc<VulkanCore>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    size: u64,
    mapped: *mut u8,
}

unsafe impl Send for Buffer {}

impl Buffer {
    fn new(
        core: Arc<VulkanCore>,
        size: u64,
        usage: vk::BufferUsageFlags,
        mapped: bool,
    ) -> Result<Self, Error> {
        let create = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: create data is live for the call.
        let buffer = unsafe { core.device.create_buffer(&create, None) }
            .map_err(|e| vk_resource("vkCreateBuffer", e))?;
        // SAFETY: buffer belongs to device.
        let requirements = unsafe { core.device.get_buffer_memory_requirements(buffer) };
        let required = if mapped {
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
        } else {
            vk::MemoryPropertyFlags::DEVICE_LOCAL
        };
        let memory_type = core
            .memory_type(requirements.memory_type_bits, required)
            .ok_or_else(|| {
                Error::Unavailable(format!(
                    "no memory type for buffer flags {:#x}",
                    required.as_raw()
                ))
            })?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        let memory = match unsafe { core.device.allocate_memory(&allocate, None) } {
            Ok(memory) => memory,
            Err(e) => {
                unsafe { core.device.destroy_buffer(buffer, None) };
                return Err(vk_resource("vkAllocateMemory(buffer)", e));
            }
        };
        if let Err(e) = unsafe { core.device.bind_buffer_memory(buffer, memory, 0) } {
            unsafe {
                core.device.destroy_buffer(buffer, None);
                core.device.free_memory(memory, None);
            }
            return Err(vk_resource("vkBindBufferMemory", e));
        }
        let mapped_ptr = if mapped {
            match unsafe {
                core.device
                    .map_memory(memory, 0, requirements.size, vk::MemoryMapFlags::empty())
            } {
                Ok(ptr) => ptr.cast(),
                Err(e) => {
                    unsafe {
                        core.device.destroy_buffer(buffer, None);
                        core.device.free_memory(memory, None);
                    }
                    return Err(vk_resource("vkMapMemory", e));
                }
            }
        } else {
            ptr::null_mut()
        };
        Ok(Self {
            core,
            buffer,
            memory,
            size: requirements.size,
            mapped: mapped_ptr,
        })
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: queue submissions are waited synchronously before buffers
        // can be dropped.
        unsafe {
            if !self.mapped.is_null() {
                self.core.device.unmap_memory(self.memory);
            }
            self.core.device.destroy_buffer(self.buffer, None);
            self.core.device.free_memory(self.memory, None);
        }
    }
}

struct Surface {
    core: Arc<VulkanCore>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
    fn new(
        core: Arc<VulkanCore>,
        profile: &vk::VideoProfileInfoKHR<'_>,
        format: vk::Format,
        width: u32,
        height: u32,
        usage: vk::ImageUsageFlags,
    ) -> Result<Arc<Self>, Error> {
        let profiles = [*profile];
        let mut profile_list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
        let create = vk::ImageCreateInfo::default()
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
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut profile_list);
        let image = unsafe { core.device.create_image(&create, None) }
            .map_err(|e| vk_resource("vkCreateImage(decode surface)", e))?;
        let requirements = unsafe { core.device.get_image_memory_requirements(image) };
        let memory_type = core
            .memory_type(
                requirements.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .ok_or_else(|| Error::Unavailable("no device-local image memory type".into()))?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        let memory = match unsafe { core.device.allocate_memory(&allocate, None) } {
            Ok(memory) => memory,
            Err(e) => {
                unsafe { core.device.destroy_image(image, None) };
                return Err(vk_resource("vkAllocateMemory(decode surface)", e));
            }
        };
        if let Err(e) = unsafe { core.device.bind_image_memory(image, memory, 0) } {
            unsafe {
                core.device.destroy_image(image, None);
                core.device.free_memory(memory, None);
            }
            return Err(vk_resource("vkBindImageMemory(decode surface)", e));
        }
        let view_create = vk::ImageViewCreateInfo::default()
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
        let view = match unsafe { core.device.create_image_view(&view_create, None) } {
            Ok(view) => view,
            Err(e) => {
                unsafe {
                    core.device.destroy_image(image, None);
                    core.device.free_memory(memory, None);
                }
                return Err(vk_resource("vkCreateImageView(decode surface)", e));
            }
        };
        Ok(Arc::new(Self {
            core,
            image,
            memory,
            view,
        }))
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            self.core.device.destroy_image_view(self.view, None);
            self.core.device.destroy_image(self.image, None);
            self.core.device.free_memory(self.memory, None);
        }
    }
}

struct SlotPool {
    free: RefCell<Vec<i32>>,
}

struct SlotLease {
    index: i32,
    pool: Weak<SlotPool>,
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            pool.free.borrow_mut().push(self.index);
        }
    }
}

struct VkDecodedPicture {
    frame: Arc<CpuFrame>,
    dpb: Arc<Surface>,
    slot: Rc<SlotLease>,
    timestamp: u64,
    coded: Resolution,
    display: Resolution,
    av1_reference: Option<StdVideoDecodeAV1ReferenceInfo>,
}

#[derive(Clone)]
pub(crate) struct VkHandle(Rc<VkDecodedPicture>);

impl VkHandle {
    fn slot(&self) -> i32 {
        self.0.slot.index
    }

    fn dpb(&self) -> &Arc<Surface> {
        &self.0.dpb
    }

    fn rgba(&self) -> anyhow::Result<Vec<u8>> {
        self.0.frame.clone_rgba()
    }
}

impl DecodedHandle for VkHandle {
    type Frame = CpuFrame;

    fn video_frame(&self) -> Arc<Self::Frame> {
        self.0.frame.clone()
    }

    fn timestamp(&self) -> u64 {
        self.0.timestamp
    }

    fn coded_resolution(&self) -> Resolution {
        self.0.coded
    }

    fn display_resolution(&self) -> Resolution {
        self.0.display
    }

    fn is_ready(&self) -> bool {
        self.0.frame.rgba.lock().is_ok_and(|rgba| rgba.is_some())
    }

    fn sync(&self) -> anyhow::Result<()> {
        if self.is_ready() {
            Ok(())
        } else {
            bail!("Vulkan decode frame is not ready")
        }
    }
}

struct Session {
    core: Arc<VulkanCore>,
    codec: Codec,
    chroma: Chroma,
    profile_choice: ProfileChoice,
    coded_width: u32,
    coded_height: u32,
    display_width: u32,
    display_height: u32,
    crop_x: u32,
    crop_y: u32,
    full_range: bool,
    max_active_references: usize,
    coincide: bool,
    video_session: vk::VideoSessionKHR,
    session_memory: Vec<vk::DeviceMemory>,
    session_parameters: vk::VideoSessionParametersKHR,
    update_sequence: u32,
    h264_pps: HashSet<u8>,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    bitstream: Buffer,
    bitstream_size_alignment: u64,
    staging: Buffer,
    chroma_offset: u64,
    slot_pool: Rc<SlotPool>,
    reset: bool,
}

impl Session {
    fn new(
        core: Arc<VulkanCore>,
        codec: Codec,
        chroma: Chroma,
        profile_choice: ProfileChoice,
        coded_width: u32,
        coded_height: u32,
        display_width: u32,
        display_height: u32,
        crop_x: u32,
        crop_y: u32,
        full_range: bool,
    ) -> Result<Self, Error> {
        validate_dimensions(chroma, display_width, display_height)?;
        if crop_x
            .checked_add(display_width)
            .is_none_or(|right| right > coded_width)
            || crop_y
                .checked_add(display_height)
                .is_none_or(|bottom| bottom > coded_height)
        {
            return Err(Error::Invalid(format!(
                "display crop {crop_x},{crop_y} + {display_width}x{display_height} exceeds {coded_width}x{coded_height}"
            )));
        }
        let queried = query_caps(
            &core.video_instance,
            core.physical_device,
            codec,
            chroma,
            profile_choice,
        )?;
        if coded_width < queried.min_coded_extent.width
            || coded_height < queried.min_coded_extent.height
            || coded_width > queried.max_coded_extent.width
            || coded_height > queried.max_coded_extent.height
        {
            return Err(Error::Unavailable(format!(
                "coded extent {coded_width}x{coded_height} outside driver range {}x{}..{}x{}",
                queried.min_coded_extent.width,
                queried.min_coded_extent.height,
                queried.max_coded_extent.width,
                queried.max_coded_extent.height,
            )));
        }
        let coincide = queried.coincide;
        let max_dpb_slots = queried.max_dpb_slots.min(17);
        let max_active_refs = queried.max_active_reference_pictures.min(16);
        if max_dpb_slots < 2 || max_active_refs == 0 {
            return Err(Error::Unavailable(format!(
                "driver exposes only {max_dpb_slots} DPB slots and {max_active_refs} active references"
            )));
        }
        let format = chroma.format();

        let video_session = with_profile(codec, chroma, profile_choice, |profile| {
            let create = vk::VideoSessionCreateInfoKHR::default()
                .queue_family_index(core.queue_family)
                .video_profile(profile)
                .picture_format(format)
                .max_coded_extent(vk::Extent2D {
                    width: coded_width,
                    height: coded_height,
                })
                .reference_picture_format(format)
                .max_dpb_slots(max_dpb_slots)
                .max_active_reference_pictures(max_active_refs)
                .std_header_version(&queried.std_header_version);
            let mut session = vk::VideoSessionKHR::null();
            let result = unsafe {
                (core.video_device.fp().create_video_session_khr)(
                    core.device.handle(),
                    &create,
                    ptr::null(),
                    &mut session,
                )
            };
            if result == vk::Result::SUCCESS {
                Ok(session)
            } else {
                Err(vk_unavailable("vkCreateVideoSessionKHR", result))
            }
        })?;

        let session_memory = allocate_session_memory(&core, video_session)?;
        let command_create = vk::CommandPoolCreateInfo::default()
            .queue_family_index(core.queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = unsafe { core.device.create_command_pool(&command_create, None) }
            .map_err(|e| vk_resource("vkCreateCommandPool", e))?;
        let command_allocate = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let command_buffer = unsafe { core.device.allocate_command_buffers(&command_allocate) }
            .map_err(|e| vk_resource("vkAllocateCommandBuffers", e))?[0];
        let fence = unsafe {
            core.device
                .create_fence(&vk::FenceCreateInfo::default(), None)
        }
        .map_err(|e| vk_resource("vkCreateFence", e))?;
        let bitstream_size = align_up(
            MAX_ENCODED_BYTES as u64 + queried.min_bitstream_buffer_size_alignment,
            queried.min_bitstream_buffer_size_alignment.max(1),
        );
        let bitstream = Buffer::new(
            core.clone(),
            bitstream_size,
            vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR,
            true,
        )?;
        let luma_size = u64::from(coded_width) * u64::from(coded_height);
        let chroma_offset = align_up(luma_size, core.transfer_alignment);
        let chroma_size = u64::from(chroma.chroma_width(coded_width))
            * u64::from(chroma.chroma_height(coded_height))
            * 2;
        let staging = Buffer::new(
            core.clone(),
            chroma_offset + chroma_size,
            vk::BufferUsageFlags::TRANSFER_DST,
            true,
        )?;
        let slot_pool = Rc::new(SlotPool {
            free: RefCell::new((0..max_dpb_slots as i32).rev().collect()),
        });

        Ok(Self {
            core,
            codec,
            chroma,
            profile_choice,
            coded_width,
            coded_height,
            display_width,
            display_height,
            crop_x,
            crop_y,
            full_range,
            max_active_references: max_active_refs as usize,
            coincide,
            video_session,
            session_memory,
            session_parameters: vk::VideoSessionParametersKHR::null(),
            update_sequence: 0,
            h264_pps: HashSet::new(),
            command_pool,
            command_buffer,
            fence,
            bitstream,
            bitstream_size_alignment: queried.min_bitstream_buffer_size_alignment.max(1),
            staging,
            chroma_offset,
            slot_pool,
            reset: true,
        })
    }

    fn new_picture(&self, timestamp: u64, frame: CpuFrame) -> NewPictureResult<Picture> {
        let slot_index = self
            .slot_pool
            .free
            .borrow_mut()
            .pop()
            .ok_or(NewPictureError::OutOfOutputBuffers)?;
        let slot = Rc::new(SlotLease {
            index: slot_index,
            pool: Rc::downgrade(&self.slot_pool),
        });
        let result = with_profile(self.codec, self.chroma, self.profile_choice, |profile| {
            let common_usage = vk::ImageUsageFlags::TRANSFER_SRC;
            let dpb_usage = vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR;
            if self.coincide {
                let surface = Surface::new(
                    self.core.clone(),
                    profile,
                    self.chroma.format(),
                    self.coded_width,
                    self.coded_height,
                    common_usage | dpb_usage | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR,
                )?;
                Ok((surface.clone(), surface))
            } else {
                let dpb = Surface::new(
                    self.core.clone(),
                    profile,
                    self.chroma.format(),
                    self.coded_width,
                    self.coded_height,
                    dpb_usage,
                )?;
                let output = Surface::new(
                    self.core.clone(),
                    profile,
                    self.chroma.format(),
                    self.coded_width,
                    self.coded_height,
                    common_usage | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR,
                )?;
                Ok((dpb, output))
            }
        });
        let (dpb, output) = result.map_err(|e: Error| NewPictureError::BackendError(anyhow!(e)))?;
        Ok(Picture {
            timestamp,
            frame: Arc::new(frame),
            dpb,
            output,
            slot,
            h264: None,
            av1: None,
            bitstream: Vec::new(),
            offsets: Vec::new(),
            sizes: Vec::new(),
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = self.core.device.device_wait_idle();
            self.core.device.destroy_fence(self.fence, None);
            self.core
                .device
                .destroy_command_pool(self.command_pool, None);
            if self.session_parameters != vk::VideoSessionParametersKHR::null() {
                (self
                    .core
                    .video_device
                    .fp()
                    .destroy_video_session_parameters_khr)(
                    self.core.device.handle(),
                    self.session_parameters,
                    ptr::null(),
                );
            }
            (self.core.video_device.fp().destroy_video_session_khr)(
                self.core.device.handle(),
                self.video_session,
                ptr::null(),
            );
            for memory in self.session_memory.drain(..) {
                self.core.device.free_memory(memory, None);
            }
        }
    }
}

fn allocate_session_memory(
    core: &Arc<VulkanCore>,
    session: vk::VideoSessionKHR,
) -> Result<Vec<vk::DeviceMemory>, Error> {
    let mut count = 0;
    let result = unsafe {
        (core
            .video_device
            .fp()
            .get_video_session_memory_requirements_khr)(
            core.device.handle(),
            session,
            &mut count,
            ptr::null_mut(),
        )
    };
    if result != vk::Result::SUCCESS {
        return Err(vk_resource(
            "vkGetVideoSessionMemoryRequirementsKHR(count)",
            result,
        ));
    }
    let mut requirements = vec![vk::VideoSessionMemoryRequirementsKHR::default(); count as usize];
    let result = unsafe {
        (core
            .video_device
            .fp()
            .get_video_session_memory_requirements_khr)(
            core.device.handle(),
            session,
            &mut count,
            requirements.as_mut_ptr(),
        )
    };
    if result != vk::Result::SUCCESS {
        return Err(vk_resource(
            "vkGetVideoSessionMemoryRequirementsKHR",
            result,
        ));
    }
    let mut memory = Vec::with_capacity(count as usize);
    let mut bindings = Vec::with_capacity(count as usize);
    for requirement in requirements.into_iter().take(count as usize) {
        let memory_type = core
            .memory_type(
                requirement.memory_requirements.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .or_else(|| {
                core.memory_type(
                    requirement.memory_requirements.memory_type_bits,
                    vk::MemoryPropertyFlags::empty(),
                )
            })
            .ok_or_else(|| Error::Unavailable("no video-session memory type".into()))?;
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirement.memory_requirements.size)
            .memory_type_index(memory_type);
        let allocated = unsafe { core.device.allocate_memory(&allocate, None) }
            .map_err(|e| vk_resource("vkAllocateMemory(video session)", e))?;
        memory.push(allocated);
        bindings.push(
            vk::BindVideoSessionMemoryInfoKHR::default()
                .memory_bind_index(requirement.memory_bind_index)
                .memory(allocated)
                .memory_offset(0)
                .memory_size(requirement.memory_requirements.size),
        );
    }
    let result = unsafe {
        (core.video_device.fp().bind_video_session_memory_khr)(
            core.device.handle(),
            session,
            bindings.len() as u32,
            bindings.as_ptr(),
        )
    };
    if result != vk::Result::SUCCESS {
        for allocated in memory.drain(..) {
            unsafe { core.device.free_memory(allocated, None) };
        }
        return Err(vk_resource("vkBindVideoSessionMemoryKHR", result));
    }
    Ok(memory)
}

struct H264PictureState {
    std_picture: StdVideoDecodeH264PictureInfo,
    references: Vec<H264Reference>,
}

struct H264Reference {
    handle: VkHandle,
    std_reference: StdVideoDecodeH264ReferenceInfo,
}

struct Av1PictureState {
    header: FrameHeaderObu,
    references: [Option<VkHandle>; NUM_REF_FRAMES],
}

pub(crate) struct Picture {
    timestamp: u64,
    frame: Arc<CpuFrame>,
    dpb: Arc<Surface>,
    output: Arc<Surface>,
    slot: Rc<SlotLease>,
    h264: Option<H264PictureState>,
    av1: Option<Av1PictureState>,
    bitstream: Vec<u8>,
    offsets: Vec<u32>,
    sizes: Vec<u32>,
}

pub(crate) struct VulkanBackend {
    core: Arc<VulkanCore>,
    codec: Codec,
    chroma: Chroma,
    expected_width: u32,
    expected_height: u32,
    stream_info: Option<StreamInfo>,
    session: Option<Session>,
}

impl VulkanBackend {
    fn new(codec: Codec, chroma: Chroma, width: u32, height: u32) -> Result<Self, Error> {
        validate_dimensions(chroma, width, height)?;
        Ok(Self {
            core: VulkanCore::new(codec, chroma)?,
            codec,
            chroma,
            expected_width: width,
            expected_height: height,
            stream_info: None,
            session: None,
        })
    }

    fn session_mut(&mut self) -> anyhow::Result<&mut Session> {
        self.session
            .as_mut()
            .ok_or_else(|| anyhow!("Vulkan Video session is not configured"))
    }
}

impl StatelessDecoderBackend for VulkanBackend {
    type Handle = VkHandle;

    fn stream_info(&self) -> Option<&StreamInfo> {
        self.stream_info.as_ref()
    }

    fn reset_backend(&mut self) -> anyhow::Result<()> {
        if let Some(session) = self.session.as_mut() {
            unsafe { session.core.device.device_wait_idle() }
                .context("vkDeviceWaitIdle during decoder reset")?;
            session.reset = true;
        }
        Ok(())
    }
}

impl StatelessDecoderBackendPicture<H264> for VulkanBackend {
    type Picture = Picture;
}

impl StatelessDecoderBackendPicture<Av1> for VulkanBackend {
    type Picture = Picture;
}

fn h264_profile(sps: &Sps) -> anyhow::Result<StdVideoH264ProfileIdc> {
    match sps.profile_idc {
        66 => Ok(StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_BASELINE),
        77 => Ok(StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_MAIN),
        100 => Ok(StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH),
        244 => Ok(StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH_444_PREDICTIVE),
        profile => bail!("unsupported H.264 profile_idc {profile}"),
    }
}

fn h264_level(level: u8) -> anyhow::Result<StdVideoH264LevelIdc> {
    Ok(match level {
        10 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_0,
        11 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_1,
        12 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_2,
        13 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_3,
        20 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_0,
        21 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_1,
        22 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_2,
        30 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_0,
        31 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_1,
        32 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_2,
        40 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_0,
        41 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_1,
        42 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_2,
        50 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_0,
        51 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_1,
        52 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_2,
        60 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_0,
        61 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_1,
        62 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2,
        other => bail!("unsupported H.264 level_idc {other}"),
    })
}

fn h264_scaling_lists(
    lists_4x4: [[u8; 16]; 6],
    lists_8x8: [[u8; 64]; 6],
) -> StdVideoH264ScalingLists {
    StdVideoH264ScalingLists {
        scaling_list_present_mask: 0x0fff,
        use_default_scaling_matrix_mask: 0,
        ScalingList4x4: lists_4x4,
        ScalingList8x8: lists_8x8,
    }
}

fn std_h264_sps(
    sps: &Sps,
    scaling: &StdVideoH264ScalingLists,
) -> anyhow::Result<StdVideoH264SequenceParameterSet> {
    let mut flags: StdVideoH264SpsFlags = unsafe { std::mem::zeroed() };
    flags.set_constraint_set0_flag(sps.constraint_set0_flag.into());
    flags.set_constraint_set1_flag(sps.constraint_set1_flag.into());
    flags.set_constraint_set2_flag(sps.constraint_set2_flag.into());
    flags.set_constraint_set3_flag(sps.constraint_set3_flag.into());
    flags.set_constraint_set4_flag(sps.constraint_set4_flag.into());
    flags.set_constraint_set5_flag(sps.constraint_set5_flag.into());
    flags.set_direct_8x8_inference_flag(sps.direct_8x8_inference_flag.into());
    flags.set_mb_adaptive_frame_field_flag(sps.mb_adaptive_frame_field_flag.into());
    flags.set_frame_mbs_only_flag(sps.frame_mbs_only_flag.into());
    flags.set_delta_pic_order_always_zero_flag(sps.delta_pic_order_always_zero_flag.into());
    flags.set_separate_colour_plane_flag(sps.separate_colour_plane_flag.into());
    flags.set_gaps_in_frame_num_value_allowed_flag(sps.gaps_in_frame_num_value_allowed_flag.into());
    flags.set_qpprime_y_zero_transform_bypass_flag(sps.qpprime_y_zero_transform_bypass_flag.into());
    flags.set_frame_cropping_flag(sps.frame_cropping_flag.into());
    flags.set_seq_scaling_matrix_present_flag(sps.seq_scaling_matrix_present_flag.into());
    // VUI is not needed by the decode operation, and leaving its pointer null
    // avoids exposing a partial VUI structure.
    flags.set_vui_parameters_present_flag(0);
    Ok(StdVideoH264SequenceParameterSet {
        flags,
        profile_idc: h264_profile(sps)?,
        level_idc: h264_level(sps.level_idc as u8)?,
        chroma_format_idc: sps.chroma_format_idc.into(),
        seq_parameter_set_id: sps.seq_parameter_set_id,
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        log2_max_frame_num_minus4: sps.log2_max_frame_num_minus4,
        pic_order_cnt_type: sps.pic_order_cnt_type.into(),
        offset_for_non_ref_pic: sps.offset_for_non_ref_pic,
        offset_for_top_to_bottom_field: sps.offset_for_top_to_bottom_field,
        log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
        num_ref_frames_in_pic_order_cnt_cycle: sps.num_ref_frames_in_pic_order_cnt_cycle,
        max_num_ref_frames: sps.max_num_ref_frames,
        reserved1: 0,
        pic_width_in_mbs_minus1: sps.pic_width_in_mbs_minus1.into(),
        pic_height_in_map_units_minus1: sps.pic_height_in_map_units_minus1.into(),
        frame_crop_left_offset: sps.frame_crop_left_offset,
        frame_crop_right_offset: sps.frame_crop_right_offset,
        frame_crop_top_offset: sps.frame_crop_top_offset,
        frame_crop_bottom_offset: sps.frame_crop_bottom_offset,
        reserved2: 0,
        pOffsetForRefFrame: sps.offset_for_ref_frame.as_ptr(),
        pScalingLists: scaling,
        pSequenceParameterSetVui: ptr::null(),
    })
}

fn std_h264_pps(pps: &Pps, scaling: &StdVideoH264ScalingLists) -> StdVideoH264PictureParameterSet {
    let mut flags: StdVideoH264PpsFlags = unsafe { std::mem::zeroed() };
    flags.set_transform_8x8_mode_flag(pps.transform_8x8_mode_flag.into());
    flags.set_redundant_pic_cnt_present_flag(pps.redundant_pic_cnt_present_flag.into());
    flags.set_constrained_intra_pred_flag(pps.constrained_intra_pred_flag.into());
    flags.set_deblocking_filter_control_present_flag(
        pps.deblocking_filter_control_present_flag.into(),
    );
    flags.set_weighted_pred_flag(pps.weighted_pred_flag.into());
    flags.set_bottom_field_pic_order_in_frame_present_flag(
        pps.bottom_field_pic_order_in_frame_present_flag.into(),
    );
    flags.set_entropy_coding_mode_flag(pps.entropy_coding_mode_flag.into());
    flags.set_pic_scaling_matrix_present_flag(pps.pic_scaling_matrix_present_flag.into());
    StdVideoH264PictureParameterSet {
        flags,
        seq_parameter_set_id: pps.seq_parameter_set_id,
        pic_parameter_set_id: pps.pic_parameter_set_id,
        num_ref_idx_l0_default_active_minus1: pps.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1: pps.num_ref_idx_l1_default_active_minus1,
        weighted_bipred_idc: pps.weighted_bipred_idc.into(),
        pic_init_qp_minus26: pps.pic_init_qp_minus26,
        pic_init_qs_minus26: pps.pic_init_qs_minus26,
        chroma_qp_index_offset: pps.chroma_qp_index_offset,
        second_chroma_qp_index_offset: pps.second_chroma_qp_index_offset,
        pScalingLists: scaling,
    }
}

fn ensure_h264_parameters(session: &mut Session, sps: &Sps, pps: &Pps) -> anyhow::Result<()> {
    if session.h264_pps.contains(&pps.pic_parameter_set_id) {
        return Ok(());
    }
    let sps_scaling = h264_scaling_lists(sps.scaling_lists_4x4, sps.scaling_lists_8x8);
    let pps_scaling = h264_scaling_lists(pps.scaling_lists_4x4, pps.scaling_lists_8x8);
    let std_sps = std_h264_sps(sps, &sps_scaling)?;
    let std_pps = std_h264_pps(pps, &pps_scaling);
    let spss = [std_sps];
    let ppss = [std_pps];
    let add = vk::VideoDecodeH264SessionParametersAddInfoKHR::default()
        .std_sp_ss(&spss)
        .std_pp_ss(&ppss);
    if session.session_parameters == vk::VideoSessionParametersKHR::null() {
        let mut codec_create = vk::VideoDecodeH264SessionParametersCreateInfoKHR::default()
            .max_std_sps_count(32)
            .max_std_pps_count(256)
            .parameters_add_info(&add);
        let create = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(session.video_session)
            .push_next(&mut codec_create);
        let result = unsafe {
            (session
                .core
                .video_device
                .fp()
                .create_video_session_parameters_khr)(
                session.core.device.handle(),
                &create,
                ptr::null(),
                &mut session.session_parameters,
            )
        };
        if result != vk::Result::SUCCESS {
            bail!("vkCreateVideoSessionParametersKHR(H.264): {result:?}");
        }
    } else {
        session.update_sequence = session.update_sequence.wrapping_add(1);
        let mut add = add;
        let update = vk::VideoSessionParametersUpdateInfoKHR::default()
            .update_sequence_count(session.update_sequence)
            .push_next(&mut add);
        let result = unsafe {
            (session
                .core
                .video_device
                .fp()
                .update_video_session_parameters_khr)(
                session.core.device.handle(),
                session.session_parameters,
                &update,
            )
        };
        if result != vk::Result::SUCCESS {
            bail!("vkUpdateVideoSessionParametersKHR(H.264): {result:?}");
        }
    }
    session.h264_pps.insert(pps.pic_parameter_set_id);
    Ok(())
}

fn std_h264_reference(picture: &PictureData) -> StdVideoDecodeH264ReferenceInfo {
    let mut flags: StdVideoDecodeH264ReferenceInfoFlags = unsafe { std::mem::zeroed() };
    flags.set_top_field_flag(matches!(picture.field, Field::Top).into());
    flags.set_bottom_field_flag(matches!(picture.field, Field::Bottom).into());
    flags.set_used_for_long_term_reference(
        matches!(picture.reference(), Reference::LongTerm).into(),
    );
    flags.set_is_non_existing(picture.nonexisting.into());
    StdVideoDecodeH264ReferenceInfo {
        flags,
        FrameNum: if matches!(picture.reference(), Reference::LongTerm) {
            picture.long_term_frame_idx as u16
        } else {
            picture.frame_num as u16
        },
        reserved: 0,
        PicOrderCnt: [picture.top_field_order_cnt, picture.bottom_field_order_cnt],
    }
}

impl StatelessH264DecoderBackend for VulkanBackend {
    fn new_sequence(&mut self, sps: &Rc<Sps>) -> StatelessBackendResult<()> {
        if self.codec != Codec::H264 {
            return Err(anyhow!("H.264 sequence sent to non-H.264 backend").into());
        }
        if sps.bit_depth_luma_minus8 != 0 || sps.bit_depth_chroma_minus8 != 0 {
            return Err(anyhow!("only 8-bit H.264 is supported").into());
        }
        let actual_chroma = match sps.chroma_format_idc {
            1 => Chroma::Cs420,
            3 if !sps.separate_colour_plane_flag => Chroma::Cs444,
            other => return Err(anyhow!("unsupported H.264 chroma_format_idc {other}").into()),
        };
        if actual_chroma != self.chroma {
            return Err(anyhow!(
                "H.264 stream is {}, negotiated {}",
                actual_chroma.label(),
                self.chroma.label()
            )
            .into());
        }
        let rect = sps.visible_rectangle();
        let display_width = rect.max.x - rect.min.x;
        let display_height = rect.max.y - rect.min.y;
        if display_width != self.expected_width || display_height != self.expected_height {
            return Err(anyhow!(
                "H.264 dimensions {display_width}x{display_height}, expected {}x{}",
                self.expected_width,
                self.expected_height
            )
            .into());
        }
        if !sps.frame_mbs_only_flag {
            return Err(anyhow!("interlaced H.264 camera input is unsupported").into());
        }
        let profile_choice = ProfileChoice {
            h264: h264_profile(sps)?,
            av1: 0,
        };
        self.session = Some(
            Session::new(
                self.core.clone(),
                Codec::H264,
                self.chroma,
                profile_choice,
                sps.width(),
                sps.height(),
                display_width,
                display_height,
                rect.min.x,
                rect.min.y,
                sps.vui_parameters_present_flag
                    && sps.vui_parameters.video_signal_type_present_flag
                    && sps.vui_parameters.video_full_range_flag,
            )
            .map_err(anyhow::Error::new)?,
        );
        self.stream_info = Some(StreamInfo {
            format: match self.chroma {
                Chroma::Cs420 => DecodedFormat::NV12,
                Chroma::Cs444 => DecodedFormat::I444,
            },
            coded_resolution: (sps.width(), sps.height()).into(),
            display_resolution: (display_width, display_height).into(),
            min_num_frames: usize::from(sps.max_num_ref_frames) + 2,
        });
        Ok(())
    }

    fn new_picture(
        &mut self,
        timestamp: u64,
        alloc_cb: &mut dyn FnMut() -> Option<CpuFrame>,
    ) -> NewPictureResult<Self::Picture> {
        let frame = alloc_cb().ok_or(NewPictureError::OutOfOutputBuffers)?;
        self.session
            .as_ref()
            .ok_or_else(|| NewPictureError::BackendError(anyhow!("H.264 session missing")))?
            .new_picture(timestamp, frame)
    }

    fn new_field_picture(
        &mut self,
        _timestamp: u64,
        _first_field: &Self::Handle,
    ) -> NewPictureResult<Self::Picture> {
        Err(NewPictureError::BackendError(anyhow!(
            "interlaced H.264 is unsupported"
        )))
    }

    fn start_picture(
        &mut self,
        picture: &mut Self::Picture,
        picture_data: &PictureData,
        sps: &Sps,
        pps: &Pps,
        dpb: &Dpb<Self::Handle>,
        hdr: &SliceHeader,
    ) -> StatelessBackendResult<()> {
        let session = self.session_mut()?;
        ensure_h264_parameters(session, sps, pps)?;
        let mut flags: StdVideoDecodeH264PictureInfoFlags = unsafe { std::mem::zeroed() };
        flags.set_field_pic_flag(hdr.field_pic_flag.into());
        flags.set_is_intra(hdr.slice_type.is_i().into());
        flags.set_IdrPicFlag(matches!(picture_data.is_idr, IsIdr::Yes { .. }).into());
        flags.set_bottom_field_flag(hdr.bottom_field_flag.into());
        flags.set_is_reference((picture_data.nal_ref_idc != 0).into());
        flags.set_complementary_field_pair(
            picture_data
                .is_second_field_of_complementary_ref_pair()
                .into(),
        );
        picture.h264 = Some(H264PictureState {
            std_picture: StdVideoDecodeH264PictureInfo {
                flags,
                seq_parameter_set_id: sps.seq_parameter_set_id,
                pic_parameter_set_id: pps.pic_parameter_set_id,
                reserved1: 0,
                reserved2: 0,
                frame_num: hdr.frame_num,
                idr_pic_id: hdr.idr_pic_id,
                PicOrderCnt: [
                    picture_data.top_field_order_cnt,
                    picture_data.bottom_field_order_cnt,
                ],
            },
            references: dpb
                .entries()
                .iter()
                .filter_map(|entry| {
                    entry.reference.clone().map(|handle| H264Reference {
                        handle,
                        std_reference: std_h264_reference(&entry.pic.borrow()),
                    })
                })
                .collect(),
        });
        Ok(())
    }

    fn decode_slice(
        &mut self,
        picture: &mut Self::Picture,
        slice: &Slice,
        _sps: &Sps,
        _pps: &Pps,
        _ref_pic_list0: &[&DpbEntry<Self::Handle>],
        _ref_pic_list1: &[&DpbEntry<Self::Handle>],
    ) -> StatelessBackendResult<()> {
        let offset = u32::try_from(picture.bitstream.len())
            .context("H.264 picture bitstream exceeds u32")?;
        picture.offsets.push(offset);
        picture.bitstream.extend_from_slice(&[0, 0, 1]);
        picture.bitstream.extend_from_slice(slice.nalu.as_ref());
        Ok(())
    }

    fn submit_picture(&mut self, picture: Self::Picture) -> StatelessBackendResult<Self::Handle> {
        let session = self.session_mut()?;
        submit_h264(session, &picture)?;
        Ok(finish_picture(session, picture)?)
    }
}

fn av1_profile(profile: Av1Profile) -> StdVideoAV1Profile {
    match profile {
        Av1Profile::Profile0 => StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
        Av1Profile::Profile1 => StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_HIGH,
        Av1Profile::Profile2 => StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_PROFESSIONAL,
    }
}

fn create_av1_parameters(session: &mut Session, seq: &SequenceHeaderObu) -> anyhow::Result<()> {
    let color_config = &seq.color_config;
    let mut color_flags: StdVideoAV1ColorConfigFlags = unsafe { std::mem::zeroed() };
    color_flags.set_mono_chrome(color_config.mono_chrome.into());
    color_flags.set_color_range(color_config.color_range.into());
    color_flags.set_separate_uv_delta_q(color_config.separate_uv_delta_q.into());
    color_flags
        .set_color_description_present_flag(color_config.color_description_present_flag.into());
    let color = StdVideoAV1ColorConfig {
        flags: color_flags,
        BitDepth: match seq.bit_depth {
            BitDepth::Depth8 => 8,
            BitDepth::Depth10 => 10,
            BitDepth::Depth12 => 12,
        },
        subsampling_x: color_config.subsampling_x.into(),
        subsampling_y: color_config.subsampling_y.into(),
        reserved1: 0,
        color_primaries: color_config.color_primaries as u32,
        transfer_characteristics: color_config.transfer_characteristics as u32,
        matrix_coefficients: color_config.matrix_coefficients as u32,
        chroma_sample_position: color_config.chroma_sample_position as u32,
    };

    let mut timing_flags: StdVideoAV1TimingInfoFlags = unsafe { std::mem::zeroed() };
    timing_flags.set_equal_picture_interval(seq.timing_info.equal_picture_interval.into());
    let timing = StdVideoAV1TimingInfo {
        flags: timing_flags,
        num_units_in_display_tick: seq.timing_info.num_units_in_display_tick,
        time_scale: seq.timing_info.time_scale,
        num_ticks_per_picture_minus_1: seq.timing_info.num_ticks_per_picture_minus_1,
    };

    let mut flags: StdVideoAV1SequenceHeaderFlags = unsafe { std::mem::zeroed() };
    flags.set_still_picture(seq.still_picture.into());
    flags.set_reduced_still_picture_header(seq.reduced_still_picture_header.into());
    flags.set_use_128x128_superblock(seq.use_128x128_superblock.into());
    flags.set_enable_filter_intra(seq.enable_filter_intra.into());
    flags.set_enable_intra_edge_filter(seq.enable_intra_edge_filter.into());
    flags.set_enable_interintra_compound(seq.enable_interintra_compound.into());
    flags.set_enable_masked_compound(seq.enable_masked_compound.into());
    flags.set_enable_warped_motion(seq.enable_warped_motion.into());
    flags.set_enable_dual_filter(seq.enable_dual_filter.into());
    flags.set_enable_order_hint(seq.enable_order_hint.into());
    flags.set_enable_jnt_comp(seq.enable_jnt_comp.into());
    flags.set_enable_ref_frame_mvs(seq.enable_ref_frame_mvs.into());
    flags.set_frame_id_numbers_present_flag(seq.frame_id_numbers_present_flag.into());
    flags.set_enable_superres(seq.enable_superres.into());
    flags.set_enable_cdef(seq.enable_cdef.into());
    flags.set_enable_restoration(seq.enable_restoration.into());
    flags.set_film_grain_params_present(seq.film_grain_params_present.into());
    flags.set_timing_info_present_flag(seq.timing_info_present_flag.into());
    flags.set_initial_display_delay_present_flag(seq.initial_display_delay_present_flag.into());
    let sequence = StdVideoAV1SequenceHeader {
        flags,
        seq_profile: av1_profile(seq.seq_profile),
        frame_width_bits_minus_1: seq.frame_width_bits_minus_1,
        frame_height_bits_minus_1: seq.frame_height_bits_minus_1,
        max_frame_width_minus_1: seq.max_frame_width_minus_1,
        max_frame_height_minus_1: seq.max_frame_height_minus_1,
        delta_frame_id_length_minus_2: u8::try_from(seq.delta_frame_id_length_minus_2)
            .context("AV1 delta_frame_id_length_minus_2 exceeds u8")?,
        additional_frame_id_length_minus_1: u8::try_from(seq.additional_frame_id_length_minus_1)
            .context("AV1 additional_frame_id_length_minus_1 exceeds u8")?,
        order_hint_bits_minus_1: u8::try_from(seq.order_hint_bits_minus_1.max(0))
            .context("AV1 order_hint_bits_minus_1 exceeds u8")?,
        seq_force_integer_mv: u8::try_from(seq.seq_force_integer_mv)
            .context("AV1 seq_force_integer_mv exceeds u8")?,
        seq_force_screen_content_tools: u8::try_from(seq.seq_force_screen_content_tools)
            .context("AV1 seq_force_screen_content_tools exceeds u8")?,
        reserved1: [0; 5],
        pColorConfig: &color,
        pTimingInfo: if seq.timing_info_present_flag {
            &timing
        } else {
            ptr::null()
        },
    };
    let mut codec_create =
        vk::VideoDecodeAV1SessionParametersCreateInfoKHR::default().std_sequence_header(&sequence);
    let create = vk::VideoSessionParametersCreateInfoKHR::default()
        .video_session(session.video_session)
        .push_next(&mut codec_create);
    let result = unsafe {
        (session
            .core
            .video_device
            .fp()
            .create_video_session_parameters_khr)(
            session.core.device.handle(),
            &create,
            ptr::null(),
            &mut session.session_parameters,
        )
    };
    if result != vk::Result::SUCCESS {
        bail!("vkCreateVideoSessionParametersKHR(AV1): {result:?}");
    }
    Ok(())
}

fn std_av1_reference(header: &FrameHeaderObu) -> StdVideoDecodeAV1ReferenceInfo {
    let mut flags: StdVideoDecodeAV1ReferenceInfoFlags = unsafe { std::mem::zeroed() };
    flags.set_disable_frame_end_update_cdf(header.disable_frame_end_update_cdf.into());
    flags.set_segmentation_enabled(header.segmentation_params.segmentation_enabled.into());
    let mut saved_order_hints = [0u8; NUM_REF_FRAMES];
    for (dst, src) in saved_order_hints.iter_mut().zip(header.order_hints) {
        *dst = src as u8;
    }
    let sign_bias = header
        .ref_frame_sign_bias
        .iter()
        .enumerate()
        .fold(0u8, |mask, (index, set)| mask | (u8::from(*set) << index));
    StdVideoDecodeAV1ReferenceInfo {
        flags,
        frame_type: header.frame_type as u8,
        RefFrameSignBias: sign_bias,
        OrderHint: header.order_hint as u8,
        SavedOrderHints: saved_order_hints,
    }
}

impl StatelessAV1DecoderBackend for VulkanBackend {
    fn change_stream_info(&mut self, stream_info: &Av1StreamInfo) -> StatelessBackendResult<()> {
        if self.codec != Codec::Av1 {
            return Err(anyhow!("AV1 sequence sent to non-AV1 backend").into());
        }
        let seq = stream_info.seq_header.as_ref();
        if seq.bit_depth != BitDepth::Depth8 || seq.color_config.mono_chrome {
            return Err(anyhow!("only 8-bit, three-plane AV1 is supported").into());
        }
        let actual_chroma = match (
            seq.seq_profile,
            seq.color_config.subsampling_x,
            seq.color_config.subsampling_y,
        ) {
            (Av1Profile::Profile0, true, true) => Chroma::Cs420,
            (Av1Profile::Profile1, false, false) => Chroma::Cs444,
            (profile, x, y) => {
                return Err(anyhow!(
                    "unsupported AV1 profile/subsampling combination {profile:?}, x={x}, y={y}"
                )
                .into());
            }
        };
        if actual_chroma != self.chroma {
            return Err(anyhow!(
                "AV1 stream is {}, negotiated {}",
                actual_chroma.label(),
                self.chroma.label()
            )
            .into());
        }
        if stream_info.render_width != self.expected_width
            || stream_info.render_height != self.expected_height
        {
            return Err(anyhow!(
                "AV1 render size {}x{}, expected {}x{}",
                stream_info.render_width,
                stream_info.render_height,
                self.expected_width,
                self.expected_height
            )
            .into());
        }
        let coded_width = u32::from(seq.max_frame_width_minus_1) + 1;
        let coded_height = u32::from(seq.max_frame_height_minus_1) + 1;
        if coded_width != self.expected_width || coded_height != self.expected_height {
            return Err(anyhow!(
                "AV1 sequence maximum {coded_width}x{coded_height} differs from fixed camera size {}x{}",
                self.expected_width,
                self.expected_height
            )
            .into());
        }
        let profile_choice = ProfileChoice {
            h264: 0,
            av1: av1_profile(seq.seq_profile),
        };
        let mut session = Session::new(
            self.core.clone(),
            Codec::Av1,
            self.chroma,
            profile_choice,
            coded_width,
            coded_height,
            stream_info.render_width,
            stream_info.render_height,
            0,
            0,
            seq.color_config.color_range,
        )
        .map_err(anyhow::Error::new)?;
        create_av1_parameters(&mut session, seq)?;
        self.session = Some(session);
        self.stream_info = Some(StreamInfo {
            format: match self.chroma {
                Chroma::Cs420 => DecodedFormat::NV12,
                Chroma::Cs444 => DecodedFormat::I444,
            },
            coded_resolution: (coded_width, coded_height).into(),
            display_resolution: (stream_info.render_width, stream_info.render_height).into(),
            min_num_frames: NUM_REF_FRAMES + 2,
        });
        Ok(())
    }

    fn new_picture(
        &mut self,
        _hdr: &FrameHeaderObu,
        timestamp: u64,
        alloc_cb: &mut dyn FnMut() -> Option<CpuFrame>,
    ) -> NewPictureResult<Self::Picture> {
        let frame = alloc_cb().ok_or(NewPictureError::OutOfOutputBuffers)?;
        self.session
            .as_ref()
            .ok_or_else(|| NewPictureError::BackendError(anyhow!("AV1 session missing")))?
            .new_picture(timestamp, frame)
    }

    fn begin_picture(
        &mut self,
        picture: &mut Self::Picture,
        _stream_info: &Av1StreamInfo,
        hdr: &FrameHeaderObu,
        reference_frames: &[Option<Self::Handle>; NUM_REF_FRAMES],
    ) -> StatelessBackendResult<()> {
        if hdr.render_width != self.expected_width || hdr.render_height != self.expected_height {
            return Err(anyhow!(
                "AV1 frame render size {}x{} changed from {}x{}",
                hdr.render_width,
                hdr.render_height,
                self.expected_width,
                self.expected_height
            )
            .into());
        }
        if hdr.upscaled_width != self.expected_width || hdr.frame_height != self.expected_height {
            return Err(anyhow!(
                "AV1 frame output size {}x{} changed from {}x{}",
                hdr.upscaled_width,
                hdr.frame_height,
                self.expected_width,
                self.expected_height
            )
            .into());
        }
        if hdr.film_grain_params.apply_grain {
            return Err(
                anyhow!("AV1 film-grain synthesis is not exposed by this Vulkan profile").into(),
            );
        }
        picture.av1 = Some(Av1PictureState {
            header: hdr.clone(),
            references: reference_frames.clone(),
        });
        Ok(())
    }

    fn decode_tile_group(
        &mut self,
        picture: &mut Self::Picture,
        tile_group: TileGroupObu,
    ) -> StatelessBackendResult<()> {
        let obu = tile_group.obu.as_ref();
        for tile in &tile_group.tiles {
            let start =
                usize::try_from(tile.tile_offset).context("AV1 tile offset exceeds usize")?;
            let size = usize::try_from(tile.tile_size).context("AV1 tile size exceeds usize")?;
            let end = start
                .checked_add(size)
                .ok_or_else(|| anyhow!("AV1 tile range overflow"))?;
            let bytes = obu.get(start..end).ok_or_else(|| {
                anyhow!(
                    "AV1 tile range {start}..{end} exceeds OBU size {}",
                    obu.len()
                )
            })?;
            picture
                .offsets
                .push(u32::try_from(picture.bitstream.len()).context("AV1 picture exceeds u32")?);
            picture.sizes.push(tile.tile_size);
            picture.bitstream.extend_from_slice(bytes);
        }
        Ok(())
    }

    fn submit_picture(&mut self, picture: Self::Picture) -> StatelessBackendResult<Self::Handle> {
        let session = self.session_mut()?;
        submit_av1(session, &picture)?;
        Ok(finish_picture(session, picture)?)
    }
}

fn submit_av1(session: &mut Session, picture: &Picture) -> anyhow::Result<()> {
    if picture.bitstream.is_empty() || picture.bitstream.len() > MAX_ENCODED_BYTES {
        bail!(
            "invalid Vulkan AV1 bitstream length {}",
            picture.bitstream.len()
        );
    }
    if picture.offsets.len() != picture.sizes.len() || picture.offsets.is_empty() {
        bail!("AV1 tile offset/size arrays are inconsistent");
    }
    let aligned_size = align_up(
        picture.bitstream.len() as u64,
        session.bitstream_size_alignment,
    );
    if aligned_size > session.bitstream.size {
        bail!("Vulkan AV1 bitstream exceeds allocated buffer");
    }
    unsafe {
        ptr::write_bytes(session.bitstream.mapped, 0, aligned_size as usize);
        ptr::copy_nonoverlapping(
            picture.bitstream.as_ptr(),
            session.bitstream.mapped,
            picture.bitstream.len(),
        );
    }
    record_av1_submission(session, picture, aligned_size)
}

fn record_av1_submission(
    session: &mut Session,
    picture: &Picture,
    aligned_size: u64,
) -> anyhow::Result<()> {
    let state = picture
        .av1
        .as_ref()
        .ok_or_else(|| anyhow!("AV1 picture state missing"))?;
    let header = &state.header;

    let tile = &header.tile_info;
    let mut mi_col_starts = [0u16; MAX_TILE_COLS + 1];
    let mut mi_row_starts = [0u16; MAX_TILE_ROWS + 1];
    let mut width_in_sbs_minus_1 = [0u16; MAX_TILE_COLS];
    let mut height_in_sbs_minus_1 = [0u16; MAX_TILE_ROWS];
    for (dst, src) in mi_col_starts.iter_mut().zip(tile.mi_col_starts) {
        *dst = u16::try_from(src).context("AV1 MiColStart exceeds u16")?;
    }
    for (dst, src) in mi_row_starts.iter_mut().zip(tile.mi_row_starts) {
        *dst = u16::try_from(src).context("AV1 MiRowStart exceeds u16")?;
    }
    for (dst, src) in width_in_sbs_minus_1
        .iter_mut()
        .zip(tile.width_in_sbs_minus_1)
    {
        *dst = u16::try_from(src).context("AV1 tile width exceeds u16")?;
    }
    for (dst, src) in height_in_sbs_minus_1
        .iter_mut()
        .zip(tile.height_in_sbs_minus_1)
    {
        *dst = u16::try_from(src).context("AV1 tile height exceeds u16")?;
    }
    let mut tile_flags: StdVideoAV1TileInfoFlags = unsafe { std::mem::zeroed() };
    tile_flags.set_uniform_tile_spacing_flag(tile.uniform_tile_spacing_flag.into());
    let tile_info = StdVideoAV1TileInfo {
        flags: tile_flags,
        TileCols: u8::try_from(tile.tile_cols).context("AV1 TileCols exceeds u8")?,
        TileRows: u8::try_from(tile.tile_rows).context("AV1 TileRows exceeds u8")?,
        context_update_tile_id: u16::try_from(tile.context_update_tile_id)
            .context("AV1 context_update_tile_id exceeds u16")?,
        tile_size_bytes_minus_1: u8::try_from(tile.tile_size_bytes.saturating_sub(1))
            .context("AV1 tile_size_bytes_minus_1 exceeds u8")?,
        reserved1: [0; 7],
        pMiColStarts: mi_col_starts.as_ptr(),
        pMiRowStarts: mi_row_starts.as_ptr(),
        pWidthInSbsMinus1: width_in_sbs_minus_1.as_ptr(),
        pHeightInSbsMinus1: height_in_sbs_minus_1.as_ptr(),
    };

    let quant = &header.quantization_params;
    let mut quant_flags: StdVideoAV1QuantizationFlags = unsafe { std::mem::zeroed() };
    quant_flags.set_using_qmatrix(quant.using_qmatrix.into());
    quant_flags.set_diff_uv_delta(quant.diff_uv_delta.into());
    let quantization = StdVideoAV1Quantization {
        flags: quant_flags,
        base_q_idx: u8::try_from(quant.base_q_idx).context("AV1 base_q_idx exceeds u8")?,
        DeltaQYDc: i8::try_from(quant.delta_q_y_dc).context("AV1 DeltaQYDc exceeds i8")?,
        DeltaQUDc: i8::try_from(quant.delta_q_u_dc).context("AV1 DeltaQUDc exceeds i8")?,
        DeltaQUAc: i8::try_from(quant.delta_q_u_ac).context("AV1 DeltaQUAc exceeds i8")?,
        DeltaQVDc: i8::try_from(quant.delta_q_v_dc).context("AV1 DeltaQVDc exceeds i8")?,
        DeltaQVAc: i8::try_from(quant.delta_q_v_ac).context("AV1 DeltaQVAc exceeds i8")?,
        qm_y: u8::try_from(quant.qm_y).context("AV1 qm_y exceeds u8")?,
        qm_u: u8::try_from(quant.qm_u).context("AV1 qm_u exceeds u8")?,
        qm_v: u8::try_from(quant.qm_v).context("AV1 qm_v exceeds u8")?,
    };

    let seg = &header.segmentation_params;
    let mut feature_enabled = [0u8; MAX_SEGMENTS];
    for (segment, mask) in feature_enabled.iter_mut().enumerate() {
        for feature in 0..SEG_LVL_MAX {
            if seg.feature_enabled[segment][feature] {
                *mask |= 1 << feature;
            }
        }
    }
    let segmentation = StdVideoAV1Segmentation {
        FeatureEnabled: feature_enabled,
        FeatureData: seg.feature_data,
    };

    let filter = &header.loop_filter_params;
    let mut filter_flags: StdVideoAV1LoopFilterFlags = unsafe { std::mem::zeroed() };
    filter_flags.set_loop_filter_delta_enabled(filter.loop_filter_delta_enabled.into());
    filter_flags.set_loop_filter_delta_update(filter.loop_filter_delta_update.into());
    let loop_filter = StdVideoAV1LoopFilter {
        flags: filter_flags,
        loop_filter_level: filter.loop_filter_level,
        loop_filter_sharpness: filter.loop_filter_sharpness,
        update_ref_delta: 0,
        loop_filter_ref_deltas: filter.loop_filter_ref_deltas,
        update_mode_delta: 0,
        loop_filter_mode_deltas: filter.loop_filter_mode_deltas,
    };

    let source_cdef = &header.cdef_params;
    let mut cdef_y_pri_strength = [0u8; 8];
    let mut cdef_y_sec_strength = [0u8; 8];
    let mut cdef_uv_pri_strength = [0u8; 8];
    let mut cdef_uv_sec_strength = [0u8; 8];
    for index in 0..8 {
        cdef_y_pri_strength[index] = u8::try_from(source_cdef.cdef_y_pri_strength[index])
            .context("AV1 CDEF Y primary strength exceeds u8")?;
        cdef_y_sec_strength[index] = u8::try_from(source_cdef.cdef_y_sec_strength[index])
            .context("AV1 CDEF Y secondary strength exceeds u8")?;
        cdef_uv_pri_strength[index] = u8::try_from(source_cdef.cdef_uv_pri_strength[index])
            .context("AV1 CDEF UV primary strength exceeds u8")?;
        cdef_uv_sec_strength[index] = u8::try_from(source_cdef.cdef_uv_sec_strength[index])
            .context("AV1 CDEF UV secondary strength exceeds u8")?;
    }
    let cdef = StdVideoAV1CDEF {
        cdef_damping_minus_3: u8::try_from(source_cdef.cdef_damping.saturating_sub(3))
            .context("AV1 CDEF damping exceeds u8")?,
        cdef_bits: u8::try_from(source_cdef.cdef_bits).context("AV1 CDEF bits exceeds u8")?,
        cdef_y_pri_strength,
        cdef_y_sec_strength,
        cdef_uv_pri_strength,
        cdef_uv_sec_strength,
    };

    let restoration = &header.loop_restoration_params;
    let luma_restoration_size = 1u16 + u16::from(restoration.lr_unit_shift);
    let chroma_restoration_size = luma_restoration_size
        .checked_sub(u16::from(restoration.lr_uv_shift))
        .ok_or_else(|| anyhow!("invalid AV1 loop-restoration shifts"))?;
    let loop_restoration = StdVideoAV1LoopRestoration {
        FrameRestorationType: restoration.frame_restoration_type.map(|kind| kind as u32),
        LoopRestorationSize: [
            luma_restoration_size,
            chroma_restoration_size,
            chroma_restoration_size,
        ],
    };
    let global_motion = StdVideoAV1GlobalMotion {
        GmType: header.global_motion_params.gm_type.map(|kind| kind as u8),
        gm_params: header.global_motion_params.gm_params,
    };

    let mut flags: StdVideoDecodeAV1PictureInfoFlags = unsafe { std::mem::zeroed() };
    flags.set_error_resilient_mode(header.error_resilient_mode.into());
    flags.set_disable_cdf_update(header.disable_cdf_update.into());
    flags.set_use_superres(header.use_superres.into());
    flags.set_render_and_frame_size_different(header.render_and_frame_size_different.into());
    flags.set_allow_screen_content_tools(header.allow_screen_content_tools);
    flags.set_is_filter_switchable(header.is_filter_switchable.into());
    flags.set_force_integer_mv(header.force_integer_mv);
    flags.set_frame_size_override_flag(header.frame_size_override_flag.into());
    flags.set_buffer_removal_time_present_flag(header.buffer_removal_time_present_flag.into());
    flags.set_allow_intrabc(header.allow_intrabc.into());
    flags.set_frame_refs_short_signaling(header.frame_refs_short_signaling.into());
    flags.set_allow_high_precision_mv(header.allow_high_precision_mv.into());
    flags.set_is_motion_mode_switchable(header.is_motion_mode_switchable.into());
    flags.set_use_ref_frame_mvs(header.use_ref_frame_mvs.into());
    flags.set_disable_frame_end_update_cdf(header.disable_frame_end_update_cdf.into());
    flags.set_allow_warped_motion(header.allow_warped_motion.into());
    flags.set_reduced_tx_set(header.reduced_tx_set.into());
    flags.set_reference_select(header.reference_select.into());
    flags.set_skip_mode_present(header.skip_mode_present.into());
    flags.set_delta_q_present(quant.delta_q_present.into());
    flags.set_delta_lf_present(filter.delta_lf_present.into());
    flags.set_delta_lf_multi(filter.delta_lf_multi.into());
    flags.set_segmentation_enabled(seg.segmentation_enabled.into());
    flags.set_segmentation_update_map(seg.segmentation_update_map.into());
    flags.set_segmentation_temporal_update(seg.segmentation_temporal_update.into());
    flags.set_segmentation_update_data(seg.segmentation_update_data.into());
    flags.set_UsesLr(restoration.uses_lr.into());
    flags.set_usesChromaLr(restoration.uses_chroma_lr.into());
    flags.set_apply_grain(0);

    let std_picture = StdVideoDecodeAV1PictureInfo {
        flags,
        frame_type: header.frame_type as u32,
        current_frame_id: header.current_frame_id,
        OrderHint: u8::try_from(header.order_hint).context("AV1 OrderHint exceeds u8")?,
        primary_ref_frame: u8::try_from(header.primary_ref_frame)
            .context("AV1 primary_ref_frame exceeds u8")?,
        refresh_frame_flags: u8::try_from(header.refresh_frame_flags)
            .context("AV1 refresh_frame_flags exceeds u8")?,
        reserved1: 0,
        interpolation_filter: header.interpolation_filter as u32,
        TxMode: header.tx_mode as u32,
        delta_q_res: u8::try_from(quant.delta_q_res).context("AV1 delta_q_res exceeds u8")?,
        delta_lf_res: filter.delta_lf_res,
        SkipModeFrame: header.skip_mode_frame.map(|index| index as u8),
        coded_denom: if header.use_superres {
            u8::try_from(
                header
                    .superres_denom
                    .saturating_sub(SUPERRES_DENOM_MIN as u32),
            )
            .context("AV1 coded_denom exceeds u8")?
        } else {
            0
        },
        reserved2: [0; 3],
        OrderHints: header.order_hints.map(|hint| hint as u8),
        expectedFrameId: [0; NUM_REF_FRAMES],
        pTileInfo: &tile_info,
        pQuantization: &quantization,
        pSegmentation: &segmentation,
        pLoopFilter: &loop_filter,
        pCDEF: &cdef,
        pLoopRestoration: &loop_restoration,
        pGlobalMotion: &global_motion,
        pFilmGrain: ptr::null(),
    };

    let mut reference_name_slot_indices = [-1i32; 7];
    let mut unique_slots = HashSet::new();
    let mut reference_handles = Vec::new();
    if !header.frame_is_intra {
        for (name, reference_index) in header.ref_frame_idx.iter().copied().enumerate() {
            let handle = state.references[usize::from(reference_index)]
                .as_ref()
                .ok_or_else(|| anyhow!("AV1 reference slot {reference_index} is missing"))?;
            reference_name_slot_indices[name] = handle.slot();
            if unique_slots.insert(handle.slot()) {
                reference_handles.push(handle.clone());
            }
        }
    }
    if reference_handles.len() > session.max_active_references {
        bail!(
            "AV1 picture requires {} active references, driver permits {}",
            reference_handles.len(),
            session.max_active_references
        );
    }
    let reference_std: Vec<_> = reference_handles
        .iter()
        .map(|handle| {
            handle
                .0
                .av1_reference
                .ok_or_else(|| anyhow!("AV1 DPB handle lacks reference metadata"))
        })
        .collect::<anyhow::Result<_>>()?;
    let mut reference_codec: Vec<_> = reference_std
        .iter()
        .map(|std| vk::VideoDecodeAV1DpbSlotInfoKHR::default().std_reference_info(std))
        .collect();
    let reference_pictures: Vec<_> = reference_handles
        .iter()
        .map(|handle| {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(vk::Extent2D {
                    width: session.coded_width,
                    height: session.coded_height,
                })
                .image_view_binding(handle.dpb().view)
        })
        .collect();
    let reference_slots: Vec<_> = reference_pictures
        .iter()
        .zip(reference_codec.iter_mut())
        .zip(reference_handles.iter())
        .map(|((resource, codec), handle)| {
            vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(handle.slot())
                .picture_resource(resource)
                .push_next(codec)
        })
        .collect();

    let setup_std = std_av1_reference(header);
    let mut setup_codec =
        vk::VideoDecodeAV1DpbSlotInfoKHR::default().std_reference_info(&setup_std);
    let setup_picture = vk::VideoPictureResourceInfoKHR::default()
        .coded_extent(vk::Extent2D {
            width: session.coded_width,
            height: session.coded_height,
        })
        .image_view_binding(picture.dpb.view);
    let setup_slot = vk::VideoReferenceSlotInfoKHR::default()
        .slot_index(picture.slot.index)
        .picture_resource(&setup_picture)
        .push_next(&mut setup_codec);
    let dst_picture = vk::VideoPictureResourceInfoKHR::default()
        .coded_extent(vk::Extent2D {
            width: session.coded_width,
            height: session.coded_height,
        })
        .image_view_binding(picture.output.view);
    let begin = vk::VideoBeginCodingInfoKHR::default()
        .video_session(session.video_session)
        .video_session_parameters(session.session_parameters)
        .reference_slots(&reference_slots);
    let mut codec_info = vk::VideoDecodeAV1PictureInfoKHR::default()
        .std_picture_info(&std_picture)
        .reference_name_slot_indices(reference_name_slot_indices)
        .frame_header_offset(0)
        .tile_offsets(&picture.offsets)
        .tile_sizes(&picture.sizes);
    let mut decode = vk::VideoDecodeInfoKHR::default()
        .src_buffer(session.bitstream.buffer)
        .src_buffer_offset(0)
        .src_buffer_range(aligned_size)
        .dst_picture_resource(dst_picture)
        .setup_reference_slot(&setup_slot)
        .reference_slots(&reference_slots)
        .push_next(&mut codec_info);
    record_submit_and_readback(session, picture, &begin, &mut decode)
}

fn submit_h264(session: &mut Session, picture: &Picture) -> anyhow::Result<()> {
    if picture.bitstream.is_empty() || picture.bitstream.len() > MAX_ENCODED_BYTES {
        bail!(
            "invalid Vulkan decode bitstream length {}",
            picture.bitstream.len()
        );
    }
    let aligned_size = align_up(
        picture.bitstream.len() as u64,
        session.bitstream_size_alignment,
    );
    if aligned_size > session.bitstream.size {
        bail!("Vulkan bitstream exceeds allocated buffer");
    }
    unsafe {
        ptr::write_bytes(session.bitstream.mapped, 0, aligned_size as usize);
        ptr::copy_nonoverlapping(
            picture.bitstream.as_ptr(),
            session.bitstream.mapped,
            picture.bitstream.len(),
        );
    }

    record_h264_submission(session, picture, aligned_size)
}

fn record_h264_submission(
    session: &mut Session,
    picture: &Picture,
    aligned_size: u64,
) -> anyhow::Result<()> {
    let state = picture
        .h264
        .as_ref()
        .ok_or_else(|| anyhow!("H.264 state missing"))?;
    if state.references.len() > session.max_active_references {
        bail!(
            "H.264 picture requires {} active references, driver permits {}",
            state.references.len(),
            session.max_active_references
        );
    }
    let mut codec_info = vk::VideoDecodeH264PictureInfoKHR::default()
        .std_picture_info(&state.std_picture)
        .slice_offsets(&picture.offsets);

    let mut ref_codec: Vec<_> = state
        .references
        .iter()
        .map(|reference| &reference.std_reference)
        .map(|std| vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(std))
        .collect();
    let ref_pictures: Vec<_> = state
        .references
        .iter()
        .map(|reference| {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(vk::Extent2D {
                    width: session.coded_width,
                    height: session.coded_height,
                })
                .image_view_binding(reference.handle.dpb().view)
        })
        .collect();
    let ref_slots: Vec<_> = ref_pictures
        .iter()
        .zip(ref_codec.iter_mut())
        .zip(state.references.iter())
        .map(|((resource, codec), reference)| {
            vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(reference.handle.slot())
                .picture_resource(resource)
                .push_next(codec)
        })
        .collect();

    let mut setup_std = StdVideoDecodeH264ReferenceInfo {
        flags: unsafe { std::mem::zeroed() },
        FrameNum: state.std_picture.frame_num,
        reserved: 0,
        PicOrderCnt: state.std_picture.PicOrderCnt,
    };
    setup_std.flags.set_used_for_long_term_reference(0);
    let mut setup_codec =
        vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(&setup_std);
    let setup_picture = vk::VideoPictureResourceInfoKHR::default()
        .coded_extent(vk::Extent2D {
            width: session.coded_width,
            height: session.coded_height,
        })
        .image_view_binding(picture.dpb.view);
    let setup_slot = vk::VideoReferenceSlotInfoKHR::default()
        .slot_index(picture.slot.index)
        .picture_resource(&setup_picture)
        .push_next(&mut setup_codec);
    let dst_picture = vk::VideoPictureResourceInfoKHR::default()
        .coded_extent(vk::Extent2D {
            width: session.coded_width,
            height: session.coded_height,
        })
        .image_view_binding(picture.output.view);

    let begin = vk::VideoBeginCodingInfoKHR::default()
        .video_session(session.video_session)
        .video_session_parameters(session.session_parameters)
        .reference_slots(&ref_slots);
    let mut decode = vk::VideoDecodeInfoKHR::default()
        .src_buffer(session.bitstream.buffer)
        .src_buffer_offset(0)
        .src_buffer_range(aligned_size)
        .dst_picture_resource(dst_picture)
        .setup_reference_slot(&setup_slot)
        .reference_slots(&ref_slots)
        .push_next(&mut codec_info);
    record_submit_and_readback(session, picture, &begin, &mut decode)
}

fn record_submit_and_readback(
    session: &mut Session,
    picture: &Picture,
    begin: &vk::VideoBeginCodingInfoKHR<'_>,
    decode: &mut vk::VideoDecodeInfoKHR<'_>,
) -> anyhow::Result<()> {
    unsafe {
        session.core.device.reset_fences(&[session.fence])?;
        session
            .core
            .device
            .reset_command_buffer(session.command_buffer, vk::CommandBufferResetFlags::empty())?;
        session.core.device.begin_command_buffer(
            session.command_buffer,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
    }

    let full_range = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    let mut barriers = Vec::with_capacity(2);
    if !session.coincide {
        barriers.push(
            vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::VIDEO_DECODE_DPB_KHR)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(picture.dpb.image)
                .subresource_range(full_range)
                .dst_access_mask(vk::AccessFlags::MEMORY_WRITE),
        );
    }
    barriers.push(
        vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(if session.coincide {
                vk::ImageLayout::VIDEO_DECODE_DPB_KHR
            } else {
                vk::ImageLayout::VIDEO_DECODE_DST_KHR
            })
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(picture.output.image)
            .subresource_range(full_range)
            .dst_access_mask(vk::AccessFlags::MEMORY_WRITE),
    );
    unsafe {
        session.core.device.cmd_pipeline_barrier(
            session.command_buffer,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::BY_REGION,
            &[],
            &[],
            &barriers,
        );
        (session.core.video_device.fp().cmd_begin_video_coding_khr)(session.command_buffer, begin);
        if session.reset {
            let control = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::RESET);
            (session.core.video_device.fp().cmd_control_video_coding_khr)(
                session.command_buffer,
                &control,
            );
            session.reset = false;
        }
        (session.core.decode_device.fp().cmd_decode_video_khr)(session.command_buffer, decode);
        (session.core.video_device.fp().cmd_end_video_coding_khr)(
            session.command_buffer,
            &vk::VideoEndCodingInfoKHR::default(),
        );
    }

    let decoded_layout = if session.coincide {
        vk::ImageLayout::VIDEO_DECODE_DPB_KHR
    } else {
        vk::ImageLayout::VIDEO_DECODE_DST_KHR
    };
    let to_transfer = vk::ImageMemoryBarrier::default()
        .old_layout(decoded_layout)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(picture.output.image)
        .subresource_range(full_range)
        .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
        .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
    unsafe {
        session.core.device.cmd_pipeline_barrier(
            session.command_buffer,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::BY_REGION,
            &[],
            &[],
            &[to_transfer],
        );
    }
    let chroma_width = session.chroma.chroma_width(session.coded_width);
    let chroma_height = session.chroma.chroma_height(session.coded_height);
    let regions = [
        vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(session.coded_width)
            .buffer_image_height(session.coded_height)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::PLANE_0,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width: session.coded_width,
                height: session.coded_height,
                depth: 1,
            }),
        vk::BufferImageCopy::default()
            .buffer_offset(session.chroma_offset)
            .buffer_row_length(chroma_width)
            .buffer_image_height(chroma_height)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::PLANE_1,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width: chroma_width,
                height: chroma_height,
                depth: 1,
            }),
    ];
    unsafe {
        session.core.device.cmd_copy_image_to_buffer(
            session.command_buffer,
            picture.output.image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            session.staging.buffer,
            &regions,
        );
        let restore = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(decoded_layout)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(picture.output.image)
            .subresource_range(full_range)
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ);
        session.core.device.cmd_pipeline_barrier(
            session.command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::BY_REGION,
            &[],
            &[],
            &[restore],
        );
        session
            .core
            .device
            .end_command_buffer(session.command_buffer)?;
        let command_buffers = [session.command_buffer];
        let submits = [vk::SubmitInfo::default().command_buffers(&command_buffers)];
        session
            .core
            .device
            .queue_submit(session.core.queue, &submits, session.fence)?;
        let timeout = std::env::var("BLIT_DECODE_FENCE_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map_or(DEFAULT_FENCE_TIMEOUT_NS, |ms| {
                if ms == 0 {
                    u64::MAX
                } else {
                    ms.saturating_mul(1_000_000)
                }
            });
        session
            .core
            .device
            .wait_for_fences(&[session.fence], true, timeout)?;
    }
    Ok(())
}

fn finish_picture(session: &Session, picture: Picture) -> anyhow::Result<VkHandle> {
    let y_size = (session.coded_width as usize) * (session.coded_height as usize);
    let uv_size = (session.chroma.chroma_width(session.coded_width) as usize)
        * (session.chroma.chroma_height(session.coded_height) as usize)
        * 2;
    let y = unsafe { std::slice::from_raw_parts(session.staging.mapped, y_size) };
    let uv = unsafe {
        std::slice::from_raw_parts(
            session.staging.mapped.add(session.chroma_offset as usize),
            uv_size,
        )
    };
    let rgba = semiplanar_to_rgba(
        y,
        uv,
        session.coded_width as usize,
        session.crop_x as usize,
        session.crop_y as usize,
        session.display_width as usize,
        session.display_height as usize,
        session.chroma,
        session.full_range,
    );
    picture.frame.set_rgba(rgba)?;
    Ok(VkHandle(Rc::new(VkDecodedPicture {
        frame: picture.frame,
        dpb: picture.dpb,
        slot: picture.slot,
        timestamp: picture.timestamp,
        coded: (session.coded_width, session.coded_height).into(),
        display: (session.display_width, session.display_height).into(),
        av1_reference: picture
            .av1
            .as_ref()
            .map(|state| std_av1_reference(&state.header)),
    })))
}

fn semiplanar_to_rgba(
    y_plane: &[u8],
    uv_plane: &[u8],
    coded_width: usize,
    crop_x: usize,
    crop_y: usize,
    width: usize,
    height: usize,
    chroma: Chroma,
    full_range: bool,
) -> Vec<u8> {
    let mut rgba = vec![0; width * height * 4];
    let chroma_width = chroma.chroma_width(coded_width as u32) as usize;
    for y in 0..height {
        let source_y = crop_y + y;
        let cy = if chroma == Chroma::Cs420 {
            source_y / 2
        } else {
            source_y
        };
        for x in 0..width {
            let source_x = crop_x + x;
            let cx = if chroma == Chroma::Cs420 {
                source_x / 2
            } else {
                source_x
            };
            let yy = y_plane[source_y * coded_width + source_x];
            let uv_offset = (cy * chroma_width + cx) * 2;
            let (r, g, b) =
                yuv709_to_rgb(yy, uv_plane[uv_offset], uv_plane[uv_offset + 1], full_range);
            let out = (y * width + x) * 4;
            rgba[out..out + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    rgba
}

fn yuv709_to_rgb(y: u8, u: u8, v: u8, full_range: bool) -> (u8, u8, u8) {
    let y = i32::from(y);
    let u = i32::from(u) - 128;
    let v = i32::from(v) - 128;
    let (r, g, b) = if full_range {
        (
            y + ((403 * v + 128) >> 8),
            y - ((48 * u + 120 * v + 128) >> 8),
            y + ((475 * u + 128) >> 8),
        )
    } else {
        let y = (y - 16).max(0);
        (
            (298 * y + 459 * v + 128) >> 8,
            (298 * y - 55 * u - 136 * v + 128) >> 8,
            (298 * y + 541 * u + 128) >> 8,
        )
    };
    (
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
    )
}

type H264Decoder = StatelessDecoder<H264, VulkanBackend>;
type Av1Decoder = StatelessDecoder<Av1, VulkanBackend>;

enum Inner {
    H264(Box<H264Decoder>),
    Av1(Box<Av1Decoder>),
}

pub(crate) struct Decoder {
    inner: Inner,
    width: u32,
    height: u32,
    timestamp: u64,
}

impl Decoder {
    pub(crate) fn new(
        codec: Codec,
        chroma: Chroma,
        width: u16,
        height: u16,
    ) -> Result<Self, Error> {
        let width = u32::from(width);
        let height = u32::from(height);
        let backend = VulkanBackend::new(codec, chroma, width, height)?;
        let inner = match codec {
            Codec::H264 => Inner::H264(Box::new(
                StatelessDecoder::new(backend, BlockingMode::Blocking)
                    .map_err(|e| Error::Resource(format!("create H.264 state machine: {e}")))?,
            )),
            Codec::Av1 => Inner::Av1(Box::new(
                StatelessDecoder::new(backend, BlockingMode::Blocking)
                    .map_err(|e| Error::Resource(format!("create AV1 state machine: {e}")))?,
            )),
        };
        Ok(Self {
            inner,
            width,
            height,
            timestamp: 0,
        })
    }

    pub(crate) fn decode(&mut self, encoded: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        if encoded.is_empty() || encoded.len() > MAX_ENCODED_BYTES {
            return Err(Error::Invalid(format!(
                "encoded packet length {}",
                encoded.len()
            )));
        }
        self.timestamp = self.timestamp.wrapping_add(1);
        let width = self.width;
        let height = self.height;
        let mut allocate = || Some(CpuFrame::new(width, height));
        let result = match &mut self.inner {
            Inner::H264(decoder) => {
                decode_h264_all(decoder.as_mut(), self.timestamp, encoded, &mut allocate)
            }
            Inner::Av1(decoder) => {
                decode_all(decoder.as_mut(), self.timestamp, encoded, &mut allocate)
            }
        };
        result.map_err(|e| Error::Invalid(e.to_string()))
    }

    pub(crate) fn flush(&mut self) {
        match &mut self.inner {
            Inner::H264(decoder) => {
                let _ = decoder.flush();
            }
            Inner::Av1(decoder) => {
                let _ = decoder.flush();
            }
        }
    }
}

fn decode_h264_all(
    decoder: &mut H264Decoder,
    timestamp: u64,
    encoded: &[u8],
    allocate: &mut dyn FnMut() -> Option<CpuFrame>,
) -> anyhow::Result<Option<Vec<u8>>> {
    decode_input(decoder, timestamp, encoded, allocate)?;
    decoder.end_access_unit()?;
    take_output(decoder)
}

fn decode_all<D>(
    decoder: &mut D,
    timestamp: u64,
    encoded: &[u8],
    allocate: &mut dyn FnMut() -> Option<CpuFrame>,
) -> anyhow::Result<Option<Vec<u8>>>
where
    D: StatelessVideoDecoder<Handle = VkHandle>,
{
    decode_input(decoder, timestamp, encoded, allocate)?;
    take_output(decoder)
}

fn decode_input<D>(
    decoder: &mut D,
    timestamp: u64,
    encoded: &[u8],
    allocate: &mut dyn FnMut() -> Option<CpuFrame>,
) -> anyhow::Result<()>
where
    D: StatelessVideoDecoder<Handle = VkHandle>,
{
    let mut offset = 0;
    while offset < encoded.len() {
        match decoder.decode(timestamp, &encoded[offset..], allocate) {
            Ok(0) => bail!("codec parser made no progress"),
            Ok(consumed) => offset += consumed,
            Err(cros_codecs::decoder::stateless::DecodeError::CheckEvents) => {
                drain_format_events(decoder)?;
            }
            Err(error) => return Err(anyhow!(error)),
        }
    }
    Ok(())
}

fn take_output<D>(decoder: &mut D) -> anyhow::Result<Option<Vec<u8>>>
where
    D: StatelessVideoDecoder<Handle = VkHandle>,
{
    let mut output = None;
    while let Some(event) = decoder.next_event() {
        match event {
            DecoderEvent::FormatChanged => {}
            DecoderEvent::FrameReady(handle) => {
                let rgba = handle.rgba()?;
                if output.replace(rgba).is_some() {
                    bail!("one camera packet produced multiple decoded frames");
                }
            }
        }
    }
    Ok(output)
}

fn drain_format_events<D>(decoder: &mut D) -> anyhow::Result<()>
where
    D: StatelessVideoDecoder<Handle = VkHandle>,
{
    let mut changed = false;
    while let Some(event) = decoder.next_event() {
        match event {
            DecoderEvent::FormatChanged => changed = true,
            DecoderEvent::FrameReady(_) => bail!("frame arrived while processing format change"),
        }
    }
    if changed {
        Ok(())
    } else {
        bail!("decoder requested event processing without an event")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_chroma_formats() {
        assert!(Chroma::Cs420.format() == vk::Format::G8_B8R8_2PLANE_420_UNORM);
        assert!(Chroma::Cs444.format() == vk::Format::G8_B8R8_2PLANE_444_UNORM);
    }

    #[test]
    fn semiplanar_black_and_white() {
        let y = [16, 235];
        let uv = [128, 128, 128, 128];
        let rgba = semiplanar_to_rgba(&y, &uv, 2, 0, 0, 2, 1, Chroma::Cs444, false);
        assert_eq!(&rgba[..4], &[0, 0, 0, 255]);
        assert_eq!(&rgba[4..], &[255, 255, 255, 255]);
    }

    #[test]
    fn dimension_limits_are_exact() {
        assert!(validate_dimensions(Chroma::Cs420, 256, 256).is_ok());
        assert!(validate_dimensions(Chroma::Cs420, 255, 256).is_err());
        assert!(validate_dimensions(Chroma::Cs444, 255, 255).is_ok());
        assert!(validate_dimensions(Chroma::Cs444, 0, 1).is_err());
    }
}
