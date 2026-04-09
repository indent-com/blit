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
use super::render::{GpuLayer, collect_gpu_layers, to_physical};

// ===================================================================
// VulkanRenderer
// ===================================================================

pub(crate) struct VulkanRenderer {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    physical_device: vk::PhysicalDevice,
    queue: vk::Queue,
    #[expect(dead_code)]
    queue_family: u32,
    command_pool: vk::CommandPool,

    // Extensions
    ext_mem_fd: ash::khr::external_memory_fd::Device,

    // Render pipeline
    render_pass: vk::RenderPass,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    sampler: vk::Sampler,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,

    // Output images (triple-buffered)
    output_images: Vec<OutputImage>,
    output_idx: usize,

    // Per-frame temporary textures (SHM uploads) — freed at start of next frame.
    frame_textures: Vec<TempTexture>,

    // In-flight GPU submission that timed out — tracked for later cleanup.
    pending_submit: Option<PendingSubmit>,
}

struct TempTexture {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    descriptor_set: vk::DescriptorSet,
}

/// In-flight GPU submission that timed out.  Resources are kept alive
/// until the fence signals so the GPU doesn't access freed memory.
struct PendingSubmit {
    fence: vk::Fence,
    cb: vk::CommandBuffer,
    textures: Vec<TempTexture>,
}

unsafe impl Send for VulkanRenderer {}

