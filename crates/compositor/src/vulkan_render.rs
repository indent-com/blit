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
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
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

    /// Supported DRM format modifiers queried from the Vulkan device.
    pub(crate) supported_dmabuf_modifiers: Vec<(u32, u64)>,

    /// Encoder-allocated output buffers imported as Vulkan render targets.
    /// When non-empty, `render_tree_sized` renders into these instead of
    /// self-allocated output images, and returns `PixelData::VaSurface`
    /// for true zero-copy encoding.
    external_outputs: Vec<ExternalOutput>,
    external_output_idx: usize,
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
    /// Keep the DMA-BUF fd alive.
    _fd: Arc<OwnedFd>,
}

struct TempTexture {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    descriptor_set: vk::DescriptorSet,
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
}

unsafe impl Send for VulkanRenderer {}

struct OutputImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    width: u32,
    height: u32,

    /// Staging buffer for CPU readback.
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

        // Query supported DRM format modifiers for each format we accept.
        // Clients (Chromium, mpv, …) will pick from these when allocating
        // DMA-BUFs, ensuring the GPU can import them with the correct
        // tiling layout.
        let supported_dmabuf_modifiers = {
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
        };

        Some(Self {
            _entry: entry,
            instance,
            device,
            physical_device,
            queue,
            queue_family,
            command_pool,
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
            supported_dmabuf_modifiers,
            external_outputs: Vec::new(),
            external_output_idx: 0,
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

    pub(crate) fn set_external_output_buffers(&mut self, buffers: Vec<ExternalOutputBuffer>) {
        if buffers.is_empty() {
            self.destroy_external_outputs();
            return;
        }
        // Import each encoder-allocated DMA-BUF as a Vulkan render target.
        // The encoder owns the buffer; we borrow it for compositing.
        // After rendering, we return PixelData::VaSurface and the encoder
        // encodes directly — zero copies, zero bus crossings.
        self.destroy_external_outputs();
        let format = vk::Format::B8G8R8A8_UNORM;
        for buf in &buffers {
            let Some(ext_out) = self.import_external_output(buf, format) else {
                eprintln!(
                    "[vulkan-render] failed to import external output {}x{}",
                    buf.width, buf.height,
                );
                continue;
            };
            self.external_outputs.push(ext_out);
        }
        self.external_output_idx = 0;
        if !self.external_outputs.is_empty() {
            eprintln!(
                "[vulkan-render] {} external output buffers imported ({}x{})",
                self.external_outputs.len(),
                buffers[0].width,
                buffers[0].height,
            );
        }
    }

    fn destroy_external_outputs(&mut self) {
        for ext in self.external_outputs.drain(..) {
            unsafe {
                self.device.destroy_framebuffer(ext.framebuffer, None);
                self.device.destroy_image_view(ext.view, None);
                self.device.destroy_image(ext.image, None);
                self.device.free_memory(ext.memory, None);
            }
        }
        self.external_output_idx = 0;
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
        let buf_size = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
        unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
        let plane_size = if buf_size > 0 {
            buf_size as u64 - buf.offset as u64
        } else {
            buf.stride as u64 * h as u64
        };
        let plane_layout = vk::SubresourceLayout {
            offset: buf.offset as u64,
            size: plane_size,
            row_pitch: buf.stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        };
        let mut drm_mod_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(buf.modifier)
            .plane_layouts(std::slice::from_ref(&plane_layout));
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let format_list_entry = [format];
        let mut format_list =
            vk::ImageFormatListCreateInfo::default().view_formats(&format_list_entry);

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
                    | vk::ImageUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext_info)
            .push_next(&mut drm_mod_info)
            .push_next(&mut format_list);

        let image = unsafe { self.device.create_image(&image_info, None).ok()? };
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
            Err(_) => {
                unsafe {
                    self.device.destroy_image(image, None);
                    libc::close(dup_fd);
                }
                return None;
            }
        };
        unsafe { self.device.bind_image_memory(image, memory, 0).ok()? };

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

    fn create_output_image(&self, w: u32, h: u32) -> Option<OutputImage> {
        let format = vk::Format::B8G8R8A8_UNORM;

        // TILING_OPTIMAL + DEVICE_LOCAL: GPU renders at full speed in
        // VRAM.  Staging buffer (HOST_VISIBLE) handles CPU readback via
        // cmd_copy_image_to_buffer.
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

        let image = unsafe { self.device.create_image(&image_info, None).ok()? };
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
            })?;
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mem_type);
        let memory = unsafe { self.device.allocate_memory(&alloc_info, None).ok()? };
        unsafe { self.device.bind_image_memory(image, memory, 0).ok()? };

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

    /// Returns true when there is in-flight GPU work that hasn't been
    /// retired yet.  The event loop uses this to shorten its poll
    /// timeout so it can call `try_retire_pending` promptly.
    pub fn has_pending(&self) -> bool {
        self.pending_submit.is_some()
    }

    /// Non-blocking check: if the previous GPU submission has completed,
    /// read back its result and return it.  Called from the compositor's
    /// main event loop so completed frames are flushed to the server
    /// without waiting for the next Wayland surface commit.
    pub fn try_retire_pending(&mut self) -> Option<(u32, u32, PixelData)> {
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
        let result = self.retire_pending(pending);
        // Free per-frame temporary textures now that the GPU is done.
        self.free_frame_textures();
        result
    }

    /// Produce the result from a completed GPU submission.
    fn retire_pending(&mut self, pending: PendingSubmit) -> Option<(u32, u32, PixelData)> {
        // External output → encoder owns the buffer → zero-copy VaSurface.
        if pending.external {
            let ext = self.external_outputs.get(pending.output_idx)?;
            return Some((
                pending.phys_w,
                pending.phys_h,
                PixelData::VaSurface {
                    surface_id: ext.va_surface_id,
                    va_display: ext.va_display,
                    _fd: ext._fd.clone(),
                },
            ));
        }

        // Self-allocated output → staging readback.
        let result = if pending.output_idx < self.output_images.len() {
            let img = &self.output_images[pending.output_idx];
            let size = (pending.phys_w * pending.phys_h * 4) as usize;
            let bgra = unsafe { std::slice::from_raw_parts(img.staging_ptr, size) }.to_vec();
            Some((
                pending.phys_w,
                pending.phys_h,
                PixelData::Bgra(Arc::new(bgra)),
            ))
        } else {
            None
        };
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
        // Retire the previous submission if done (non-blocking).
        // The result we return is from the PREVIOUS frame; the current
        // frame is submitted asynchronously and retired on the next call
        // or by the event loop via try_retire_pending().
        let prev_result = if self.pending_submit.is_some() {
            let r = self.try_retire_pending();
            if self.pending_submit.is_some() {
                // GPU still busy — skip this compositing pass.
                return None;
            }
            r
        } else {
            self.free_frame_textures();
            None
        };

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

        // Prefer encoder-allocated external outputs (zero-copy to
        // encoder).  Fall back to self-allocated output images with
        // staging readback.
        let use_external = !self.external_outputs.is_empty()
            && self.external_outputs[0].width == phys_w
            && self.external_outputs[0].height == phys_h;

        let (out_framebuffer, out_image, out_staging_buf, out_idx, external) = if use_external {
            let idx = self.external_output_idx % self.external_outputs.len();
            let ext = &self.external_outputs[idx];
            (ext.framebuffer, ext.image, vk::Buffer::null(), idx, true)
        } else {
            self.ensure_output_images(phys_w, phys_h);
            if self.output_images.is_empty() {
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

        let mut is_first_layer = true;
        for l in &all_layers {
            // Every layer must be offset by the xdg_geometry crop origin
            // so the geometry area starts at (0,0) in the composited
            // output.  The first layer (root surface) is a special case
            // only when its logical size exactly matches the output —
            // meaning the surface *is* the geometry (crop_x/y are zero
            // or the surface already accounts for them).  Child layers
            // (popups, subsurfaces) must ALWAYS be cropped even if they
            // happen to share the output dimensions; otherwise they
            // render at the wrong position.
            let (adj_x, adj_y) = if is_first_layer && l.logical_w == log_w && l.logical_h == log_h {
                (l.x, l.y)
            } else {
                (l.x - crop_x, l.y - crop_y)
            };
            is_first_layer = false;
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
                            // DMA-BUF import failed — skip this layer.
                            // CPU mmap is not an option: GPU buffers use
                            // vendor-specific tiled layouts that produce
                            // garbage when read linearly and may block on
                            // VRAM page faults.
                            eprintln!(
                                "[vulkan-render] DMA-BUF import failed for {}x{} fourcc=0x{:08x} modifier=0x{:016x}",
                                l.pixel_w, l.pixel_h, fourcc, modifier,
                            );
                            continue;
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

        self.pending_submit = Some(PendingSubmit {
            fence,
            cb,
            textures: std::mem::take(&mut self.frame_textures),
            output_idx: out_idx,
            phys_w,
            phys_h,
            external,
        });
        if external {
            self.external_output_idx = (self.external_output_idx + 1) % self.external_outputs.len();
        } else {
            self.output_idx = (self.output_idx + 1) % self.output_images.len();
        }

        // Return the PREVIOUS frame's readback (or None on the first
        // frame).  The current frame will be read back next time.
        prev_result
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
            // Destroy external outputs.
            for ext in self.external_outputs.drain(..) {
                self.device.destroy_framebuffer(ext.framebuffer, None);
                self.device.destroy_image_view(ext.view, None);
                self.device.destroy_image(ext.image, None);
                self.device.free_memory(ext.memory, None);
            }
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
