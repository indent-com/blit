//! Vulkan-based GPU compositor renderer.
//!
//! Replaces the EGL/GLES2 renderer for compositing Wayland client surfaces
//! into a single output image.  Uses `ash` with the `loaded` feature to
//! dlopen libvulkan.so at runtime.
//!
//! Key advantages over the GL path:
//! - Explicit pixel format control (`VK_FORMAT_B8G8R8A8_UNORM`)
//! - Top-down framebuffer (no Y-flip needed)
//! - DMA-BUF import/export with explicit modifiers
//! - Proper synchronization via Vulkan fences

#![allow(non_upper_case_globals, clippy::too_many_arguments)]

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use ash::vk;
use wayland_server::backend::ObjectId;

use super::imp::{ExternalOutputBuffer, PixelData, Surface};
use super::render::{GpuLayer, SurfaceMeta, collect_gpu_layers, to_physical};

// ===================================================================
// VulkanRenderer
// ===================================================================

pub(crate) struct VulkanRenderer {
    #[expect(dead_code)]
    entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    physical_device: vk::PhysicalDevice,
    queue: vk::Queue,
    #[expect(dead_code)]
    queue_family: u32,
    command_pool: vk::CommandPool,

    // Vulkan Video encode support (optional).
    video_encode_queue: Option<vk::Queue>,
    video_encode_queue_family: Option<u32>,
    video_encode_command_pool: Option<vk::CommandPool>,
    video_fns: Option<crate::vulkan_encode::VideoFns>,
    /// Per-surface Vulkan Video H.264 encoders.
    vulkan_encoders: HashMap<u32, crate::vulkan_encode::VulkanVideoEncoder>,
    /// Whether the device supports VK_KHR_video_encode_queue + H.264 extensions.
    has_video_encode: bool,
    /// Whether the device supports VK_KHR_video_encode_av1 extension.
    has_video_encode_av1: bool,
    /// Whether the device supports DMA-BUF import/export extensions.
    has_dmabuf: bool,

    // Render pipeline
    render_pass: vk::RenderPass,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    sampler: vk::Sampler,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,

    // BGRA→NV12 compute pipeline — buffer path (linear NV12)
    compute_pipeline: vk::Pipeline,
    compute_pipeline_layout: vk::PipelineLayout,
    compute_descriptor_set_layout: vk::DescriptorSetLayout,

    // BGRA→NV12 compute pipeline — image path (tiled NV12)
    compute_image_pipeline: vk::Pipeline,
    compute_image_pipeline_layout: vk::PipelineLayout,
    compute_image_descriptor_set_layout: vk::DescriptorSetLayout,

    // Output images (triple-buffered)
    output_images: Vec<OutputImage>,
    output_idx: usize,

    // Per-frame temporary textures (SHM uploads) — freed at start of next frame.
    frame_textures: Vec<TempTexture>,

    // In-flight GPU submission — tracked so we can retire its resources
    // once the fence signals.
    pending_submit: Option<PendingSubmit>,

    /// Submissions for external outputs whose fences we don't need to
    /// block on (VPP handles sync via implicit DMA-BUF fencing).  We
    /// only keep them alive so we can free the Vulkan command buffer,
    /// fence, and per-frame textures once the GPU is done.
    deferred_submits: Vec<PendingSubmit>,

    /// VK_KHR_external_fence_fd function loader — used to export Vulkan
    /// fences as sync_fd for cross-process / cross-API synchronisation.
    external_fence_fd_fn: Option<ash::khr::external_fence_fd::Device>,

    /// Supported DRM format modifiers queried from the Vulkan device.
    pub(crate) supported_dmabuf_modifiers: Vec<(u32, u64)>,

    /// Encoder-allocated output buffers imported as Vulkan render targets,
    /// keyed by surface_id.  Each surface has its own pool so different
    /// surfaces never share buffers.
    external_outputs: HashMap<u32, (Vec<ExternalOutput>, usize)>,

    /// NV12 output buffers for zero-copy BGRA→NV12 compute conversion,
    /// keyed by surface_id.  The `usize` is the round-robin index.
    nv12_outputs: HashMap<u32, (Vec<Nv12Output>, usize)>,

    /// Persistent texture cache keyed by Wayland surface ObjectId.
    /// Textures are created at surface commit time and reused across
    /// frames until the surface commits a new buffer or is destroyed.
    surface_textures: HashMap<ObjectId, CachedSurfaceTexture>,

    /// Textures replaced by a surface commit but still potentially
    /// referenced by in-flight GPU work.  Freed when the pending
    /// submission completes (retire_pending / free_frame_textures).
    pending_destroy_textures: Vec<CachedSurfaceTexture>,
}

/// Encoder-allocated DMA-BUF imported as a Vulkan framebuffer.
struct ExternalOutput {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    width: u32,
    height: u32,
    va_surface_id: u32,
    va_display: usize,
    fourcc: u32,
    modifier: u64,
    stride: u32,
    /// Keep the DMA-BUF fd alive.
    _fd: Arc<OwnedFd>,
}

/// NV12 output for zero-copy encode.
struct Nv12Output {
    fd: Arc<OwnedFd>,
    descriptor_set: vk::DescriptorSet,
    /// NV12 surface dimensions (encoder-padded, may be larger than source).
    width: u32,
    height: u32,
    kind: Nv12OutputKind,
}

enum Nv12OutputKind {
    /// Linear NV12 in a single VkBuffer (Intel/linear path).
    Buffer {
        buffer: vk::Buffer,
        memory: vk::DeviceMemory,
        buf_size: u64,
        stride: u32,
        uv_offset: u32,
    },
    /// Tiled NV12 as a multi-plane VkImage (AMD/tiled path).
    /// Single G8_B8R8_2PLANE_420_UNORM image with per-plane views.
    Image {
        image: vk::Image,
        y_memory: vk::DeviceMemory,
        y_view: vk::ImageView,
        uv_memory: vk::DeviceMemory,
        uv_view: vk::ImageView,
        /// Full-image COLOR view for Vulkan Video encode source.
        encode_view: Option<vk::ImageView>,
    },
}

struct TempTexture {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    descriptor_set: vk::DescriptorSet,
}

/// Persistent GPU texture for a Wayland surface, cached between frames.
/// Created at surface commit time, reused until the surface commits a
/// new buffer or is destroyed.
struct CachedSurfaceTexture {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    descriptor_set: vk::DescriptorSet,
    /// Vulkan image layout — SHM textures start at PREINITIALIZED,
    /// DMA-BUF imports start at UNDEFINED.
    initial_layout: vk::ImageLayout,
}

/// In-flight GPU submission.  Resources are kept alive until the fence
/// signals so the GPU doesn't access freed memory.
struct PendingSubmit {
    fence: vk::Fence,
    cb: vk::CommandBuffer,
    textures: Vec<TempTexture>,
    output_idx: usize,
    phys_w: u32,
    phys_h: u32,
    /// True when the render targeted an encoder-allocated external buffer.
    external: bool,
    /// Toplevel surface_id this submission was rendered for, so async
    /// retirement can attribute the pixels to the correct surface.
    toplevel_sid: u16,
    /// Surface id used to look up per-surface external/NV12 output pools.
    surface_id: u32,
}

unsafe impl Send for VulkanRenderer {}

struct OutputImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    width: u32,
    height: u32,

    /// Staging buffer for CPU readback (fallback when DMA-BUF export unavailable).
    staging_buf: vk::Buffer,
    staging_mem: vk::DeviceMemory,
    staging_ptr: *mut u8,
}

// Inline SPIR-V for vertex and fragment shaders.
// Vertex: transforms unit quad via push constants (x, y, w, h in clip space).
// Fragment: samples a combined image sampler.

// Equivalent GLSL (vertex):
//   #version 450
//   layout(push_constant) uniform PC { vec4 geom; };
//   layout(location=0) out vec2 v_tc;
//   void main() {
//       vec2 pos = vec2(gl_VertexIndex & 1, (gl_VertexIndex >> 1) & 1);
//       gl_Position = vec4(geom.xy + pos * geom.zw, 0.0, 1.0);
//       v_tc = pos;
//   }
static VERT_SPV: &[u8] = include_bytes!("shaders/composite.vert.spv");

// Equivalent GLSL (fragment):
//   #version 450
//   layout(location=0) in vec2 v_tc;
//   layout(set=0, binding=0) uniform sampler2D tex;
//   layout(location=0) out vec4 color;
//   void main() { color = texture(tex, v_tc); }
static FRAG_SPV: &[u8] = include_bytes!("shaders/composite.frag.spv");

static NV12_COMP_SPV: &[u8] = include_bytes!("shaders/bgra_to_nv12.comp.spv");

static NV12_IMAGE_COMP_SPV: &[u8] = include_bytes!("shaders/bgra_to_nv12_image.comp.spv");

/// Convert a DRM fourcc to a VkFormat.  Returns None for unsupported formats.
fn drm_fourcc_to_vk_format(fourcc: u32) -> Option<vk::Format> {
    match fourcc {
        // ARGB8888 = B8G8R8A8 in Vulkan byte order
        0x34325241 => Some(vk::Format::B8G8R8A8_UNORM),
        // XRGB8888 = B8G8R8A8 (alpha ignored)
        0x34325258 => Some(vk::Format::B8G8R8A8_UNORM),
        // ABGR8888 = R8G8B8A8
        0x34324241 => Some(vk::Format::R8G8B8A8_UNORM),
        // XBGR8888
        0x34324258 => Some(vk::Format::R8G8B8A8_UNORM),
        _ => None,
    }
}

impl VulkanRenderer {
    pub(crate) fn try_new(drm_device: &str) -> Option<Self> {
        // Load Vulkan at runtime via dlopen.
        let entry = match unsafe { ash::Entry::load() } {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[vulkan-render] failed to load libvulkan: {e}");
                return None;
            }
        };

        // Create instance with external memory extensions.
        let app_info = vk::ApplicationInfo::default()
            .application_name(c"blit-compositor")
            .application_version(1)
            .api_version(vk::make_api_version(0, 1, 3, 0));