#[expect(dead_code)]
struct OutputImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    width: u32,
    height: u32,
    /// DMA-BUF fd for export to VA-API.
    dmabuf_fd: Option<OwnedFd>,
    /// Staging buffer for CPU readback.
    staging_buf: vk::Buffer,
    staging_mem: vk::DeviceMemory,
    staging_ptr: *mut u8,
    staging_size: usize,
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
        let (physical_device, queue_family) =
            Self::find_device(&instance, &phys_devices, drm_device)?;

        // Device extensions for DMA-BUF import/export.
        let device_extensions = [
            ash::khr::external_memory_fd::NAME.as_ptr(),
            ash::khr::external_memory::NAME.as_ptr(),
            ash::ext::external_memory_dma_buf::NAME.as_ptr(),
            ash::ext::image_drm_format_modifier::NAME.as_ptr(),
            ash::khr::image_format_list::NAME.as_ptr(),
        ];

        let queue_priorities = [1.0f32];
        let queue_create = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&queue_priorities);

        let device_create = vk::DeviceCreateInfo::default()
            .queue_create_infos(std::slice::from_ref(&queue_create))
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

        let ext_mem_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);

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

        // Descriptor pool (pre-allocate for texture cache).
        let pool_size = vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(256);
        let dp_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(256)
            .pool_sizes(std::slice::from_ref(&pool_size))
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

        eprintln!("[vulkan-render] initialized on {drm_device}");

        Some(Self {
            _entry: entry,
            instance,
            device,
            physical_device,
            queue,
            queue_family,
            command_pool,
            ext_mem_fd,
            render_pass,
            pipeline_layout,
            pipeline,
            sampler,
            descriptor_set_layout,
            descriptor_pool,
            output_images: Vec::new(),
            output_idx: 0,
            frame_textures: Vec::new(),
            pending_submit: None,
        })
    }

    fn find_device(
        instance: &ash::Instance,
        devices: &[vk::PhysicalDevice],
        _drm_device: &str,
    ) -> Option<(vk::PhysicalDevice, u32)> {
        // For now, pick the first device with a graphics queue.
        // TODO: match against the DRM render node.
        for &pd in devices {
            let props = unsafe { instance.get_physical_device_queue_family_properties(pd) };
            for (i, qf) in props.iter().enumerate() {
                if qf.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
                    return Some((pd, i as u32));
                }
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
    // External output buffers (VA-API zero-copy)
    // ---------------------------------------------------------------

    pub(crate) fn set_external_output_buffers(&mut self, _buffers: Vec<ExternalOutputBuffer>) {
        // VA-API external buffer path is not used with Vulkan renderer.
        // We export our own DMA-BUFs instead.
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
        // Create triple-buffered output.
        for _ in 0..3 {
            if let Some(img) = self.create_output_image(w, h) {
                self.output_images.push(img);
            }
        }
        self.output_idx = 0;
    }

    fn create_output_image(&self, w: u32, h: u32) -> Option<OutputImage> {
        let format = vk::Format::B8G8R8A8_UNORM;

        // Create image with external memory export capability.
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

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
            .tiling(vk::ImageTiling::LINEAR)
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext_info);

        let image = unsafe { self.device.create_image(&image_info, None).ok()? };
        let mem_reqs = unsafe { self.device.get_image_memory_requirements(image) };

        // Allocate with export capability.
        let mut export_info = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let mem_type = self
            .find_memory_type(
                mem_reqs.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
            .or_else(|| {
                self.find_memory_type(
                    mem_reqs.memory_type_bits,
                    vk::MemoryPropertyFlags::DEVICE_LOCAL,
                )
            })?;

        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mem_type)
            .push_next(&mut export_info);

        let memory = unsafe { self.device.allocate_memory(&alloc_info, None).ok()? };
        unsafe { self.device.bind_image_memory(image, memory, 0).ok()? };

        // Export as DMA-BUF fd.
        let fd_info = vk::MemoryGetFdInfoKHR::default()
            .memory(memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let fd = unsafe { self.ext_mem_fd.get_memory_fd(&fd_info).ok()? };
        let dmabuf_fd = Some(unsafe { OwnedFd::from_raw_fd(fd) });

        // Image view.
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

        // Framebuffer.
        let fb_info = vk::FramebufferCreateInfo::default()
            .render_pass(self.render_pass)
            .attachments(std::slice::from_ref(&view))
            .width(w)
            .height(h)
            .layers(1);
        let framebuffer = unsafe { self.device.create_framebuffer(&fb_info, None).ok()? };

        // Staging buffer for CPU readback.
        let staging_size = (w * h * 4) as usize;
        let buf_info = vk::BufferCreateInfo::default()
            .size(staging_size as u64)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let staging_buf = unsafe { self.device.create_buffer(&buf_info, None).ok()? };
        let buf_reqs = unsafe { self.device.get_buffer_memory_requirements(staging_buf) };
        let buf_mem_type = self.find_memory_type(
            buf_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let buf_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(buf_reqs.size)
            .memory_type_index(buf_mem_type);
        let staging_mem = unsafe { self.device.allocate_memory(&buf_alloc, None).ok()? };
        unsafe {
            self.device
                .bind_buffer_memory(staging_buf, staging_mem, 0)
                .ok()?
        };
        let staging_ptr = unsafe {
            self.device
                .map_memory(staging_mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                .ok()?
        } as *mut u8;

        Some(OutputImage {
            image,
            memory,
            view,
            framebuffer,
            width: w,
            height: h,
            dmabuf_fd,
            staging_buf,
            staging_mem,
            staging_ptr,
            staging_size,
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
    // Texture import
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
        const DRM_FORMAT_MOD_LINEAR: u64 = 0;

        let vk_format = drm_fourcc_to_vk_format(fourcc)?;

        // Choose the import tiling mode based on the DMA-BUF modifier.
        // LINEAR (0) or INVALID uses the old VK_IMAGE_TILING_LINEAR path.
        // Any other modifier uses VK_EXT_image_drm_format_modifier so the
        // driver can handle vendor-specific tiled layouts (e.g. NVIDIA).
        let use_drm_modifier =
            modifier != DRM_FORMAT_MOD_LINEAR && modifier != DRM_FORMAT_MOD_INVALID;

        let image = if use_drm_modifier {
            // DRM format modifier path — VK_EXT_image_drm_format_modifier.
            // Provides the exact modifier + plane layout so the driver
            // can import tiled VRAM buffers directly.
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

            unsafe { self.device.create_image(&image_info, None).ok() }
        } else {
            // LINEAR path.
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

            let img = unsafe { self.device.create_image(&image_info, None).ok()? };

            // Check whether the driver's LINEAR row pitch matches the
            // DMA-BUF stride.  When they differ (e.g. different alignment
            // between the client allocator and the compositor's Vulkan
            // driver), binding the foreign memory and sampling would
            // produce diagonal-shear artefacts.  Fall back to mmap +
            // row-by-row upload in that case.
            let subresource = vk::ImageSubresource {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                array_layer: 0,
            };
            let layout = unsafe { self.device.get_image_subresource_layout(img, subresource) };
            if layout.row_pitch != stride as u64 {
                unsafe { self.device.destroy_image(img, None) };
                return self.import_dmabuf_mmap_fallback(fd, fourcc, stride, width, height);
            }
            Some(img)
        };

        let image = image?;
        let mem_reqs = unsafe { self.device.get_image_memory_requirements(image) };

        // Import the fd.
        let dup_fd = unsafe { libc::dup(fd) };
        if dup_fd < 0 {
            unsafe { self.device.destroy_image(image, None) };
            return None;
        }

        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup_fd);

        // For DRM modifier imports use a dedicated allocation — required
        // by most drivers for tiled layouts.
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);

        let mut alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(
                self.find_memory_type(mem_reqs.memory_type_bits, vk::MemoryPropertyFlags::empty())?,
            )
            .push_next(&mut import_info);
        if use_drm_modifier {
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

    /// Fallback: mmap the DMA-BUF, strip stride padding, convert
    /// BGRA→RGBA if needed, and upload via the SHM texture path.
    fn import_dmabuf_mmap_fallback(
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

        // DMA-BUF sync: try to wait for the client's GPU writes before
        // mmap-reading.  Anonymous /dmabuf fds (Vulkan WSI) may carry
        // implicit fences that block indefinitely on SYNC_START — they
        // can depend on the compositor releasing a previous buffer, which
        // we're deferring until after compositing.  Use a non-blocking
        // poll() to detect whether the fence is ready; skip the sync if
        // not (accept possible tearing rather than deadlocking).
        #[repr(C)]
        struct DmaBufSync {
            flags: u64,
        }
        const DMA_BUF_SYNC_READ: u64 = 1;
        const DMA_BUF_SYNC_START: u64 = 0;
        const DMA_BUF_SYNC_END: u64 = 4;
        const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x40086200;

        let did_sync = {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pfd, 1, 0) }; // non-blocking
            if ready > 0 {
                let sync_start = DmaBufSync {
                    flags: DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ,
                };
                unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC as _, &sync_start) };
                true
            } else {
                false // fence not ready — skip sync, accept possible tearing
            }
        };

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
            if did_sync {
                let sync_end = DmaBufSync {
                    flags: DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ,
                };
                unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC as _, &sync_end) };
            }
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
        if did_sync {
            let sync_end = DmaBufSync {
                flags: DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ,
            };
            unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC as _, &sync_end) };
        }

        // Convert BGRA↔RGBA if the DMA-BUF fourcc is ARGB/XRGB
        // (BGRA in memory) but upload_rgba_texture expects RGBA.
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
    // Main render
    // ---------------------------------------------------------------

    pub fn render_tree_sized(
        &mut self,
        root_id: &ObjectId,
        surfaces: &HashMap<ObjectId, Surface>,
        cache: &HashMap<ObjectId, (u32, u32, i32, bool, PixelData)>,
        output_scale_120: u16,
        target_phys: Option<(u32, u32)>,
    ) -> Option<(u32, u32, PixelData)> {
        // Retire any previous submission that timed out.  If the GPU is
        // still working on it, skip Vulkan compositing entirely so the
        // compositor thread never blocks.
        if let Some(pending) = self.pending_submit.take() {
            let raw = unsafe {
                (self.device.fp_v1_0().wait_for_fences)(
                    self.device.handle(),
                    1,
                    [pending.fence].as_ptr(),
                    vk::TRUE,
                    0, // non-blocking check
                )
            };
            if raw == vk::Result::SUCCESS {
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
            } else {
                // GPU still busy — fall back to CPU compositing.
                self.pending_submit = Some(pending);
                return None;
            }
        }

        // Free per-frame temporary textures from the previous frame.
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

        let s120 = (output_scale_120 as u32).max(120);

        let mut all_layers: Vec<GpuLayer<'_>> = Vec::new();
        collect_gpu_layers(root_id, surfaces, cache, 0, 0, &mut all_layers);

        if all_layers.is_empty() {
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

        self.ensure_output_images(phys_w, phys_h);
        if self.output_images.is_empty() {
            return None;
        }

        let out_idx = self.output_idx;
        // Copy what we need from the output image to avoid borrowing self.
        let out_framebuffer = self.output_images[out_idx].framebuffer;
        let out_image = self.output_images[out_idx].image;
        let out_staging_buf = self.output_images[out_idx].staging_buf;
        let out_staging_ptr = self.output_images[out_idx].staging_ptr;

        // Allocate command buffer.
        let cb_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cb = unsafe { self.device.allocate_command_buffers(&cb_alloc).ok()?[0] };

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe { self.device.begin_command_buffer(cb, &begin_info).ok()? };

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
            let (adj_x, adj_y) = if l.logical_w == log_w && l.logical_h == log_h {
                (l.x, l.y)
            } else {
                (l.x - crop_x, l.y - crop_y)
            };
            let px = (adj_x as i64 * s120 as i64 / 120) as i32;
            let py = (adj_y as i64 * s120 as i64 / 120) as i32;
            let pw = to_physical(l.logical_w, s120);
            let ph = to_physical(l.logical_h, s120);

            let (ds, img, old_layout) = match l.pixels {
                PixelData::DmaBuf {
                    fd,
                    fourcc,
                    modifier,
                    stride,
                    offset,
                    ..
                } => {
                    match self.import_dmabuf_texture(
                        fd.as_raw_fd(),
                        *fourcc,
                        *modifier,
                        *stride,
                        *offset,
                        l.pixel_w,
                        l.pixel_h,
                    ) {
                        Some((d, i)) => (d, i, vk::ImageLayout::UNDEFINED),
                        None => {
                            // Vulkan DMA-BUF import failed (e.g. tiled
                            // VRAM buffer on NVIDIA).  Fall back to CPU
                            // readback so the layer is still composited
                            // instead of silently dropped (black frame).
                            let rgba = l.pixels.to_rgba(l.pixel_w, l.pixel_h);
                            if rgba.is_empty() {
                                continue;
                            }
                            match self.upload_rgba_texture(&rgba, l.pixel_w, l.pixel_h) {
                                Some((d, i)) => (d, i, vk::ImageLayout::PREINITIALIZED),
                                None => continue,
                            }
                        }
                    }
                }
                _ => {
                    let rgba = l.pixels.to_rgba(l.pixel_w, l.pixel_h);
                    if rgba.is_empty() {
                        continue;
                    }
                    match self.upload_rgba_texture(&rgba, l.pixel_w, l.pixel_h) {
                        Some((d, i)) => (d, i, vk::ImageLayout::PREINITIALIZED),
                        None => continue,
                    }
                }
            };

            // Vulkan clip space: x=[-1,1] left→right, y=[-1,1] top→bottom.
            let clip_x = (px as f32 / phys_w as f32) * 2.0 - 1.0;
            let clip_y = (py as f32 / phys_h as f32) * 2.0 - 1.0;
            let clip_w = (pw as f32 / phys_w as f32) * 2.0;
            let clip_h = (ph as f32 / phys_h as f32) * 2.0;

            draws.push(DrawCmd {
                descriptor_set: ds,
                image: img,
                old_layout,
                geom: [clip_x, clip_y, clip_w, clip_h],
            });
        }

        if draws.is_empty() {
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

        // Copy to staging buffer for CPU readback.
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

        // Submit and wait with a bounded timeout.  On AMD VAAPI the GPU
        // can stall when clients contend for hardware decoders; an
        // unbounded wait would freeze the entire compositor thread.
        unsafe {
            self.device.end_command_buffer(cb).ok()?;
        }
        let fence_info = vk::FenceCreateInfo::default();
        let fence = unsafe { self.device.create_fence(&fence_info, None).ok()? };
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cb));
        unsafe {
            self.device
                .queue_submit(self.queue, &[submit], fence)
                .ok()?;
        }
        let wait_result = unsafe {
            (self.device.fp_v1_0().wait_for_fences)(
                self.device.handle(),
                1,
                [fence].as_ptr(),
                vk::TRUE,
                500_000_000, // 500 ms
            )
        };
        if wait_result != vk::Result::SUCCESS {
            eprintln!(
                "[vulkan-render] vkWaitForFences timed out ({wait_result:?}), \
                 deferring to CPU compositing"
            );
            self.pending_submit = Some(PendingSubmit {
                fence,
                cb,
                textures: std::mem::take(&mut self.frame_textures),
            });
            return None;
        }
        unsafe {
            self.device.destroy_fence(fence, None);
            self.device.free_command_buffers(self.command_pool, &[cb]);
        }

        // Read back from staging buffer.
        let size = (phys_w * phys_h * 4) as usize;
        let bgra = unsafe { std::slice::from_raw_parts(out_staging_ptr, size) }.to_vec();

        self.output_idx = (self.output_idx + 1) % self.output_images.len();

        Some((phys_w, phys_h, PixelData::Bgra(Arc::new(bgra))))
    }
}

fn bytemuck_cast_slice(data: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of_val(data)) }
}

impl Drop for VulkanRenderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            // Retire any pending submission.
            if let Some(pending) = self.pending_submit.take() {
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
            // Destroy per-frame temp textures.
            for t in self.frame_textures.drain(..) {
                self.device.destroy_image_view(t.view, None);
                self.device.destroy_image(t.image, None);
                self.device.free_memory(t.memory, None);
            }
            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.device
                .destroy_descriptor_set_layout(self.descriptor_set_layout, None);
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