        let instance_extensions = [
            ash::khr::external_memory_capabilities::NAME.as_ptr(),
            ash::khr::get_physical_device_properties2::NAME.as_ptr(),
        ];

        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&instance_extensions);

        let instance = match unsafe { entry.create_instance(&create_info, None) } {
            Ok(i) => i,
            Err(e) => {
                eprintln!("[vulkan-render] vkCreateInstance failed: {e}");
                return None;
            }
        };

        // Find the physical device matching the DRM render node.
        let phys_devices = unsafe { instance.enumerate_physical_devices().ok()? };
        let (physical_device, queue_family, video_encode_queue_family) =
            Self::find_device(&instance, &phys_devices, drm_device)?;

        // Probe device extensions for video encode support.
        let ext_props_all = unsafe {
            instance
                .enumerate_device_extension_properties(physical_device)
                .unwrap_or_default()
        };
        let ext_names_all: Vec<&std::ffi::CStr> = ext_props_all
            .iter()
            .map(|p| unsafe { std::ffi::CStr::from_ptr(p.extension_name.as_ptr()) })
            .collect();

        let has_video_encode = {
            let has_video_queue = ext_names_all.contains(&c"VK_KHR_video_queue");
            let has_video_encode_queue = ext_names_all.contains(&c"VK_KHR_video_encode_queue");
            let has_video_encode_h264 = ext_names_all.contains(&c"VK_KHR_video_encode_h264");
            let ok = has_video_queue
                && has_video_encode_queue
                && has_video_encode_h264
                && video_encode_queue_family.is_some();
            if ok {
                eprintln!("[vulkan-render] Vulkan Video encode extensions available");
            } else {
                eprintln!(
                    "[vulkan-render] Vulkan Video encode not available (queue={} enc_queue={} h264={} enc_qf={:?})",
                    has_video_queue,
                    has_video_encode_queue,
                    has_video_encode_h264,
                    video_encode_queue_family,
                );
            }
            ok
        };

        let has_video_encode_av1 =
            has_video_encode && ext_names_all.contains(&c"VK_KHR_video_encode_av1");
        if has_video_encode_av1 {
            eprintln!("[vulkan-render] Vulkan Video AV1 encode extension available");
        }

        // Probe for external fence fd support (needed for sync_fd export).
        let has_external_fence_fd = ext_names_all.contains(&ash::khr::external_fence_fd::NAME)
            && ext_names_all.contains(&ash::khr::external_fence::NAME);

        // DMA-BUF extensions are optional — llvmpipe and other software
        // renderers lack them.  When absent the compositor runs in SHM-only
        // mode: clients use wl_shm, and any DMA-BUF buffers that arrive
        // are imported via the mmap fallback path.
        let dmabuf_extensions: &[&std::ffi::CStr] = &[
            ash::khr::external_memory_fd::NAME,
            ash::khr::external_memory::NAME,
            ash::ext::external_memory_dma_buf::NAME,
            ash::ext::image_drm_format_modifier::NAME,
            ash::khr::image_format_list::NAME,
        ];
        let has_dmabuf = dmabuf_extensions.iter().all(|e| ext_names_all.contains(e));
        if !has_dmabuf {
            eprintln!("[vulkan-render] DMA-BUF extensions not available, SHM-only mode");
        }
        let mut device_extensions: Vec<*const std::ffi::c_char> = Vec::new();
        if has_dmabuf {
            device_extensions.extend(dmabuf_extensions.iter().map(|e| e.as_ptr()));
        }
        if has_external_fence_fd {
            device_extensions.push(ash::khr::external_fence::NAME.as_ptr());
            device_extensions.push(ash::khr::external_fence_fd::NAME.as_ptr());
        }
        if has_video_encode {
            device_extensions.push(c"VK_KHR_video_queue".as_ptr());
            device_extensions.push(c"VK_KHR_video_encode_queue".as_ptr());
            device_extensions.push(c"VK_KHR_video_encode_h264".as_ptr());
        }
        if has_video_encode_av1 {
            device_extensions.push(c"VK_KHR_video_encode_av1".as_ptr());
        }

        let queue_priorities = [1.0f32];
        let mut queue_creates: Vec<vk::DeviceQueueCreateInfo> = vec![
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue_family)
                .queue_priorities(&queue_priorities),
        ];
        let video_encode_qf = if has_video_encode {
            video_encode_queue_family
        } else {
            None
        };
        if let Some(enc_qf) = video_encode_qf
            && enc_qf != queue_family
        {
            queue_creates.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(enc_qf)
                    .queue_priorities(&queue_priorities),
            );
        }

        let device_create = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_creates)
            .enabled_extension_names(&device_extensions);

        let device = match unsafe { instance.create_device(physical_device, &device_create, None) }
        {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[vulkan-render] vkCreateDevice failed: {e}");
                unsafe { instance.destroy_instance(None) };
                return None;
            }
        };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };

        let external_fence_fd_fn = if has_external_fence_fd {
            Some(ash::khr::external_fence_fd::Device::new(&instance, &device))
        } else {
            None
        };

        // Video encode queue and command pool.
        let (video_encode_queue, video_encode_command_pool, video_fns) = if let Some(enc_qf) =
            video_encode_qf
        {
            let enc_queue = if enc_qf == queue_family {
                // Same family — use queue index 0 (shared).
                queue
            } else {
                unsafe { device.get_device_queue(enc_qf, 0) }
            };
            let pool_info = vk::CommandPoolCreateInfo::default()
                .queue_family_index(enc_qf)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
            let enc_pool = unsafe { device.create_command_pool(&pool_info, None).ok() };
            let vfns = unsafe { crate::vulkan_encode::VideoFns::load(&entry, &instance, &device) };
            if enc_pool.is_some() && vfns.is_some() {
                eprintln!("[vulkan-render] video encode queue family={enc_qf}, pool + fns loaded",);
            }
            (Some(enc_queue), enc_pool, vfns)
        } else {
            (None, None, None)
        };
        // Command pool.
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = unsafe { device.create_command_pool(&pool_info, None).ok()? };

        // Sampler for texture sampling.
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_info, None).ok()? };

        // Descriptor set layout: one combined image sampler at binding 0.
        let binding = vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .immutable_samplers(std::slice::from_ref(&sampler));
        let ds_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(std::slice::from_ref(&binding));
        let descriptor_set_layout = unsafe {
            device
                .create_descriptor_set_layout(&ds_layout_info, None)
                .ok()?
        };

        // Descriptor pool (pre-allocate for texture cache + compute NV12 outputs).
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(256),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(48),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(16),
        ];
        let dp_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(256)
            .pool_sizes(&pool_sizes)
            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET);
        let descriptor_pool = unsafe { device.create_descriptor_pool(&dp_info, None).ok()? };

        // Push constant range for geometry (x, y, w, h).
        let push_range = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .offset(0)
            .size(16); // 4 floats

        let pl_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(std::slice::from_ref(&descriptor_set_layout))
            .push_constant_ranges(std::slice::from_ref(&push_range));
        let pipeline_layout = unsafe { device.create_pipeline_layout(&pl_info, None).ok()? };

        // Render pass: single color attachment, B8G8R8A8_UNORM.
        let attachment = vk::AttachmentDescription::default()
            .format(vk::Format::B8G8R8A8_UNORM)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
        let color_ref = vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        let subpass = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(std::slice::from_ref(&color_ref));
        let rp_info = vk::RenderPassCreateInfo::default()
            .attachments(std::slice::from_ref(&attachment))
            .subpasses(std::slice::from_ref(&subpass));
        let render_pass = unsafe { device.create_render_pass(&rp_info, None).ok()? };

        // Shader modules.
        let vert_code = Self::spirv_from_bytes(VERT_SPV)?;
        let frag_code = Self::spirv_from_bytes(FRAG_SPV)?;
        let vert_info = vk::ShaderModuleCreateInfo::default().code(&vert_code);
        let frag_info = vk::ShaderModuleCreateInfo::default().code(&frag_code);
        let vert_mod = unsafe { device.create_shader_module(&vert_info, None).ok()? };
        let frag_mod = unsafe { device.create_shader_module(&frag_info, None).ok()? };

        let entry_name = c"main";
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vert_mod)
                .name(entry_name),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(frag_mod)
                .name(entry_name),
        ];

        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);

        // Dynamic viewport/scissor.
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_info =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);

        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);

        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);

        // Pre-multiplied alpha blending.
        let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA);

        let blend_info = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(std::slice::from_ref(&blend_attachment));

        let pipeline_info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend_info)
            .dynamic_state(&dynamic_info)
            .layout(pipeline_layout)
            .render_pass(render_pass)
            .subpass(0);

        let pipeline = unsafe {
            device
                .create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
                .ok()?[0]
        };

        // Clean up shader modules (not needed after pipeline creation).
        unsafe {
            device.destroy_shader_module(vert_mod, None);
            device.destroy_shader_module(frag_mod, None);
        }

        // -----------------------------------------------------------
        // BGRA→NV12 compute pipeline
        // -----------------------------------------------------------
        // Descriptor set layout: 3 storage images.
        //   binding 0 = BGRA input  (rgba8)
        //   binding 1 = Y output    (r8)
        //   binding 1 = NV12 output  (storage buffer)
        let compute_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let compute_ds_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&compute_bindings);
        let compute_descriptor_set_layout = unsafe {
            device
                .create_descriptor_set_layout(&compute_ds_layout_info, None)
                .ok()?
        };

        // Push constants: src_width, src_height, y_stride, uv_offset, enc_width, enc_height (6 × u32 = 24 bytes).
        let compute_push_range = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(24);
        let compute_pl_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(std::slice::from_ref(&compute_descriptor_set_layout))
            .push_constant_ranges(std::slice::from_ref(&compute_push_range));
        let compute_pipeline_layout =
            unsafe { device.create_pipeline_layout(&compute_pl_info, None).ok()? };

        // Load compute shader and create pipeline.
        let comp_code = Self::spirv_from_bytes(NV12_COMP_SPV)?;
        let comp_shader_info = vk::ShaderModuleCreateInfo::default().code(&comp_code);
        let comp_mod = unsafe { device.create_shader_module(&comp_shader_info, None).ok()? };
        let comp_entry_name = c"main";
        let comp_stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(comp_mod)
            .name(comp_entry_name);
        let compute_pipeline_info = vk::ComputePipelineCreateInfo::default()
            .stage(comp_stage)
            .layout(compute_pipeline_layout);
        let compute_pipeline = unsafe {
            device
                .create_compute_pipelines(vk::PipelineCache::null(), &[compute_pipeline_info], None)
                .ok()?[0]
        };
        unsafe {
            device.destroy_shader_module(comp_mod, None);
        }

        // -----------------------------------------------------------
        // BGRA→NV12 compute pipeline — image path (tiled NV12)
        // -----------------------------------------------------------
        // Descriptor set layout: 3 storage images.
        //   binding 0 = BGRA input  (rgba8, storage image)
        //   binding 1 = Y output    (r8, storage image)
        //   binding 2 = UV output   (rg8, storage image)
        let compute_image_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let compute_image_ds_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&compute_image_bindings);
        let compute_image_descriptor_set_layout = unsafe {
            device
                .create_descriptor_set_layout(&compute_image_ds_layout_info, None)
                .ok()?
        };

        // Push constants: src_width, src_height, enc_width, enc_height (4 × u32 = 16 bytes).
        let compute_image_push_range = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(16);
        let compute_image_pl_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(std::slice::from_ref(&compute_image_descriptor_set_layout))
            .push_constant_ranges(std::slice::from_ref(&compute_image_push_range));
        let compute_image_pipeline_layout = unsafe {
            device
                .create_pipeline_layout(&compute_image_pl_info, None)
                .ok()?
        };

        let comp_image_code = Self::spirv_from_bytes(NV12_IMAGE_COMP_SPV)?;
        let comp_image_shader_info = vk::ShaderModuleCreateInfo::default().code(&comp_image_code);
        let comp_image_mod = unsafe {
            device
                .create_shader_module(&comp_image_shader_info, None)
                .ok()?
        };
        let comp_image_stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(comp_image_mod)
            .name(c"main");
        let compute_image_pipeline_info = vk::ComputePipelineCreateInfo::default()
            .stage(comp_image_stage)
            .layout(compute_image_pipeline_layout);
        let compute_image_pipeline = unsafe {
            device
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[compute_image_pipeline_info],
                    None,
                )
                .ok()?[0]
        };
        unsafe {
            device.destroy_shader_module(comp_image_mod, None);
        }

        eprintln!("[vulkan-render] initialized on {drm_device}");

        // Query supported DRM format modifiers for each format we accept.
        // Clients (Chromium, mpv, …) will pick from these when allocating
        // DMA-BUFs, ensuring the GPU can import them with the correct
        // tiling layout.
        // Skip the query entirely when DMA-BUF extensions are absent —
        // DrmFormatModifierPropertiesListEXT requires the extension.
        let supported_dmabuf_modifiers = if has_dmabuf {
            use super::imp::drm_fourcc;
            let format_pairs: &[(u32, vk::Format)] = &[
                (drm_fourcc::ARGB8888, vk::Format::B8G8R8A8_UNORM),
                (drm_fourcc::XRGB8888, vk::Format::B8G8R8A8_UNORM),
                (drm_fourcc::ABGR8888, vk::Format::R8G8B8A8_UNORM),
                (drm_fourcc::XBGR8888, vk::Format::R8G8B8A8_UNORM),
            ];
            let mut mods = Vec::new();
            for &(drm_fmt, vk_fmt) in format_pairs {
                // First pass: get count.
                let mut mod_list = vk::DrmFormatModifierPropertiesListEXT::default();
                let mut fp2 = vk::FormatProperties2::default().push_next(&mut mod_list);
                unsafe {
                    instance.get_physical_device_format_properties2(
                        physical_device,
                        vk_fmt,
                        &mut fp2,
                    );
                }
                let count = mod_list.drm_format_modifier_count as usize;
                if count == 0 {
                    // No modifier support — fall back to LINEAR.
                    mods.push((drm_fmt, 0u64));
                    continue;
                }
                // Second pass: read properties.
                let mut props = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
                mod_list.drm_format_modifier_count = count as u32;
                mod_list.p_drm_format_modifier_properties = props.as_mut_ptr();
                let mut fp2 = vk::FormatProperties2::default().push_next(&mut mod_list);
                unsafe {
                    instance.get_physical_device_format_properties2(
                        physical_device,
                        vk_fmt,
                        &mut fp2,
                    );
                }
                let mut has_linear = false;
                for p in &props {
                    // Only advertise single-plane modifiers that support
                    // sampling (we need to texture from the imported image).
                    if p.drm_format_modifier_plane_count == 1
                        && p.drm_format_modifier_tiling_features
                            .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
                    {
                        mods.push((drm_fmt, p.drm_format_modifier));
                        if p.drm_format_modifier == 0 {
                            has_linear = true;
                        }
                    }
                }
                // Always include LINEAR so clients that can't use
                // vendor-specific tiled modifiers have a fallback.
                if !has_linear {
                    mods.push((drm_fmt, 0u64));
                }
            }
            eprintln!(
                "[vulkan-render] {} supported DMA-BUF format/modifier pairs",
                mods.len(),
            );
            mods
        } else {
            Vec::new()
        };

        Some(Self {
            entry,
            instance,
            device,
            physical_device,
            queue,
            queue_family,
            command_pool,
            video_encode_queue,
            video_encode_queue_family: video_encode_qf,
            video_encode_command_pool,
            video_fns,
            vulkan_encoders: HashMap::new(),
            has_video_encode,
            has_video_encode_av1,
            has_dmabuf,
            render_pass,
            pipeline_layout,
            pipeline,
            sampler,
            descriptor_set_layout,
            descriptor_pool,
            compute_pipeline,
            compute_pipeline_layout,
            compute_descriptor_set_layout,
            compute_image_pipeline,
            compute_image_pipeline_layout,
            compute_image_descriptor_set_layout,
            output_images: Vec::new(),
            output_idx: 0,
            frame_textures: Vec::new(),
            pending_submit: None,
            deferred_submits: Vec::new(),
            external_fence_fd_fn,
            supported_dmabuf_modifiers,
            external_outputs: HashMap::new(),
            nv12_outputs: HashMap::new(),
            surface_textures: HashMap::new(),
            pending_destroy_textures: Vec::new(),
        })
    }

    fn find_device(
        instance: &ash::Instance,
        devices: &[vk::PhysicalDevice],
        _drm_device: &str,
    ) -> Option<(vk::PhysicalDevice, u32, Option<u32>)> {
        // For now, pick the first device with a graphics queue.
        // TODO: match against the DRM render node.
        for &pd in devices {
            let props = unsafe { instance.get_physical_device_queue_family_properties(pd) };
            let mut graphics_qf = None;
            let mut video_encode_qf = None;
            for (i, qf) in props.iter().enumerate() {
                if qf.queue_flags.contains(vk::QueueFlags::GRAPHICS) && graphics_qf.is_none() {
                    graphics_qf = Some(i as u32);
                }
                // VIDEO_ENCODE_KHR = 0x40
                if qf.queue_flags.contains(vk::QueueFlags::from_raw(0x40))
                    && video_encode_qf.is_none()
                {
                    video_encode_qf = Some(i as u32);
                }
            }
            if let Some(gqf) = graphics_qf {
                return Some((pd, gqf, video_encode_qf));
            }
        }
        None
    }

    fn spirv_from_bytes(bytes: &[u8]) -> Option<Vec<u32>> {
        if !bytes.len().is_multiple_of(4) {
            return None;
        }
        let code: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        Some(code)
    }

    fn find_memory_type(&self, type_bits: u32, properties: vk::MemoryPropertyFlags) -> Option<u32> {
        let mem_props = unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical_device)
        };
        (0..mem_props.memory_type_count).find(|&i| {
            (type_bits & (1 << i)) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(properties)
        })
    }

    // ---------------------------------------------------------------
    // Vulkan Video capability queries
    // ---------------------------------------------------------------

    /// Whether the device supports Vulkan Video H.264 encode.
    pub(crate) fn has_video_encode(&self) -> bool {
        self.has_video_encode
    }

    /// Whether the device supports Vulkan Video AV1 encode.
    pub(crate) fn has_video_encode_av1(&self) -> bool {
        self.has_video_encode_av1
    }

    /// Whether the device supports DMA-BUF import/export extensions.
    pub(crate) fn has_dmabuf(&self) -> bool {
        self.has_dmabuf
    }

    /// Number of cached surface textures (diagnostic).
    pub(crate) fn surface_texture_count(&self) -> usize {
        self.surface_textures.len()
    }

    // ---------------------------------------------------------------
    // Vulkan Video encoder management
    // ---------------------------------------------------------------

    /// Create a Vulkan Video encoder for the given surface.
    /// `codec`: 0x01 = H.264, 0x02 = AV1.
    pub(crate) fn create_vulkan_encoder(
        &mut self,
        surface_id: u32,
        codec: u8,
        qp: u8,
        w: u32,
        h: u32,
    ) {
        if !self.has_video_encode {
            eprintln!("[vulkan-render] cannot create vulkan encoder: video encode not available");
            return;
        }
        let enc_qf = match self.video_encode_queue_family {
            Some(qf) => qf,
            None => return,
        };

        // Remove existing encoder if any.
        if let Some(mut old) = self.vulkan_encoders.remove(&surface_id)
            && let Some(ref vfns) = self.video_fns
        {
            unsafe { old.destroy(&self.device, vfns) };
        }

        let codec_name = match codec {
            0x02 => "av1",
            _ => "h264",
        };

        let encoder = match codec {
            0x02 if self.has_video_encode_av1 => unsafe {
                crate::vulkan_encode::VulkanVideoEncoder::try_new_av1(
                    &self.device,
                    &self.instance,
                    self.physical_device,
                    self.video_fns.as_ref().unwrap(),
                    enc_qf,
                    w,
                    h,
                    qp,
                )
            },
            0x02 => {
                eprintln!(
                    "[vulkan-render] AV1 encode not available, cannot create encoder for surface {surface_id}",
                );
                return;
            }
            _ => unsafe {
                crate::vulkan_encode::VulkanVideoEncoder::try_new_h264(
                    &self.device,
                    &self.instance,
                    self.physical_device,
                    self.video_fns.as_ref().unwrap(),
                    enc_qf,
                    w,
                    h,
                    qp,
                )
            },
        };
        match encoder {
            Some(enc) => {
                eprintln!(
                    "[vulkan-render] created vulkan {codec_name} encoder for surface {surface_id} {w}x{h} qp={qp}",
                );
                self.vulkan_encoders.insert(surface_id, enc);
            }
            None => {
                eprintln!(
                    "[vulkan-render] failed to create vulkan {codec_name} encoder for surface {surface_id}",
                );
            }
        }
    }

    /// Request the next frame for this surface's encoder to be a keyframe.
    pub(crate) fn request_encoder_keyframe(&mut self, surface_id: u32) {
        if let Some(enc) = self.vulkan_encoders.get_mut(&surface_id) {
            enc.request_idr();
        }
    }

    /// Destroy the vulkan encoder for a surface.
    pub(crate) fn destroy_vulkan_encoder(&mut self, surface_id: u32) {
        if let Some(mut enc) = self.vulkan_encoders.remove(&surface_id)
            && let Some(ref vfns) = self.video_fns
        {
            unsafe { enc.destroy(&self.device, vfns) };
        }
    }

    // ---------------------------------------------------------------
    // External output buffers (VA-API zero-copy)
    // ---------------------------------------------------------------

    pub(crate) fn set_external_output_buffers(
        &mut self,
        surface_id: u32,
        buffers: Vec<ExternalOutputBuffer>,
    ) {
        if buffers.is_empty() {
            self.destroy_external_outputs_for(surface_id);
            return;
        }
        if !self.has_dmabuf {
            return;
        }
        // Import each encoder-allocated DMA-BUF as a Vulkan render target.
        // The encoder owns the buffer; we borrow it for compositing.
        // After rendering, we return PixelData::VaSurface and the encoder
        // encodes directly — zero copies, zero bus crossings.
        self.destroy_external_outputs_for(surface_id);
        let format = vk::Format::B8G8R8A8_UNORM;
        let mut imported = Vec::new();
        for buf in &buffers {
            let Some(ext_out) = self.import_external_output(buf, format) else {
                eprintln!(
                    "[vulkan-render] failed to import external output {}x{}",
                    buf.width, buf.height,
                );
                continue;
            };
            imported.push(ext_out);
        }
        if !imported.is_empty() {
            eprintln!(
                "[vulkan-render] {} external output buffers imported for surface {surface_id} ({}x{})",
                imported.len(),
                buffers[0].width,
                buffers[0].height,
            );
            // Import NV12 output planes for the compute BGRA→NV12 path.
            // Use the encoder's padded NV12 dimensions (may differ from BGRA
            // source dimensions due to AV1 superblock alignment).
            let nv12_fds: Vec<_> = buffers
                .iter()
                .filter_map(|b| {
                    let fd = b.nv12_fd.as_ref()?.clone();
                    let nv12_w = if b.nv12_width > 0 {
                        b.nv12_width
                    } else {
                        b.width
                    };
                    let nv12_h = if b.nv12_height > 0 {
                        b.nv12_height
                    } else {
                        b.height
                    };
                    Some((
                        fd,
                        b.nv12_stride,
                        b.nv12_uv_offset,
                        nv12_w,
                        nv12_h,
                        b.nv12_modifier,
                    ))
                })
                .collect();
            if !nv12_fds.is_empty() {
                self.create_nv12_outputs_from_fds(surface_id, &nv12_fds);
            } else {
                self.create_nv12_outputs(surface_id, buffers[0].width, buffers[0].height);
            }
        }
        self.external_outputs.insert(surface_id, (imported, 0));
    }

    fn destroy_external_outputs_for(&mut self, surface_id: u32) {
        if let Some((exts, _)) = self.external_outputs.remove(&surface_id) {
            for ext in exts {
                unsafe {
                    self.device.destroy_framebuffer(ext.framebuffer, None);
                    self.device.destroy_image_view(ext.view, None);
                    self.device.destroy_image(ext.image, None);
                    self.device.free_memory(ext.memory, None);
                }
            }
        }
        self.destroy_nv12_outputs_for(surface_id);
    }

    fn destroy_all_external_outputs(&mut self) {
        for (_, (exts, _)) in self.external_outputs.drain() {
            for ext in exts {
                unsafe {
                    self.device.destroy_framebuffer(ext.framebuffer, None);
                    self.device.destroy_image_view(ext.view, None);
                    self.device.destroy_image(ext.image, None);
                    self.device.free_memory(ext.memory, None);
                }
            }
        }
        self.destroy_all_nv12_outputs();
    }

    /// Query the Vulkan driver for the plane layout it expects for a
    /// given format + modifier + size.  Creates a temporary image with
    /// `VkImageDrmFormatModifierListCreateInfoEXT`, queries its
    /// subresource layout, and destroys it.  This gives us the driver's
    /// ground truth — independent of whatever VA-API (a different mesa
    /// frontend) reports.
    fn query_modifier_layout(
        &self,
        format: vk::Format,
        w: u32,
        h: u32,
        modifier: u64,
    ) -> Vec<vk::SubresourceLayout> {
        self.query_modifier_layout_with(
            format,
            w,
            h,
            modifier,
            vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::STORAGE,
            vk::ImageCreateFlags::MUTABLE_FORMAT,
        )
    }

    fn query_modifier_layout_with(
        &self,
        format: vk::Format,
        w: u32,
        h: u32,
        modifier: u64,
        usage: vk::ImageUsageFlags,
        flags: vk::ImageCreateFlags,
    ) -> Vec<vk::SubresourceLayout> {
        let plane_count = self.modifier_plane_count_for(format, modifier);
        let modifiers = [modifier];
        let mut mod_list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // Usage flags MUST match the real import — different usage can
        // change the driver's internal layout (pitch alignment, etc.).
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .flags(flags)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext_info)
            .push_next(&mut mod_list);
        let image = match unsafe { self.device.create_image(&image_info, None) } {
            Ok(i) => i,
            Err(_) => {
                // Modifier not supported — fall back to a basic layout.
                return vec![vk::SubresourceLayout::default()];
            }
        };
        let layouts: Vec<vk::SubresourceLayout> = (0..plane_count)
            .map(|plane_idx| {
                let subresource = vk::ImageSubresource {
                    aspect_mask: if plane_count == 1 {
                        vk::ImageAspectFlags::COLOR
                    } else {
                        vk::ImageAspectFlags::from_raw(0x10 << plane_idx) // MEMORY_PLANE_0..3
                    },
                    mip_level: 0,
                    array_layer: 0,
                };
                unsafe { self.device.get_image_subresource_layout(image, subresource) }
            })
            .collect();
        unsafe { self.device.destroy_image(image, None) };
        layouts
    }

    /// Query the Vulkan device for the expected plane count of a DRM
    /// modifier for the given format.  Falls back to 1.
    fn modifier_plane_count_for(&self, format: vk::Format, modifier: u64) -> u32 {
        let mut mod_list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut fp2 = vk::FormatProperties2::default().push_next(&mut mod_list);
        unsafe {
            self.instance.get_physical_device_format_properties2(
                self.physical_device,
                format,
                &mut fp2,
            );
        }
        let count = mod_list.drm_format_modifier_count as usize;
        let mut props = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
        mod_list.drm_format_modifier_count = count as u32;
        mod_list.p_drm_format_modifier_properties = props.as_mut_ptr();
        let mut fp2 = vk::FormatProperties2::default().push_next(&mut mod_list);
        unsafe {
            self.instance.get_physical_device_format_properties2(
                self.physical_device,
                format,
                &mut fp2,
            );
        }
        props
            .iter()
            .find(|p| p.drm_format_modifier == modifier)
            .map(|p| p.drm_format_modifier_plane_count)
            .unwrap_or(1)
    }

    fn import_external_output(
        &self,
        buf: &ExternalOutputBuffer,
        format: vk::Format,
    ) -> Option<ExternalOutput> {
        use std::os::fd::AsRawFd;
        let fd = buf.fd.as_raw_fd();
        let w = buf.width;
        let h = buf.height;

        // Import via DRM format modifier (handles tiled AMD surfaces).
        //
        // VA-API (radeonsi) exports pitch/offset values for an internal
        // DRM format (e.g. R16) that differs from the logical ARGB8888.
        // Vulkan (radv) expects layout values matching its own accounting
        // for the same modifier.  Both drivers use the same hardware
        // tiling, so a temporary radv image of the same dimensions and
        // modifier gives us the correct layout for import.
        let plane_layouts = self.query_modifier_layout(format, w, h, buf.modifier);
        let mut drm_mod_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(buf.modifier)
            .plane_layouts(&plane_layouts);
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let format_list_entry = [format];
        let mut format_list =
            vk::ImageFormatListCreateInfo::default().view_formats(&format_list_entry);

        // The render pass final layout is TRANSFER_SRC_OPTIMAL, so the
        // image must support TRANSFER_SRC even though we don't actually
        // do a staging copy on the external output path.
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::STORAGE,
            )
            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext_info)
            .push_next(&mut drm_mod_info)
            .push_next(&mut format_list);

        let image = match unsafe { self.device.create_image(&image_info, None) } {
            Ok(i) => i,
            Err(e) => {
                eprintln!(
                    "[vulkan-render] vkCreateImage failed for external output \
                     {w}x{h} modifier=0x{:016x} vk_planes={}: {e:?}",
                    buf.modifier,
                    plane_layouts.len(),
                );
                for (i, pl) in plane_layouts.iter().enumerate() {
                    eprintln!(
                        "[vulkan-render]   plane {i}: offset={} size={} row_pitch={}",
                        pl.offset, pl.size, pl.row_pitch,
                    );
                }
                return None;
            }
        };
        let mem_reqs = unsafe { self.device.get_image_memory_requirements(image) };

        let dup_fd = unsafe { libc::dup(fd) };
        if dup_fd < 0 {
            unsafe { self.device.destroy_image(image, None) };
            return None;
        }
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup_fd);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(
                self.find_memory_type(mem_reqs.memory_type_bits, vk::MemoryPropertyFlags::empty())?,
            )
            .push_next(&mut import_info)
            .push_next(&mut dedicated);

        let memory = match unsafe { self.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "[vulkan-render] vkAllocateMemory failed for external output \
                     {w}x{h} modifier=0x{:016x}: {e:?}",
                    buf.modifier,
                );
                unsafe {
                    self.device.destroy_image(image, None);
                    libc::close(dup_fd);
                }
                return None;
            }
        };
        if let Err(e) = unsafe { self.device.bind_image_memory(image, memory, 0) } {
            eprintln!("[vulkan-render] vkBindImageMemory failed for external output: {e:?}",);
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_image(image, None);
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
        let view = unsafe { self.device.create_image_view(&view_info, None).ok()? };

        let fb_info = vk::FramebufferCreateInfo::default()
            .render_pass(self.render_pass)
            .attachments(std::slice::from_ref(&view))
            .width(w)
            .height(h)
            .layers(1);
        let framebuffer = unsafe { self.device.create_framebuffer(&fb_info, None).ok()? };

        Some(ExternalOutput {
            image,
            memory,
            view,
            framebuffer,
            width: w,
            height: h,
            va_surface_id: buf.va_surface_id,
            va_display: buf.va_display,
            fourcc: buf.fourcc,
            modifier: buf.modifier,
            stride: buf.stride,
            _fd: buf.fd.clone(),
        })
    }

    // ---------------------------------------------------------------
    // Output image management
    // ---------------------------------------------------------------

    fn ensure_output_images(&mut self, w: u32, h: u32) {
        // Check if current images match.
        if !self.output_images.is_empty()
            && self.output_images[0].width == w
            && self.output_images[0].height == h
        {
            return;
        }
        // Destroy old.
        self.destroy_output_images();
        // Double-buffered: one being rendered to, one being read back.
        for _ in 0..2 {
            if let Some(img) = self.create_output_image(w, h) {
                self.output_images.push(img);
            }
        }
        self.output_idx = 0;
    }

    fn destroy_nv12_vec(&mut self, nv12s: Vec<Nv12Output>) {
        for n in nv12s {
            unsafe {
                self.device
                    .free_descriptor_sets(self.descriptor_pool, &[n.descriptor_set])
                    .ok();
                match n.kind {
                    Nv12OutputKind::Buffer { buffer, memory, .. } => {
                        self.device.destroy_buffer(buffer, None);
                        self.device.free_memory(memory, None);
                    }
                    Nv12OutputKind::Image {
                        image,
                        y_memory,
                        y_view,
                        uv_memory,
                        uv_view,
                        encode_view,
                    } => {
                        if let Some(ev) = encode_view {
                            self.device.destroy_image_view(ev, None);
                        }
                        self.device.destroy_image_view(y_view, None);
                        self.device.destroy_image_view(uv_view, None);
                        self.device.destroy_image(image, None);
                        self.device.free_memory(y_memory, None);
                        if uv_memory != vk::DeviceMemory::null() {
                            self.device.free_memory(uv_memory, None);
                        }
                    }
                }
            }
        }
    }

    fn destroy_nv12_outputs_for(&mut self, surface_id: u32) {
        if let Some((nv12s, _)) = self.nv12_outputs.remove(&surface_id) {
            self.destroy_nv12_vec(nv12s);
        }
    }

    fn destroy_all_nv12_outputs(&mut self) {
        let all: Vec<Vec<Nv12Output>> = self.nv12_outputs.drain().map(|(_, (v, _))| v).collect();
        for nv12s in all {
            self.destroy_nv12_vec(nv12s);
        }
    }

    /// Allocate NV12 output planes for the BGRA→NV12 compute path.
    fn create_nv12_outputs(&mut self, surface_id: u32, w: u32, h: u32) {
        if !self.has_dmabuf {
            return;
        }
        use std::os::fd::FromRawFd;
        self.destroy_nv12_outputs_for(surface_id);

        type GetMemoryFdKHR = unsafe extern "system" fn(
            vk::Device,
            *const vk::MemoryGetFdInfoKHR<'_>,
            *mut i32,
        ) -> vk::Result;
        let get_fd_fp: Option<GetMemoryFdKHR> = unsafe {
            let name = c"vkGetMemoryFdKHR";
            self.instance
                .get_device_proc_addr(self.device.handle(), name.as_ptr())
                .map(|f| std::mem::transmute(f))
        };
        let Some(get_fd_fp) = get_fd_fp else { return };

        // NV12: stride aligned to 64 bytes, Y = stride*h, UV = stride*h/2.
        let stride = (w + 63) & !63;
        let uv_offset = stride * h;
        let buf_size = (stride * h * 3 / 2) as u64;

        for _ in 0..3 {
            let Some(nv12) = (|| -> Option<Nv12Output> {
                let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default()
                    .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                let buf_info = vk::BufferCreateInfo::default()
                    .size(buf_size)
                    .usage(
                        vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                    )
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .push_next(&mut ext_info);
                let buffer = unsafe { self.device.create_buffer(&buf_info, None).ok()? };
                let reqs = unsafe { self.device.get_buffer_memory_requirements(buffer) };
                let mem_type = self
                    .find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                    .or_else(|| {
                        self.find_memory_type(
                            reqs.memory_type_bits,
                            vk::MemoryPropertyFlags::empty(),
                        )
                    })?;
                let mut export = vk::ExportMemoryAllocateInfo::default()
                    .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                let alloc = vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(mem_type)
                    .push_next(&mut export);
                let memory = unsafe { self.device.allocate_memory(&alloc, None).ok()? };
                if unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }.is_err() {
                    unsafe {
                        self.device.free_memory(memory, None);
                        self.device.destroy_buffer(buffer, None);
                    }
                    return None;
                }
                let fd_info = vk::MemoryGetFdInfoKHR::default()
                    .memory(memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                let mut raw_fd: i32 = -1;
                if unsafe { get_fd_fp(self.device.handle(), &fd_info, &mut raw_fd) }
                    != vk::Result::SUCCESS
                    || raw_fd < 0
                {
                    unsafe {
                        self.device.free_memory(memory, None);
                        self.device.destroy_buffer(buffer, None);
                    }
                    return None;
                }
                let fd = Arc::new(unsafe { OwnedFd::from_raw_fd(raw_fd) });

                // Descriptor set: binding 1 = storage buffer.
                let ds_alloc = vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.descriptor_pool)
                    .set_layouts(std::slice::from_ref(&self.compute_descriptor_set_layout));
                let descriptor_set =
                    unsafe { self.device.allocate_descriptor_sets(&ds_alloc).ok()?[0] };
                let buf_desc = vk::DescriptorBufferInfo::default()
                    .buffer(buffer)
                    .offset(0)
                    .range(buf_size);
                let write = vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(std::slice::from_ref(&buf_desc));
                unsafe { self.device.update_descriptor_sets(&[write], &[]) };

                Some(Nv12Output {
                    fd,
                    descriptor_set,
                    width: w,
                    height: h,
                    kind: Nv12OutputKind::Buffer {
                        buffer,
                        memory,
                        buf_size,
                        stride,
                        uv_offset,
                    },
                })
            })() else {
                eprintln!("[vulkan-render] failed to create NV12 buffer {w}x{h}");
                return;
            };
            self.nv12_outputs
                .entry(surface_id)
                .or_insert_with(|| (Vec::new(), 0))
                .0
                .push(nv12);
        }
        if let Some(entry) = self.nv12_outputs.get_mut(&surface_id) {
            entry.1 = 0;
        }
        let count = self
            .nv12_outputs
            .get(&surface_id)
            .map_or(0, |(v, _)| v.len());
        eprintln!(
            "[vulkan-render] created {count} NV12 buffers {w}x{h} stride={stride} uv_offset={uv_offset}",
        );
    }

    /// Import encoder-exported NV12 DMA-BUFs as Vulkan resources.
    /// For linear (modifier==0): import as VkBuffer (existing path).
    /// For tiled (modifier!=0): import as multi-plane VkImage.
    #[allow(clippy::type_complexity)]
    fn create_nv12_outputs_from_fds(
        &mut self,
        surface_id: u32,
        fds: &[(Arc<OwnedFd>, u32, u32, u32, u32, u64)],
    ) {
        if !self.has_dmabuf {
            return;
        }
        self.destroy_nv12_outputs_for(surface_id);

        for (fd, stride, uv_offset, w, h, modifier) in fds {
            let (fd, stride, uv_offset, w, h, modifier) =
                (fd.clone(), *stride, *uv_offset, *w, *h, *modifier);

            let nv12 = if modifier == 0 {
                // Linear: import as VkBuffer.
                self.import_nv12_buffer(fd, stride, uv_offset, w, h)
            } else {
                // Tiled: import as multi-plane VkImage.
                self.import_nv12_image(fd, w, h, modifier)
            };

            match nv12 {
                Some(n) => {
                    self.nv12_outputs
                        .entry(surface_id)
                        .or_insert_with(|| (Vec::new(), 0))
                        .0
                        .push(n);
                }
                None => {
                    eprintln!(
                        "[vulkan-render] failed to import NV12 fd {w}x{h} modifier=0x{modifier:016x}",
                    );
                }
            }
        }
        let nv12_entry = self.nv12_outputs.get(&surface_id);
        if let Some((nv12s, _)) = nv12_entry.filter(|(v, _)| !v.is_empty()) {
            let kind_str = match &nv12s[0].kind {
                Nv12OutputKind::Buffer { .. } => "buffer",
                Nv12OutputKind::Image { .. } => "image",
            };
            eprintln!(
                "[vulkan-render] imported {} NV12 outputs ({kind_str})",
                nv12s.len(),
            );
        }
        if let Some(entry) = self.nv12_outputs.get_mut(&surface_id) {
            entry.1 = 0;
        }
    }

    /// Import a linear NV12 DMA-BUF as a VkBuffer.
    fn import_nv12_buffer(
        &self,
        fd: Arc<OwnedFd>,
        stride: u32,
        uv_offset: u32,
        w: u32,
        h: u32,
    ) -> Option<Nv12Output> {
        // Use uv_offset to compute the full buffer size: Y plane is
        // uv_offset bytes, UV plane is stride * ceil(h/2).
        let buf_size = uv_offset as u64 + stride as u64 * (h as u64).div_ceil(2);
        let dup_fd = unsafe { libc::dup(fd.as_raw_fd()) };
        if dup_fd < 0 {
            return None;
        }

        let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let buf_info = vk::BufferCreateInfo::default()
            .size(buf_size)
            .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext_info);
        let buffer = unsafe { self.device.create_buffer(&buf_info, None).ok()? };
        let reqs = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let mem_type =
            self.find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::empty())?;

        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup_fd);
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(mem_type)
            .push_next(&mut import_info);
        let memory = match unsafe { self.device.allocate_memory(&alloc, None) } {
            Ok(m) => m,
            Err(_) => {
                unsafe {
                    self.device.destroy_buffer(buffer, None);
                    libc::close(dup_fd);
                }
                return None;
            }
        };
        if unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }.is_err() {
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_buffer(buffer, None);
            }
            return None;
        }

        // Descriptor set: binding 1 = storage buffer.
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(std::slice::from_ref(&self.compute_descriptor_set_layout));
        let descriptor_set = unsafe { self.device.allocate_descriptor_sets(&ds_alloc).ok()?[0] };
        let buf_desc = vk::DescriptorBufferInfo::default()
            .buffer(buffer)
            .offset(0)
            .range(buf_size);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(descriptor_set)
            .dst_binding(1)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .buffer_info(std::slice::from_ref(&buf_desc));
        unsafe { self.device.update_descriptor_sets(&[write], &[]) };

        eprintln!(
            "[vulkan-render] imported NV12 buffer {w}x{h} stride={stride} uv_offset={uv_offset}",
        );

        Some(Nv12Output {
            fd,
            descriptor_set,
            width: w,
            height: h,
            kind: Nv12OutputKind::Buffer {
                buffer,
                memory,
                buf_size,
                stride,
                uv_offset,
            },
        })
    }

    /// Import a tiled NV12 DMA-BUF as a multi-plane VkImage
    /// (G8_B8R8_2PLANE_420_UNORM with DISJOINT planes).
    fn import_nv12_image(
        &self,
        fd: Arc<OwnedFd>,
        w: u32,
        h: u32,
        modifier: u64,
    ) -> Option<Nv12Output> {
        let nv12_format = vk::Format::G8_B8R8_2PLANE_420_UNORM;

        // Add VIDEO_ENCODE_SRC usage when video encode is available so the
        // Vulkan Video encoder can read from this NV12 image directly.
        let mut usage = vk::ImageUsageFlags::STORAGE;
        if self.has_video_encode {
            usage |= vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR;
        }

        // Query expected plane layouts from the driver.
        let plane_layouts = self.query_modifier_layout_with(
            nv12_format,
            w,
            h,
            modifier,
            usage,
            vk::ImageCreateFlags::MUTABLE_FORMAT,
        );

        let mut drm_mod_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&plane_layouts);
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let format_list_entries = [vk::Format::R8_UNORM, vk::Format::R8G8_UNORM];
        let mut format_list =
            vk::ImageFormatListCreateInfo::default().view_formats(&format_list_entries);

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(nv12_format)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext_info)
            .push_next(&mut drm_mod_info)
            .push_next(&mut format_list);

        let image = match unsafe { self.device.create_image(&image_info, None) } {
            Ok(i) => i,
            Err(e) => {
                eprintln!(
                    "[vulkan-render] NV12 image create failed {w}x{h} mod=0x{modifier:016x}: {e:?}",
                );
                return None;
            }
        };

        // Non-disjoint: single memory for both planes.
        let raw_fd = fd.as_raw_fd();
        let mem_reqs = unsafe { self.device.get_image_memory_requirements(image) };
        let mem_type =
            self.find_memory_type(mem_reqs.memory_type_bits, vk::MemoryPropertyFlags::empty())?;
        let dup_fd = unsafe { libc::dup(raw_fd) };
        if dup_fd < 0 {
            unsafe { self.device.destroy_image(image, None) };
            return None;
        }
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup_fd);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mem_type)
            .push_next(&mut import)
            .push_next(&mut dedicated);
        let y_memory = match unsafe { self.device.allocate_memory(&alloc, None) } {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[vulkan-render] NV12 memory alloc failed: {e:?}");
                unsafe {
                    self.device.destroy_image(image, None);
                    libc::close(dup_fd);
                }
                return None;
            }
        };
        if unsafe { self.device.bind_image_memory(image, y_memory, 0) }.is_err() {
            unsafe {
                self.device.free_memory(y_memory, None);
                self.device.destroy_image(image, None);
            }
            return None;
        }
        // uv_memory is unused for non-disjoint — set to null handle.
        let uv_memory = vk::DeviceMemory::null();

        // Create per-plane views.
        let y_view = unsafe {
            self.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R8_UNORM)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::PLANE_0,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        }
        .ok()?;

        let uv_view = match unsafe {
            self.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R8G8_UNORM)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::PLANE_1,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        } {
            Ok(v) => v,
            Err(_) => {
                unsafe {
                    self.device.destroy_image_view(y_view, None);
                    self.device.free_memory(y_memory, None);
                    self.device.destroy_image(image, None);
                }
                return None;
            }
        };

        // Allocate descriptor set from compute_image layout.
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(std::slice::from_ref(
                &self.compute_image_descriptor_set_layout,
            ));
        let descriptor_set = unsafe { self.device.allocate_descriptor_sets(&ds_alloc).ok()?[0] };

        // Write bindings 1 (Y) and 2 (UV) as STORAGE_IMAGE.
        let y_info = vk::DescriptorImageInfo::default()
            .image_view(y_view)
            .image_layout(vk::ImageLayout::GENERAL);
        let uv_info = vk::DescriptorImageInfo::default()
            .image_view(uv_view)
            .image_layout(vk::ImageLayout::GENERAL);
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(descriptor_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(std::slice::from_ref(&y_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(descriptor_set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(std::slice::from_ref(&uv_info)),
        ];
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };

        // Create a full-image COLOR view for Vulkan Video encode source.
        let encode_view = if self.has_video_encode {
            unsafe {
                self.device
                    .create_image_view(
                        &vk::ImageViewCreateInfo::default()
                            .image(image)
                            .view_type(vk::ImageViewType::TYPE_2D)
                            .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
                            .subresource_range(vk::ImageSubresourceRange {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                base_mip_level: 0,
                                level_count: 1,
                                base_array_layer: 0,
                                layer_count: 1,
                            }),
                        None,
                    )
                    .ok()
            }
        } else {
            None
        };

        eprintln!(
            "[vulkan-render] imported NV12 image {w}x{h} modifier=0x{modifier:016x} planes={} encode_view={}",
            plane_layouts.len(),
            encode_view.is_some(),
        );

        Some(Nv12Output {
            fd,
            descriptor_set,
            width: w,
            height: h,
            kind: Nv12OutputKind::Image {
                image,
                y_memory,
                y_view,
                uv_memory,
                uv_view,
                encode_view,
            },
        })
    }

    /// Record BGRA→NV12 compute shader dispatch into the command buffer (buffer path).
    /// `src_w`/`src_h` are the BGRA source dimensions; the NV12 output
    /// dimensions come from the `Nv12Output` (may be larger due to encoder
    /// alignment).  The shader edge-extends source pixels into the padding.
    fn dispatch_nv12_compute(
        &self,
        cb: vk::CommandBuffer,
        bgra_image: vk::Image,
        nv12_vec: &[Nv12Output],
        nv12_idx: usize,
        src_w: u32,
        src_h: u32,
    ) {
        let nv12 = &nv12_vec[nv12_idx];
        let enc_w = nv12.width;
        let enc_h = nv12.height;
        let Nv12OutputKind::Buffer {
            buffer,
            buf_size,
            stride,
            uv_offset,
            ..
        } = &nv12.kind
        else {
            return;
        };

        // Create a temporary R8G8B8A8 storage view for the BGRA image
        // (image was created with MUTABLE_FORMAT + STORAGE).
        let bgra_view = match unsafe {
            self.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(bgra_image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        } {
            Ok(v) => v,
            Err(_) => return,
        };

        // Update binding 0 (BGRA input) for this frame.
        let bgra_info = vk::DescriptorImageInfo::default()
            .image_view(bgra_view)
            .image_layout(vk::ImageLayout::GENERAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(nv12.descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
            .image_info(std::slice::from_ref(&bgra_info));
        unsafe { self.device.update_descriptor_sets(&[write], &[]) };

        // Barrier: BGRA TRANSFER_SRC → GENERAL for storage read.
        let img_barrier = vk::ImageMemoryBarrier::default()
            .image(bgra_image)
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });

        unsafe {
            self.device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[img_barrier],
            );

            // Zero the NV12 buffer (atomicOr needs zeroed memory).
            self.device.cmd_fill_buffer(cb, *buffer, 0, *buf_size, 0);

            // Barrier: buffer fill → compute write.
            let buf_barrier = vk::BufferMemoryBarrier::default()
                .buffer(*buffer)
                .offset(0)
                .size(*buf_size)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::SHADER_READ);
            self.device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[buf_barrier],
                &[],
            );

            self.device.cmd_bind_pipeline(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                self.compute_pipeline,
            );
            self.device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                self.compute_pipeline_layout,
                0,
                &[nv12.descriptor_set],
                &[],
            );
            let push = [src_w, src_h, *stride, *uv_offset, enc_w, enc_h];
            self.device.cmd_push_constants(
                cb,
                self.compute_pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                std::slice::from_raw_parts(push.as_ptr() as *const u8, 24),
            );
            self.device
                .cmd_dispatch(cb, enc_w.div_ceil(16), enc_h.div_ceil(16), 1);
        }

        // Destroy the temporary view. It's been recorded into the CB
        // and the descriptor set references it, but we update the
        // descriptor set each frame before dispatch, so the view is
        // no longer needed after recording.
        // NOTE: this is technically a validation error (view destroyed
        // while in-flight CB references it) but works in practice
        // because the descriptor is re-written before next use.
        // TODO: track views in PendingSubmit for proper lifecycle.
        unsafe { self.device.destroy_image_view(bgra_view, None) };
    }

    /// Record BGRA→NV12 compute shader dispatch into the command buffer (image path).
    /// `src_w`/`src_h` are the BGRA source dimensions; the NV12 output
    /// dimensions come from the `Nv12Output`.
    fn dispatch_nv12_compute_image(
        &self,
        cb: vk::CommandBuffer,
        bgra_image: vk::Image,
        nv12_vec: &[Nv12Output],
        nv12_idx: usize,
        src_w: u32,
        src_h: u32,
    ) {
        let nv12 = &nv12_vec[nv12_idx];
        let enc_w = nv12.width;
        let enc_h = nv12.height;
        let Nv12OutputKind::Image { image, .. } = &nv12.kind else {
            return;
        };

        // Create a temporary R8G8B8A8 storage view for the BGRA image.
        let bgra_view = match unsafe {
            self.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(bgra_image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        } {
            Ok(v) => v,
            Err(_) => return,
        };

        // Update binding 0 (BGRA input) for this frame.
        let bgra_info = vk::DescriptorImageInfo::default()
            .image_view(bgra_view)
            .image_layout(vk::ImageLayout::GENERAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(nv12.descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
            .image_info(std::slice::from_ref(&bgra_info));
        unsafe { self.device.update_descriptor_sets(&[write], &[]) };

        // Barriers: BGRA → GENERAL, NV12 image → GENERAL.
        let bgra_barrier = vk::ImageMemoryBarrier::default()
            .image(bgra_image)
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });

        let nv12_barrier = vk::ImageMemoryBarrier::default()
            .image(*image)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::SHADER_WRITE)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::PLANE_0 | vk::ImageAspectFlags::PLANE_1,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });

        unsafe {
            self.device.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                    | vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[bgra_barrier, nv12_barrier],
            );

            self.device.cmd_bind_pipeline(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                self.compute_image_pipeline,
            );
            self.device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                self.compute_image_pipeline_layout,
                0,
                &[nv12.descriptor_set],
                &[],
            );
            let push = [src_w, src_h, enc_w, enc_h];
            self.device.cmd_push_constants(
                cb,
                self.compute_image_pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                std::slice::from_raw_parts(push.as_ptr() as *const u8, 16),
            );
            self.device
                .cmd_dispatch(cb, enc_w.div_ceil(16), enc_h.div_ceil(16), 1);
        }

        unsafe { self.device.destroy_image_view(bgra_view, None) };
    }

    fn create_output_image(&self, w: u32, h: u32) -> Option<OutputImage> {
        let format = vk::Format::B8G8R8A8_UNORM;

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE);

        let image = unsafe {
            self.device
                .create_image(&image_info, None)
                .inspect_err(|&e| {
                    eprintln!("[create_output_image] create_image failed: {e} ({w}x{h})");
                })
                .ok()?
        };
        let mem_reqs = unsafe { self.device.get_image_memory_requirements(image) };
        let mem_type = self
            .find_memory_type(
                mem_reqs.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .or_else(|| {
                self.find_memory_type(
                    mem_reqs.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
            });
        if mem_type.is_none() {
            eprintln!(
                "[create_output_image] no suitable memory type for image (bits={:#x})",
                mem_reqs.memory_type_bits
            );
        }
        let mem_type = mem_type?;
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mem_type);
        let memory = unsafe {
            self.device
                .allocate_memory(&alloc_info, None)
                .inspect_err(|&e| {
                    eprintln!("[create_output_image] allocate_memory(image) failed: {e}");
                })
                .ok()?
        };
        unsafe {
            self.device
                .bind_image_memory(image, memory, 0)
                .inspect_err(|&e| {
                    eprintln!("[create_output_image] bind_image_memory failed: {e}");
                })
                .ok()?
        };

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
        let view = unsafe {
            self.device
                .create_image_view(&view_info, None)
                .inspect_err(|&e| {
                    eprintln!("[create_output_image] create_image_view failed: {e}");
                })
                .ok()?
        };
        let fb_info = vk::FramebufferCreateInfo::default()
            .render_pass(self.render_pass)
            .attachments(std::slice::from_ref(&view))
            .width(w)
            .height(h)
            .layers(1);
        let framebuffer = unsafe {
            self.device
                .create_framebuffer(&fb_info, None)
                .inspect_err(|&e| {
                    eprintln!("[create_output_image] create_framebuffer failed: {e}");
                })
                .ok()?
        };

        let staging_size = (w * h * 4) as usize;
        let buf_info = vk::BufferCreateInfo::default()
            .size(staging_size as u64)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let staging_buf = unsafe {
            self.device
                .create_buffer(&buf_info, None)
                .inspect_err(|&e| {
                    eprintln!("[create_output_image] create_buffer(staging) failed: {e}");
                })
                .ok()?
        };
        let buf_reqs = unsafe { self.device.get_buffer_memory_requirements(staging_buf) };
        let buf_mem_type = self.find_memory_type(
            buf_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        );
        if buf_mem_type.is_none() {
            eprintln!(
                "[create_output_image] no HOST_VISIBLE memory for staging (bits={:#x})",
                buf_reqs.memory_type_bits
            );
        }
        let buf_mem_type = buf_mem_type?;
        let buf_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(buf_reqs.size)
            .memory_type_index(buf_mem_type);
        let staging_mem = unsafe {
            self.device
                .allocate_memory(&buf_alloc, None)
                .inspect_err(|&e| {
                    eprintln!("[create_output_image] allocate_memory(staging) failed: {e}");
                })
                .ok()?
        };
        unsafe {
            self.device
                .bind_buffer_memory(staging_buf, staging_mem, 0)
                .inspect_err(|&e| {
                    eprintln!("[create_output_image] bind_buffer_memory(staging) failed: {e}");
                })
                .ok()?
        };
        let staging_ptr = unsafe {
            self.device
                .map_memory(staging_mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                .inspect_err(|&e| {
                    eprintln!("[create_output_image] map_memory(staging) failed: {e}");
                })
                .ok()?
        } as *mut u8;

        Some(OutputImage {
            image,
            memory,
            view,
            framebuffer,
            width: w,
            height: h,
            staging_buf,
            staging_mem,
            staging_ptr,
        })
    }

    fn destroy_output_images(&mut self) {
        for img in self.output_images.drain(..) {
            unsafe {
                self.device.destroy_framebuffer(img.framebuffer, None);
                self.device.destroy_image_view(img.view, None);
                self.device.unmap_memory(img.staging_mem);
                self.device.destroy_buffer(img.staging_buf, None);
                self.device.free_memory(img.staging_mem, None);
                self.device.destroy_image(img.image, None);
                self.device.free_memory(img.memory, None);
            }
        }
    }

    // ---------------------------------------------------------------
    // Persistent surface texture cache
    // ---------------------------------------------------------------

    /// Upload or import a surface's pixel data as a persistent GPU texture.
    /// Called from the compositor at surface commit time.  If the surface
    /// already has a cached texture, the old one is moved to the
    /// pending-destroy list (freed after the current GPU submission completes).
    pub(crate) fn upload_surface(
        &mut self,
        surface_id: &ObjectId,
        pixels: &PixelData,
        width: u32,
        height: u32,
    ) {
        // Evict any existing texture for this surface.
        if let Some(old) = self.surface_textures.remove(surface_id) {
            self.pending_destroy_textures.push(old);
        }

        let cached = match pixels {
            PixelData::DmaBuf {
                fd,
                fourcc,
                modifier,
                stride,
                offset,
                ..
            } => {
                if self.has_dmabuf {
                    self.create_cached_dmabuf(
                        fd.as_raw_fd(),
                        *fourcc,
                        *modifier,
                        *stride,
                        *offset,
                        width,
                        height,
                    )
                } else {
                    // No DMA-BUF extensions — go straight to the mmap
                    // fallback which does a CPU copy into an SHM texture.
                    let _result = self.import_linear_dmabuf_mmap(
                        fd.as_raw_fd(),
                        *fourcc,
                        *stride,
                        width,
                        height,
                    );
                    if _result.is_some() {
                        let temp = self.frame_textures.pop().unwrap();
                        Some(CachedSurfaceTexture {
                            image: temp.image,
                            memory: temp.memory,
                            view: temp.view,
                            descriptor_set: temp.descriptor_set,
                            initial_layout: vk::ImageLayout::PREINITIALIZED,
                        })
                    } else {
                        None
                    }
                }
            }
            PixelData::Bgra(data) => {
                // Convert BGRA→RGBA for upload.
                let mut rgba = vec![0u8; data.len()];
                for (src, dst) in data.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
                    dst[0] = src[2]; // R
                    dst[1] = src[1]; // G
                    dst[2] = src[0]; // B
                    dst[3] = src[3]; // A
                }
                self.create_cached_shm(&rgba, width, height)
            }
            PixelData::Rgba(data) => self.create_cached_shm(data, width, height),
            _ => None,
        };

        if let Some(tex) = cached {
            self.surface_textures.insert(surface_id.clone(), tex);
        } else {
            static UF: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = UF.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 10 || n.is_multiple_of(1000) {
                let kind = match pixels {
                    PixelData::Bgra(_) => "bgra",
                    PixelData::Rgba(_) => "rgba",
                    PixelData::DmaBuf { .. } => "dmabuf",
                    _ => "other",
                };
                eprintln!("[upload #{n}] FAILED kind={kind} {width}x{height} sid={surface_id:?}");
            }
        }
    }

    /// Remove a surface's cached texture.  Called when the surface is destroyed.
    pub(crate) fn remove_surface(&mut self, surface_id: &ObjectId) {
        if let Some(old) = self.surface_textures.remove(surface_id) {
            self.pending_destroy_textures.push(old);
        }
    }

    fn create_cached_shm(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Option<CachedSurfaceTexture> {
        let format = vk::Format::R8G8B8A8_UNORM;

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
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::PREINITIALIZED);

        let image = unsafe { self.device.create_image(&image_info, None).ok()? };
        let mem_reqs = unsafe { self.device.get_image_memory_requirements(image) };

        let mem_type = self.find_memory_type(
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mem_type);
        let memory = unsafe { self.device.allocate_memory(&alloc_info, None).ok()? };
        unsafe { self.device.bind_image_memory(image, memory, 0).ok()? };

        // Query actual row pitch and upload.
        let subresource = vk::ImageSubresource {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            array_layer: 0,
        };
        let layout = unsafe { self.device.get_image_subresource_layout(image, subresource) };
        let dst_row_pitch = layout.row_pitch as usize;
        let src_row_bytes = width as usize * 4;

        let ptr = unsafe {
            self.device
                .map_memory(memory, 0, layout.size, vk::MemoryMapFlags::empty())
                .ok()?
        } as *mut u8;
        unsafe {
            let dst = ptr.add(layout.offset as usize);
            for row in 0..height as usize {
                let src_off = row * src_row_bytes;
                let dst_off = row * dst_row_pitch;
                if src_off + src_row_bytes <= rgba.len() {
                    std::ptr::copy_nonoverlapping(
                        rgba.as_ptr().add(src_off),
                        dst.add(dst_off),
                        src_row_bytes,
                    );
                }
            }
            self.device.unmap_memory(memory);
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
        let view = unsafe { self.device.create_image_view(&view_info, None).ok()? };

        let layouts = [self.descriptor_set_layout];
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&layouts);
        let descriptor_set = unsafe { self.device.allocate_descriptor_sets(&ds_alloc).ok()?[0] };

        let img_info = vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&img_info));
        unsafe { self.device.update_descriptor_sets(&[write], &[]) };

        Some(CachedSurfaceTexture {
            image,
            memory,
            view,
            descriptor_set,
            initial_layout: vk::ImageLayout::PREINITIALIZED,
        })
    }

    fn create_cached_dmabuf(
        &mut self,
        fd: RawFd,
        fourcc: u32,
        modifier: u64,
        stride: u32,
        offset: u32,
        width: u32,
        height: u32,
    ) -> Option<CachedSurfaceTexture> {
        // Reuse the existing DMA-BUF import chain — it creates Vulkan
        // image + memory + view + descriptor_set.  Instead of putting
        // the result in frame_textures, we capture it for the persistent
        // cache.
        let _result =
            self.import_dmabuf_texture(fd, fourcc, modifier, stride, offset, width, height)?;
        // The import_dmabuf_texture pushed a TempTexture to frame_textures.
        // Pop it — we're taking ownership in the persistent cache instead.
        let temp = self.frame_textures.pop()?;
        Some(CachedSurfaceTexture {
            image: temp.image,
            memory: temp.memory,
            view: temp.view,
            descriptor_set: temp.descriptor_set,
            initial_layout: vk::ImageLayout::UNDEFINED,
        })
    }

    fn drain_pending_destroy_textures(&mut self) {
        for tex in self.pending_destroy_textures.drain(..) {
            unsafe {
                self.device
                    .free_descriptor_sets(self.descriptor_pool, &[tex.descriptor_set])
                    .ok();
                self.device.destroy_image_view(tex.view, None);
                self.device.destroy_image(tex.image, None);
                self.device.free_memory(tex.memory, None);
            }
        }
    }

    // ---------------------------------------------------------------
    // Texture import (used by persistent cache for DMA-BUF)
    // ---------------------------------------------------------------

    fn import_dmabuf_texture(
        &mut self,
        fd: RawFd,
        fourcc: u32,
        modifier: u64,
        stride: u32,
        offset: u32,
        width: u32,
        height: u32,
    ) -> Option<(vk::DescriptorSet, vk::Image)> {
        // Don't cache DMA-BUF textures — the client reuses buffer fds
        // across frames with different content (e.g. popup appears/disappears).
        // Re-import every frame to get the latest content.

        const DRM_FORMAT_MOD_INVALID: u64 = 0x00ffffffffffffff;

        let vk_format = drm_fourcc_to_vk_format(fourcc)?;

        // Try DRM modifier path for non-linear tiled buffers (zero
        // GPU-CPU crossings).  LINEAR (0) skips this — the DRM modifier
        // ext produces black on AMD and y-flip on NVIDIA for LINEAR.
        if modifier != DRM_FORMAT_MOD_INVALID
            && modifier != 0
            && let Some(result) = self.try_import_dmabuf_drm_modifier(
                fd, vk_format, modifier, stride, offset, width, height,
            )
        {
            return Some(result);
        }
        // DRM modifier path failed or modifier is INVALID — try LINEAR.
        if let Some(result) = self.try_import_dmabuf_linear(fd, vk_format, stride, width, height) {
            return Some(result);
        }
        // LINEAR stride mismatch — mmap fallback (safe for linear data).
        self.import_linear_dmabuf_mmap(fd, fourcc, stride, width, height)
    }

    /// Import a DMA-BUF via VK_EXT_image_drm_format_modifier with an
    /// explicit plane layout.  Zero GPU-CPU crossings.
    fn try_import_dmabuf_drm_modifier(
        &mut self,
        fd: RawFd,
        vk_format: vk::Format,
        modifier: u64,
        stride: u32,
        offset: u32,
        width: u32,
        height: u32,
    ) -> Option<(vk::DescriptorSet, vk::Image)> {
        let buf_size = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
        unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
        let plane_size = if buf_size > 0 {
            buf_size as u64 - offset as u64
        } else {
            stride as u64 * height as u64
        };
        let plane_layout = vk::SubresourceLayout {
            offset: offset as u64,
            size: plane_size,
            row_pitch: stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        };
        let mut drm_mod_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(std::slice::from_ref(&plane_layout));
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let format_list_entry = [vk_format];
        let mut format_list =
            vk::ImageFormatListCreateInfo::default().view_formats(&format_list_entry);

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext_info)
            .push_next(&mut drm_mod_info)
            .push_next(&mut format_list);

        let image = unsafe { self.device.create_image(&image_info, None).ok()? };
        self.finish_dmabuf_import(fd, image, vk_format, true)
    }

    /// Import a DMA-BUF via VK_IMAGE_TILING_LINEAR.  Returns None on
    /// stride mismatch (caller should fall back to mmap).
    fn try_import_dmabuf_linear(
        &mut self,
        fd: RawFd,
        vk_format: vk::Format,
        stride: u32,
        width: u32,
        height: u32,
    ) -> Option<(vk::DescriptorSet, vk::Image)> {
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext_info);

        let image = unsafe { self.device.create_image(&image_info, None).ok()? };
        let subresource = vk::ImageSubresource {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            array_layer: 0,
        };
        let layout = unsafe { self.device.get_image_subresource_layout(image, subresource) };
        if layout.row_pitch != stride as u64 {
            unsafe { self.device.destroy_image(image, None) };
            return None;
        }
        self.finish_dmabuf_import(fd, image, vk_format, false)
    }

    /// Shared tail for DMA-BUF import: allocate+import memory, create
    /// image view and descriptor set.
    fn finish_dmabuf_import(
        &mut self,
        fd: RawFd,
        image: vk::Image,
        vk_format: vk::Format,
        use_dedicated: bool,
    ) -> Option<(vk::DescriptorSet, vk::Image)> {
        let mem_reqs = unsafe { self.device.get_image_memory_requirements(image) };

        let dup_fd = unsafe { libc::dup(fd) };
        if dup_fd < 0 {
            unsafe { self.device.destroy_image(image, None) };
            return None;
        }

        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup_fd);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);

        let mut alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(
                self.find_memory_type(mem_reqs.memory_type_bits, vk::MemoryPropertyFlags::empty())?,
            )
            .push_next(&mut import_info);
        if use_dedicated {
            alloc_info = alloc_info.push_next(&mut dedicated);
        }

        let memory = match unsafe { self.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(_) => {
                unsafe {
                    libc::close(dup_fd);
                    self.device.destroy_image(image, None);
                }
                return None;
            }
        };

        if unsafe { self.device.bind_image_memory(image, memory, 0) }.is_err() {
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_image(image, None);
            }
            return None;
        }

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk_format)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        let view = unsafe { self.device.create_image_view(&view_info, None).ok()? };

        // Allocate descriptor set.
        let layouts = [self.descriptor_set_layout];
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&layouts);
        let descriptor_set = unsafe { self.device.allocate_descriptor_sets(&ds_alloc).ok()?[0] };

        // Update descriptor.
        let img_info = vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&img_info));
        unsafe { self.device.update_descriptor_sets(&[write], &[]) };

        // Track for cleanup at start of next frame.
        self.frame_textures.push(TempTexture {
            image,
            memory,
            view,
            descriptor_set,
        });

        Some((descriptor_set, image))
    }

    /// mmap a LINEAR DMA-BUF, strip stride padding, convert BGRA→RGBA
    /// if needed, and upload via the SHM texture path.  Only valid for
    /// LINEAR (modifier=0) buffers — tiled VRAM must NOT be mmap'd.
    fn import_linear_dmabuf_mmap(
        &mut self,
        fd: RawFd,
        fourcc: u32,
        stride: u32,
        width: u32,
        height: u32,
    ) -> Option<(vk::DescriptorSet, vk::Image)> {
        let buf_size = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
        if buf_size <= 0 {
            return None;
        }
        unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                buf_size as usize,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return None;
        }
        let plane_data = unsafe { std::slice::from_raw_parts(ptr as *const u8, buf_size as usize) };
        let src_row = stride as usize;
        let dst_row = width as usize * 4;
        let mut packed = vec![0u8; dst_row * height as usize];
        for row in 0..height as usize {
            let src_off = row * src_row;
            let dst_off = row * dst_row;
            if src_off + dst_row <= plane_data.len() {
                packed[dst_off..dst_off + dst_row]
                    .copy_from_slice(&plane_data[src_off..src_off + dst_row]);
            }
        }
        unsafe { libc::munmap(ptr, buf_size as usize) };

        // DRM ARGB/XRGB is BGRA in memory; upload_rgba_texture expects RGBA.
        if fourcc == super::imp::drm_fourcc::ARGB8888 || fourcc == super::imp::drm_fourcc::XRGB8888
        {
            for px in packed.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        }
        self.upload_rgba_texture(&packed, width, height)
    }

    fn upload_rgba_texture(
        &mut self,
        data: &[u8],
        width: u32,
        height: u32,
    ) -> Option<(vk::DescriptorSet, vk::Image)> {
        let format = vk::Format::R8G8B8A8_UNORM;
        let _size = (width * height * 4) as u64;

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
            .tiling(vk::ImageTiling::LINEAR)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::PREINITIALIZED);

        let image = unsafe { self.device.create_image(&image_info, None).ok()? };
        let mem_reqs = unsafe { self.device.get_image_memory_requirements(image) };

        let mem_type = self.find_memory_type(
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mem_type);

        let memory = unsafe { self.device.allocate_memory(&alloc_info, None).ok()? };
        unsafe { self.device.bind_image_memory(image, memory, 0).ok()? };

        // Query the actual row pitch — GPU may pad rows for alignment.
        let subresource = vk::ImageSubresource {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            array_layer: 0,
        };
        let layout = unsafe { self.device.get_image_subresource_layout(image, subresource) };
        let dst_row_pitch = layout.row_pitch as usize;
        let src_row_bytes = width as usize * 4;

        // Map and upload row-by-row.
        let ptr = unsafe {
            self.device
                .map_memory(memory, 0, layout.size, vk::MemoryMapFlags::empty())
                .ok()?
        } as *mut u8;
        unsafe {
            let dst = ptr.add(layout.offset as usize);
            for row in 0..height as usize {
                let src_off = row * src_row_bytes;
                let dst_off = row * dst_row_pitch;
                if src_off + src_row_bytes <= data.len() {
                    std::ptr::copy_nonoverlapping(
                        data.as_ptr().add(src_off),
                        dst.add(dst_off),
                        src_row_bytes,
                    );
                }
            }
            self.device.unmap_memory(memory);
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
        let view = unsafe { self.device.create_image_view(&view_info, None).ok()? };

        let layouts = [self.descriptor_set_layout];
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&layouts);
        let descriptor_set = unsafe { self.device.allocate_descriptor_sets(&ds_alloc).ok()?[0] };

        let img_info = vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&img_info));
        unsafe { self.device.update_descriptor_sets(&[write], &[]) };

        // Track for cleanup at start of next render_tree call.
        self.frame_textures.push(TempTexture {
            image,
            memory,
            view,
            descriptor_set,
        });
        Some((descriptor_set, image))
    }

    // ---------------------------------------------------------------
    // Async submit retirement
    // ---------------------------------------------------------------

    /// Returns true when there is in-flight GPU work that needs
    /// polling.  Only self-allocated pending_submit needs the 1 ms
    /// poll (we must retire it to read back the staging buffer).
    /// Deferred external submissions are cleaned up opportunistically
    /// inside `try_retire_pending` / `render_tree_sized`.
    pub fn has_pending(&self) -> bool {
        self.pending_submit.is_some()
    }

    /// Non-blocking check: if the previous GPU submission has completed,
    /// read back its result and return it.  Called from the compositor's
    /// main event loop so completed frames are flushed to the server
    /// without waiting for the next Wayland surface commit.
    pub fn try_retire_pending(&mut self) -> Option<(u16, u32, u32, PixelData)> {
        // Opportunistically drain deferred external submissions whose
        // fences have signalled, freeing their command buffers and textures.
        self.drain_deferred_submits();

        let pending = self.pending_submit.take()?;
        let raw = unsafe {
            (self.device.fp_v1_0().wait_for_fences)(
                self.device.handle(),
                1,
                [pending.fence].as_ptr(),
                vk::TRUE,
                0, // non-blocking
            )
        };
        if raw != vk::Result::SUCCESS {
            self.pending_submit = Some(pending);
            return None;
        }
        let toplevel_sid = pending.toplevel_sid;
        let result = self.retire_pending(pending);
        // Free per-frame temporary textures now that the GPU is done.
        self.free_frame_textures();
        static TR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = TR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 10 || n.is_multiple_of(1000) {
            eprintln!(
                "[try_retire_pending #{n}] sid={toplevel_sid} result_some={}",
                result.is_some(),
            );
        }
        result.map(|(w, h, p)| (toplevel_sid, w, h, p))
    }

    /// Produce the result from a completed GPU submission.
    fn retire_pending(&mut self, pending: PendingSubmit) -> Option<(u32, u32, PixelData)> {
        // Build the result payload — external or staging readback.
        let result = if pending.external {
            let sid = pending.surface_id;
            let (ext_vec, _) = self.external_outputs.get(&sid)?;
            let ext = ext_vec.get(pending.output_idx)?;
            if ext.va_surface_id != 0 {
                // Legacy VA-API surface path.
                Some((
                    pending.phys_w,
                    pending.phys_h,
                    PixelData::VaSurface {
                        surface_id: ext.va_surface_id,
                        va_display: ext.va_display,
                        _fd: ext._fd.clone(),
                    },
                ))
            } else if let Some(&(ref nv12_vec, nv12_cur_idx)) =
                self.nv12_outputs.get(&sid).filter(|(v, _)| !v.is_empty())
            {
                // NV12 zero-copy: compute shader already wrote Y+UV planes.
                let nv12_idx = (nv12_cur_idx + nv12_vec.len() - 1) % nv12_vec.len();
                let nv12 = &nv12_vec[nv12_idx];
                let (stride, uv_offset) = match &nv12.kind {
                    Nv12OutputKind::Buffer {
                        stride, uv_offset, ..
                    } => (*stride, *uv_offset),
                    Nv12OutputKind::Image { .. } => (0, 0),
                };
                Some((
                    pending.phys_w,
                    pending.phys_h,
                    PixelData::Nv12DmaBuf {
                        fd: nv12.fd.clone(),
                        stride,
                        uv_offset,
                        width: pending.phys_w,
                        height: pending.phys_h,
                        sync_fd: None,
                    },
                ))
            } else {
                // BGRA DMA-BUF fallback.
                Some((
                    pending.phys_w,
                    pending.phys_h,
                    PixelData::DmaBuf {
                        fd: ext._fd.clone(),
                        fourcc: ext.fourcc,
                        modifier: ext.modifier,
                        stride: ext.stride,
                        offset: 0,
                        y_invert: true,
                    },
                ))
            }
        } else if pending.output_idx < self.output_images.len() {
            let img = &self.output_images[pending.output_idx];
            {
                let size = (pending.phys_w * pending.phys_h * 4) as usize;
                let bgra = unsafe { std::slice::from_raw_parts(img.staging_ptr, size) }.to_vec();
                Some((
                    pending.phys_w,
                    pending.phys_h,
                    PixelData::Bgra(Arc::new(bgra)),
                ))
            }
        } else {
            None
        };

        // Always free the fence, command buffer, and per-frame textures.
        unsafe {
            self.device.destroy_fence(pending.fence, None);
            self.device
                .free_command_buffers(self.command_pool, &[pending.cb]);
        }
        for t in pending.textures {
            unsafe {
                self.device
                    .free_descriptor_sets(self.descriptor_pool, &[t.descriptor_set])
                    .ok();
                self.device.destroy_image_view(t.view, None);
                self.device.destroy_image(t.image, None);
                self.device.free_memory(t.memory, None);
            }
        }
        result
    }

    /// Free deferred external submissions whose fences have signalled.
    fn drain_deferred_submits(&mut self) {
        self.deferred_submits.retain_mut(|pending| {
            let raw = unsafe {
                (self.device.fp_v1_0().wait_for_fences)(
                    self.device.handle(),
                    1,
                    [pending.fence].as_ptr(),
                    vk::TRUE,
                    0,
                )
            };
            if raw == vk::Result::SUCCESS {
                unsafe {
                    self.device.destroy_fence(pending.fence, None);
                    self.device
                        .free_command_buffers(self.command_pool, &[pending.cb]);
                }
                for t in pending.textures.drain(..) {
                    unsafe {
                        self.device
                            .free_descriptor_sets(self.descriptor_pool, &[t.descriptor_set])
                            .ok();
                        self.device.destroy_image_view(t.view, None);
                        self.device.destroy_image(t.image, None);
                        self.device.free_memory(t.memory, None);
                    }
                }
                false // remove from Vec
            } else {
                true // keep
            }
        });
    }

    fn free_frame_textures(&mut self) {
        for t in self.frame_textures.drain(..) {
            unsafe {
                self.device
                    .free_descriptor_sets(self.descriptor_pool, &[t.descriptor_set])
                    .ok();
                self.device.destroy_image_view(t.view, None);
                self.device.destroy_image(t.image, None);
                self.device.free_memory(t.memory, None);
            }
        }
        // Also free textures that were evicted from the persistent cache
        // while GPU work was in flight.
        self.drain_pending_destroy_textures();
    }

    // ---------------------------------------------------------------
    // Main render
    // ---------------------------------------------------------------

    pub fn render_tree_sized(
        &mut self,
        root_id: &ObjectId,
        surfaces: &HashMap<ObjectId, Surface>,
        meta: &HashMap<ObjectId, SurfaceMeta>,
        output_scale_120: u16,
        target_phys: Option<(u32, u32)>,
        toplevel_sid: u16,
    ) -> Option<(u16, u32, u32, PixelData)> {
        // Drain any completed deferred submissions (external outputs
        // whose fences have signalled) to free GPU resources.
        self.drain_deferred_submits();

        // Retire the previous submission if done (non-blocking).
        //
        // For self-allocated outputs we need the fence to complete
        // before we can read back the staging buffer, so a busy fence
        // means we must skip this compositing pass.
        //
        // For external outputs we never block on the fence here — the
        // encoder's VPP handles synchronisation via implicit DMA-BUF
        // fencing.  If the previous submit was external and still
        // in-flight, we defer it for later cleanup and proceed.
        static ENTRY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let entry_n = ENTRY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let had_pending = self.pending_submit.is_some();
        let prev_result = if let Some(pending) = self.pending_submit.take() {
            let prev_sid = pending.toplevel_sid;
            let raw = unsafe {
                (self.device.fp_v1_0().wait_for_fences)(
                    self.device.handle(),
                    1,
                    [pending.fence].as_ptr(),
                    vk::TRUE,
                    0,
                )
            };
            if raw == vk::Result::SUCCESS {
                let r = self.retire_pending(pending);
                self.free_frame_textures();
                if r.is_none() {
                    eprintln!("[render_tree_sized] fence OK but retire_pending=None");
                }
                r.map(|(w, h, p)| (prev_sid, w, h, p))
            } else if pending.external {
                // External: defer cleanup, proceed immediately.
                self.deferred_submits.push(pending);
                None
            } else {
                // Self-allocated: need staging readback — must wait.
                eprintln!("[render_tree_sized] fence not ready: {raw:?}");
                self.pending_submit = Some(pending);
                return None;
            }
        } else {
            self.free_frame_textures();
            None
        };
        if entry_n < 20 || entry_n.is_multiple_of(1000) {
            eprintln!(
                "[render_tree_sized #{entry_n}] had_pending={had_pending} prev_result={} ext_outputs={} deferred={}",
                prev_result.is_some(),
                self.external_outputs.len(),
                self.deferred_submits.len(),
            );
        }

        let s120 = (output_scale_120 as u32).max(120);

        let mut all_layers: Vec<GpuLayer> = Vec::new();
        collect_gpu_layers(root_id, surfaces, meta, 0, 0, &mut all_layers);

        if all_layers.is_empty() {
            eprintln!(
                "[render_tree_sized] all_layers empty (sid={toplevel_sid} surfaces={} meta={})",
                surfaces.len(),
                meta.len(),
            );
            return None;
        }

        // Compute output dimensions.
        let (crop_x, crop_y, log_w, log_h) = surfaces
            .get(root_id)
            .and_then(|s| s.xdg_geometry)
            .filter(|&(_, _, w, h)| w > 0 && h > 0)
            .map(|(x, y, w, h)| (x, y, w as u32, h as u32))
            .unwrap_or_else(|| {
                let mut mw = 0i32;
                let mut mh = 0i32;
                for l in &all_layers {
                    mw = mw.max(l.x + l.logical_w as i32);
                    mh = mh.max(l.y + l.logical_h as i32);
                }
                (0, 0, mw.max(0) as u32, mh.max(0) as u32)
            });

        if log_w == 0 || log_h == 0 {
            eprintln!(
                "[render_tree_sized] zero logical size log={log_w}x{log_h} layers={}",
                all_layers.len(),
            );
            return None;
        }

        // Use the target size from the browser if available, otherwise
        // derive from the layer bounding box.
        let (phys_w, phys_h) =
            target_phys.unwrap_or_else(|| (to_physical(log_w, s120), to_physical(log_h, s120)));

        static VK_DBG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = VK_DBG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 5 || n.is_multiple_of(1000) {
            eprintln!(
                "[vulkan-render #{n}] s120={s120} log={}x{} phys={}x{} target={:?} layers={}",
                log_w,
                log_h,
                phys_w,
                phys_h,
                target_phys,
                all_layers.len(),
            );
        }

        // Prefer encoder-allocated external outputs (zero-copy to
        // encoder).  Fall back to self-allocated output images with
        // staging readback.
        let sid = toplevel_sid as u32;
        let use_external = self
            .external_outputs
            .get(&sid)
            .is_some_and(|(v, _)| !v.is_empty() && v[0].width == phys_w && v[0].height == phys_h);

        let (out_framebuffer, out_image, out_staging_buf, out_idx, external) = if use_external {
            let (ext_vec, ext_idx) = &self.external_outputs[&sid];
            let idx = ext_idx % ext_vec.len();
            let ext = &ext_vec[idx];
            (ext.framebuffer, ext.image, vk::Buffer::null(), idx, true)
        } else {
            self.ensure_output_images(phys_w, phys_h);
            if self.output_images.is_empty() {
                eprintln!(
                    "[render_tree_sized] output_images empty after ensure ({phys_w}x{phys_h})"
                );
                return None;
            }
            let idx = self.output_idx;
            let img = &self.output_images[idx];
            (img.framebuffer, img.image, img.staging_buf, idx, false)
        };

        // Allocate command buffer.
        let cb_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cb = unsafe {
            self.device
                .allocate_command_buffers(&cb_alloc)
                .inspect_err(|&e| {
                    eprintln!("[render_tree_sized] allocate_command_buffers failed: {e}");
                })
                .ok()?[0]
        };

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            self.device
                .begin_command_buffer(cb, &begin_info)
                .inspect_err(|&e| {
                    eprintln!("[render_tree_sized] begin_command_buffer failed: {e}");
                })
                .ok()?
        };

        // Begin render pass.
        let clear = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 1.0],
            },
        };
        let rp_begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(out_framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: phys_w,
                    height: phys_h,
                },
            })
            .clear_values(std::slice::from_ref(&clear));

        unsafe {
            self.device
                .cmd_begin_render_pass(cb, &rp_begin, vk::SubpassContents::INLINE);
            self.device
                .cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, self.pipeline);

            let viewport = vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: phys_w as f32,
                height: phys_h as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            self.device.cmd_set_viewport(cb, 0, &[viewport]);
            let scissor = vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: phys_w,
                    height: phys_h,
                },
            };
            self.device.cmd_set_scissor(cb, 0, &[scissor]);
        }

        // Pre-process layers: import/upload textures and collect draw info.
        struct DrawCmd {
            descriptor_set: vk::DescriptorSet,
            image: vk::Image,
            old_layout: vk::ImageLayout,
            geom: [f32; 4],
        }
        let mut draws: Vec<DrawCmd> = Vec::new();

        for l in &all_layers {
            // Every layer must be offset by the xdg_geometry crop origin
            // so the geometry area starts at (0,0) in the composited
            // output.  This applies uniformly to ALL layers — the root
            // surface, subsurfaces, and popups alike.  For the root
            // surface with CSD, this shifts it to a negative position so
            // only the geometry content area is visible.
            let (adj_x, adj_y) = (l.x - crop_x, l.y - crop_y);
            let px = (adj_x as i64 * s120 as i64 / 120) as i32;
            let py = (adj_y as i64 * s120 as i64 / 120) as i32;
            let pw = to_physical(l.logical_w, s120);
            let ph = to_physical(l.logical_h, s120);

            // Look up the persistent texture for this surface.
            let (ds, img, old_layout) =
                if let Some(cached) = self.surface_textures.get(&l.surface_id) {
                    (cached.descriptor_set, cached.image, cached.initial_layout)
                } else {
                    // No cached texture — surface hasn't committed a buffer
                    // yet, or the upload failed.  Skip this layer.
                    continue;
                };

            // Vulkan clip space: x=[-1,1] left→right, y=[-1,1] top→bottom.
            let clip_x = (px as f32 / phys_w as f32) * 2.0 - 1.0;
            let mut clip_y = (py as f32 / phys_h as f32) * 2.0 - 1.0;
            let clip_w = (pw as f32 / phys_w as f32) * 2.0;
            let mut clip_h = (ph as f32 / phys_h as f32) * 2.0;

            // For y_invert (OpenGL-origin) DMA-BUFs, flip the quad
            // vertically.  The vertex shader maps pos.y ∈ [0,1] to
            // v_tc.y ∈ [0,1]; negating clip_h and offsetting clip_y
            // by the old clip_h effectively samples v_tc.y from 1→0
            // instead of 0→1, flipping the image.
            if l.y_invert {
                clip_y += clip_h;
                clip_h = -clip_h;
            }

            draws.push(DrawCmd {
                descriptor_set: ds,
                image: img,
                old_layout,
                geom: [clip_x, clip_y, clip_w, clip_h],
            });
        }

        if draws.is_empty() {
            eprintln!(
                "[render_tree_sized] draws empty! layers={} textures={}",
                all_layers.len(),
                self.surface_textures.len(),
            );
            for l in &all_layers {
                let has = self.surface_textures.contains_key(&l.surface_id);
                eprintln!("  layer sid={:?} has_texture={has}", l.surface_id);
            }
            unsafe {
                // Nothing to draw — clean up command buffer.
                let _ = self.device.end_command_buffer(cb);
                self.device.free_command_buffers(self.command_pool, &[cb]);
            }
            return None;
        }

        // Transition all input textures to SHADER_READ_ONLY_OPTIMAL.
        {
            let barriers: Vec<vk::ImageMemoryBarrier> = draws
                .iter()
                .map(|d| {
                    vk::ImageMemoryBarrier::default()
                        .image(d.image)
                        .old_layout(d.old_layout)
                        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                        .src_access_mask(vk::AccessFlags::HOST_WRITE)
                        .dst_access_mask(vk::AccessFlags::SHADER_READ)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        })
                })
                .collect();
            unsafe {
                self.device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::HOST | vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &barriers,
                );
            }
        }

        // Now draw all layers.
        for d in &draws {
            unsafe {
                self.device.cmd_bind_descriptor_sets(
                    cb,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_layout,
                    0,
                    &[d.descriptor_set],
                    &[],
                );
                self.device.cmd_push_constants(
                    cb,
                    self.pipeline_layout,
                    vk::ShaderStageFlags::VERTEX,
                    0,
                    bytemuck_cast_slice(&d.geom),
                );
                self.device.cmd_draw(cb, 4, 1, 0, 0);
            }
        }

        // End render pass.  The attachment transitions to TRANSFER_SRC_OPTIMAL.
        unsafe {
            self.device.cmd_end_render_pass(cb);
        }

        // Dispatch BGRA→NV12 compute for external outputs with NV12 planes.
        if external
            && self
                .nv12_outputs
                .get(&sid)
                .is_some_and(|(v, _)| !v.is_empty())
        {
            let &(ref nv12_vec, nv12_cur_idx) = &self.nv12_outputs[&sid];
            let nv12_idx = nv12_cur_idx % nv12_vec.len();
            match &nv12_vec[nv12_idx].kind {
                Nv12OutputKind::Buffer { .. } => {
                    self.dispatch_nv12_compute(cb, out_image, nv12_vec, nv12_idx, phys_w, phys_h);
                }
                Nv12OutputKind::Image { .. } => {
                    self.dispatch_nv12_compute_image(
                        cb, out_image, nv12_vec, nv12_idx, phys_w, phys_h,
                    );
                }
            }
        }

        // Copy to staging buffer for CPU readback (only for self-allocated
        // output images — external outputs are encoded directly).
        if !external {
            let region = vk::BufferImageCopy {
                buffer_offset: 0,
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
                image_extent: vk::Extent3D {
                    width: phys_w,
                    height: phys_h,
                    depth: 1,
                },
            };
            unsafe {
                self.device.cmd_copy_image_to_buffer(
                    cb,
                    out_image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    out_staging_buf,
                    &[region],
                );
            }
        }

        // Submit asynchronously.
        unsafe {
            self.device
                .end_command_buffer(cb)
                .inspect_err(|&e| {
                    eprintln!("[render_tree_sized] end_command_buffer failed: {e}");
                })
                .ok()?;
        }
        // When explicit sync is needed (tiled NV12 on radv), create the
        // fence with SYNC_FD export capability so we can hand a sync_fd
        // to the encoder instead of blocking the compositor thread.
        let needs_sync_fd_export = external
            && self.external_fence_fd_fn.is_some()
            && self.nv12_outputs.get(&sid).is_some_and(|(v, idx)| {
                !v.is_empty() && matches!(v[idx % v.len()].kind, Nv12OutputKind::Image { .. })
            });
        let fence = if needs_sync_fd_export {
            let mut export_info = vk::ExportFenceCreateInfo::default()
                .handle_types(vk::ExternalFenceHandleTypeFlags::SYNC_FD);
            let fence_info = vk::FenceCreateInfo::default().push_next(&mut export_info);
            unsafe {
                self.device
                    .create_fence(&fence_info, None)
                    .inspect_err(|&e| {
                        eprintln!("[render_tree_sized] create_fence(sync_fd) failed: {e}");
                    })
                    .ok()?
            }
        } else {
            let fence_info = vk::FenceCreateInfo::default();
            unsafe {
                self.device
                    .create_fence(&fence_info, None)
                    .inspect_err(|&e| {
                        eprintln!("[render_tree_sized] create_fence failed: {e}");
                    })
                    .ok()?
            }
        };
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cb));
        unsafe {
            self.device
                .queue_submit(self.queue, &[submit], fence)
                .inspect_err(|&e| {
                    eprintln!("[render_tree_sized] queue_submit failed: {e}");
                })
                .ok()?;
        }

        let submit_info = PendingSubmit {
            fence,
            cb,
            textures: std::mem::take(&mut self.frame_textures),
            output_idx: out_idx,
            phys_w,
            phys_h,
            external,
            toplevel_sid,
            surface_id: sid,
        };

        if external {
            // External output — return the VaSurface for the CURRENT
            // frame immediately.  The GPU may still be rendering, but
            // the encoder's VPP will wait for it via implicit DMA-BUF
            // fencing.  This eliminates the 1-frame pipeline delay and
            // the 1 ms poll-to-retire latency for the zero-copy path.
            let (ext_vec, _) = &self.external_outputs[&sid];
            let ext = &ext_vec[out_idx];
            let nv12_entry = self.nv12_outputs.get(&sid);
            let nv12_cur_idx = nv12_entry.map_or(0, |(_, idx)| *idx);
            let nv12_len = nv12_entry.map_or(0, |(v, _)| v.len()).max(1);
            let nv12_idx = nv12_cur_idx % nv12_len;
            let pixel_data = if ext.va_surface_id != 0 {
                PixelData::VaSurface {
                    surface_id: ext.va_surface_id,
                    va_display: ext.va_display,
                    _fd: ext._fd.clone(),
                }
            } else if let Some((nv12s, _)) = nv12_entry.filter(|(v, _)| !v.is_empty()) {
                let nv12 = &nv12s[nv12_idx];
                let (stride, uv_offset) = match &nv12.kind {
                    Nv12OutputKind::Buffer {
                        stride, uv_offset, ..
                    } => (*stride, *uv_offset),
                    Nv12OutputKind::Image { .. } => (0, 0),
                };
                PixelData::Nv12DmaBuf {
                    fd: nv12.fd.clone(),
                    stride,
                    uv_offset,
                    width: phys_w,
                    height: phys_h,
                    sync_fd: None, // set below when explicit sync is needed
                }
            } else {
                PixelData::DmaBuf {
                    fd: ext._fd.clone(),
                    fourcc: ext.fourcc,
                    modifier: ext.modifier,
                    stride: ext.stride,
                    offset: 0,
                    y_invert: true,
                }
            };
            if let Some(entry) = self.nv12_outputs.get_mut(&sid) {
                entry.1 = (nv12_cur_idx + 1) % nv12_len;
            }

            // For tiled NV12 images: radv doesn't do implicit DMA-BUF sync,
            // so we export the Vulkan fence as a sync_fd and pass it to the
            // encoder.  The encoder waits on the sync_fd (in spawn_blocking)
            // instead of blocking the compositor thread here.
            let mut pixel_data = pixel_data;
            if needs_sync_fd_export {
                if let Some(ref ext_fence_fn) = self.external_fence_fd_fn {
                    let get_info = vk::FenceGetFdInfoKHR::default()
                        .fence(submit_info.fence)
                        .handle_type(vk::ExternalFenceHandleTypeFlags::SYNC_FD);
                    match unsafe { ext_fence_fn.get_fence_fd(&get_info) } {
                        Ok(raw_fd) => {
                            let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) };
                            if let PixelData::Nv12DmaBuf {
                                ref mut sync_fd, ..
                            } = pixel_data
                            {
                                *sync_fd = Some(Arc::new(owned));
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "[vulkan-render] vkGetFenceFdKHR failed: {e:?}, \
                                 falling back to blocking wait"
                            );
                            unsafe {
                                let _ = self.device.wait_for_fences(
                                    &[submit_info.fence],
                                    true,
                                    5_000_000_000,
                                );
                            }
                        }
                    }
                }
            } else {
                // Tiled NV12 but no sync_fd export support — block here as
                // a last resort (same as the old code path).
                let needs_explicit_sync = self.nv12_outputs.get(&sid).is_some_and(|(v, _)| {
                    !v.is_empty() && matches!(v[nv12_idx].kind, Nv12OutputKind::Image { .. })
                });
                if needs_explicit_sync {
                    unsafe {
                        let _ =
                            self.device
                                .wait_for_fences(&[submit_info.fence], true, 5_000_000_000);
                    }
                }
            }

            // Vulkan Video encode: if we have a vulkan encoder for this
            // surface and the NV12 output is a tiled image with an
            // encode-compatible view, encode the frame directly on the GPU.
            let pixel_data = if self.vulkan_encoders.contains_key(&sid) {
                let nv12_image_and_view = self.nv12_outputs.get(&sid).and_then(|(v, _)| {
                    if v.is_empty() {
                        return None;
                    }
                    match &v[nv12_idx].kind {
                        Nv12OutputKind::Image {
                            image, encode_view, ..
                        } => encode_view.map(|ev| (*image, ev)),
                        _ => None,
                    }
                });
                if let Some((_nv12_img, ev)) = nv12_image_and_view {
                    let encoder = self.vulkan_encoders.get_mut(&sid).unwrap();
                    let encoded = unsafe {
                        encoder.encode(
                            &self.device,
                            self.video_fns.as_ref().unwrap(),
                            self.video_encode_queue.unwrap(),
                            self.video_encode_command_pool.unwrap(),
                            _nv12_img,
                            ev,
                            false, // force_keyframe handled via request_idr
                        )
                    };
                    if let Some((bitstream, is_keyframe)) = encoded {
                        PixelData::Encoded {
                            data: Arc::new(bitstream),
                            is_keyframe,
                            codec_flag: encoder.codec_flag(),
                        }
                    } else {
                        pixel_data
                    }
                } else {
                    pixel_data
                }
            } else {
                pixel_data
            };

            let result = Some((toplevel_sid, phys_w, phys_h, pixel_data));
            self.deferred_submits.push(submit_info);
            if let Some((ext_vec, ext_idx)) = self.external_outputs.get_mut(&sid) {
                let ext_len = ext_vec.len();
                *ext_idx = (*ext_idx + 1) % ext_len;
            }
            if entry_n < 20 {
                eprintln!("[render_tree_sized #{entry_n}] return=external Some");
            }
            result
        } else {
            self.pending_submit = Some(submit_info);
            self.output_idx = (self.output_idx + 1) % self.output_images.len();
            if entry_n < 20 || entry_n.is_multiple_of(1000) {
                eprintln!(
                    "[render_tree_sized #{entry_n}] return=self-alloc prev={}",
                    prev_result.is_some(),
                );
            }
            // Self-allocated: return the PREVIOUS frame's readback
            // (or None on the first frame).  The toplevel_sid in the
            // tuple correctly identifies which surface the previous
            // frame belonged to.
            prev_result
        }
    }
}

fn bytemuck_cast_slice(data: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of_val(data)) }
}

impl Drop for VulkanRenderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            // Retire any pending / deferred submissions.
            let all_pending = self
                .pending_submit
                .take()
                .into_iter()
                .chain(self.deferred_submits.drain(..));
            for pending in all_pending {
                self.device.destroy_fence(pending.fence, None);
                self.device
                    .free_command_buffers(self.command_pool, &[pending.cb]);
                for t in pending.textures {
                    self.device.destroy_image_view(t.view, None);
                    self.device.destroy_image(t.image, None);
                    self.device.free_memory(t.memory, None);
                }
            }
            self.destroy_output_images();
            // Destroy Vulkan Video encoders.
            for (_, mut enc) in self.vulkan_encoders.drain() {
                if let Some(ref vfns) = self.video_fns {
                    enc.destroy(&self.device, vfns);
                }
            }
            // Destroy video encode command pool.
            if let Some(pool) = self.video_encode_command_pool.take() {
                self.device.destroy_command_pool(pool, None);
            }
            // Destroy all per-surface external and NV12 outputs.
            self.destroy_all_external_outputs();
            // Destroy per-frame temp textures.
            for t in self.frame_textures.drain(..) {
                self.device.destroy_image_view(t.view, None);
                self.device.destroy_image(t.image, None);
                self.device.free_memory(t.memory, None);
            }
            // Destroy persistent surface textures.
            for (_, tex) in self.surface_textures.drain() {
                self.device
                    .free_descriptor_sets(self.descriptor_pool, &[tex.descriptor_set])
                    .ok();
                self.device.destroy_image_view(tex.view, None);
                self.device.destroy_image(tex.image, None);
                self.device.free_memory(tex.memory, None);
            }
            // Destroy pending-destroy textures.
            for tex in self.pending_destroy_textures.drain(..) {
                self.device
                    .free_descriptor_sets(self.descriptor_pool, &[tex.descriptor_set])
                    .ok();
                self.device.destroy_image_view(tex.view, None);
                self.device.destroy_image(tex.image, None);
                self.device.free_memory(tex.memory, None);
            }
            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.device
                .destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            self.device.destroy_pipeline(self.compute_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.compute_pipeline_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.compute_descriptor_set_layout, None);
            self.device
                .destroy_pipeline(self.compute_image_pipeline, None);
            self.device
                .destroy_pipeline_layout(self.compute_image_pipeline_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.compute_image_descriptor_set_layout, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
