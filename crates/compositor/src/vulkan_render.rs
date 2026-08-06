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
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use ash::vk;
use wayland_server::backend::ObjectId;

use super::imp::{EncodedFrame, ExternalOutputBuffer, PixelData, Surface};
use super::render::{GpuLayer, SurfaceMeta, collect_gpu_layers, to_physical};

// ===================================================================
// VulkanRenderer
// ===================================================================

/// How many consecutive `encode` failures mean a Vulkan Video session is not
/// coming back.  A handful of frames is enough to ride out a transient
/// refusal while still giving up long before a viewer would call the surface
/// broken — at 60 fps this is a fifth of a second.
const VULKAN_ENCODE_FAILURE_LIMIT: u32 = 12;

pub(crate) struct VulkanRenderer {
    /// Held only to keep libvulkan loaded, and deliberately never dropped --
    /// dropping it would `dlclose()` the library.  See the `Drop` impl.
    #[expect(dead_code)]
    entry: ManuallyDrop<ash::Entry>,
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
    /// Vulkan Video encoders, one per `(surface_id, client_id)`.  Each
    /// subscriber owns its own session so its GOP, keyframe cadence and
    /// quantizer are independent of every other viewer's.
    vulkan_encoders: HashMap<(u32, u64), crate::vulkan_encode::VulkanVideoEncoder>,
    /// Bitstreams produced by `vulkan_encoders` during the last render,
    /// awaiting collection by the compositor.  Kept out of the render
    /// return value because that function has a dozen early exits.
    pending_encoded_frames: Vec<EncodedFrame>,
    /// Whether the device supports VK_KHR_video_encode_queue + H.264 extensions.
    has_video_encode: bool,
    /// Whether the device supports VK_KHR_video_encode_av1 extension.
    has_video_encode_av1: bool,
    /// Whether the device supports DMA-BUF import/export extensions.
    has_dmabuf: bool,
    /// Set by an on-demand consumer (capture) to make the next retire
    /// publish the native BGRA even when an NV12 OPAQUE_FD target would
    /// otherwise claim that key.  The staging copy happens every frame
    /// regardless; what this re-enables is the `to_vec` that publishes it,
    /// which is one of the two per-frame full-surface copies the
    /// zero-copy path exists to avoid paying when nobody wants CPU pixels.
    publish_native_bgra_once: bool,
    has_external_memory_fd: bool,

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
    /// BGRA→NV24 (2-plane 4:4:4).  Shares `compute_image_pipeline_layout`.
    compute_nv24_pipeline: vk::Pipeline,
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

    /// Encoder-allocated output buffers imported as Vulkan render
    /// targets, keyed by `(surface_id, target_w, target_h)`.  Multiple
    /// distinct target sizes can coexist for one surface (one per-client
    /// encoder per size).  After compositing the surface at native size
    /// into `output_images`, the renderer `vkCmdBlitImage`s (LINEAR)
    /// from the native frame into each per-target external buffer so
    /// each per-client encoder consumes a downscaled frame zero-copy.
    /// The `usize` is the round-robin index per pool.
    external_outputs: HashMap<(u32, u32, u32), (Vec<ExternalOutput>, usize)>,

    /// NV12 output buffers for BGRA→NV12 compute conversion, keyed by
    /// `(surface_id, target_w, target_h)` to mirror `external_outputs`.
    /// The `usize` is the round-robin index.
    nv12_outputs: HashMap<(u32, u32, u32), (Vec<Nv12Output>, usize)>,
    /// NV12 buffers exported as OPAQUE_FD for the NVENC zero-copy path.
    ///
    /// Deliberately not `nv12_outputs`. That map is also where the
    /// compositor parks the encode image it owns for Vulkan Video, keyed
    /// the same `(surface, w, h)` way, and `create_nv12_outputs` destroys
    /// whatever is at the key before inserting — so sharing it let an
    /// NVENC target evict a live Vulkan Video encoder mid-stream and
    /// starve its client.
    nv12_opaque_outputs: HashMap<(u32, u32, u32), (Vec<Nv12Output>, usize)>,

    /// Keys in `nv12_outputs` the compositor allocated itself to feed a
    /// Vulkan Video encoder, as opposed to importing from VA-API, mapped to
    /// the `(is_444, codec)` the image was created against.  Tracked so we
    /// can tell "nothing is registered here" from "our own image is here",
    /// and so a session whose profile does not match is refused rather than
    /// handed an image the driver will reject.
    owned_encode_nv12: HashMap<(u32, u32, u32), (bool, u8)>,

    /// Consecutive `encode` failures per `(surface_id, client_id)`, reset by
    /// the first bitstream that comes back.
    vulkan_encode_failures: HashMap<(u32, u64), u32>,

    /// Sessions that have failed [`VULKAN_ENCODE_FAILURE_LIMIT`] times in a
    /// row, drained by the compositor so it can tell the server to fall back.
    vulkan_encode_giveups: Vec<(u32, u64)>,

    /// Server-allocated BGRA downscale targets, keyed by
    /// `(surface_id, target_w, target_h)`.  Populated for per-client
    /// encoders that don't import GBM buffers (NVENC, software h264,
    /// software AV1).  After compositing at native size, the renderer
    /// `vkCmdBlitImage`s (LINEAR) into each downscale target then
    /// copies the result into a CPU-mapped staging buffer.  `retire_pending`
    /// emits one `PixelData::Bgra` per downscale target so the per-client
    /// encoder consumes target-sized BGRA without a CPU resize step.
    downscale_outputs: HashMap<(u32, u32, u32), DownscaleOutput>,

    /// The native composite size each target above was sized against, keyed
    /// the same way.  The blits stretch the whole native frame across the
    /// whole target, so a target is only fillable while the composite still
    /// has the shape it was inscribed into: after a resize the composite
    /// moves first and the stale target would take a squashed picture.
    ///
    /// Recorded rather than re-derived because the inscription is the
    /// server's arithmetic, not ours — it rounds each axis down to even, so
    /// the target's aspect is never exactly the native one and no
    /// comparison of the two ratios can separate "rounded" from "stale"
    /// without a fudge factor.  Comparing what it was built for is exact.
    target_natives: HashMap<(u32, u32, u32), (u32, u32)>,

    /// Persistent texture cache keyed by Wayland surface ObjectId.
    /// Textures are created at surface commit time and reused across
    /// frames until the surface commits a new buffer or is destroyed.
    /// DMA-BUF entries are shared with `buffer_textures` (same `Arc`).
    surface_textures: HashMap<ObjectId, Arc<CachedSurfaceTexture>>,

    /// Zero-copy DMA-BUF imports keyed by wl_buffer ObjectId.  The
    /// imported VkImage references the client's buffer memory, so one
    /// import per wl_buffer is enough: clients rotate through a small
    /// buffer pool, and without this the import (VkImage + memory +
    /// view + descriptor set, several ms of driver CPU) reran on every
    /// commit.  Keyed by wl_buffer identity, never the dmabuf fd — the
    /// kernel recycles fd numbers, and a stale hit would hand the GPU
    /// freed memory.  Evicted when the client destroys the wl_buffer.
    buffer_textures: HashMap<ObjectId, Arc<CachedSurfaceTexture>>,

    /// Textures replaced by a surface commit but still potentially
    /// referenced by in-flight GPU work.  Freed when the pending
    /// submission completes (retire_pending / free_frame_textures).
    /// An entry destroys its Vulkan objects only when it holds the last
    /// `Arc` — otherwise the remaining holder (`surface_textures` /
    /// `buffer_textures`) re-pushes it on its own eviction.
    pending_destroy_textures: Vec<Arc<CachedSurfaceTexture>>,

    /// External / NV12 / downscale targets removed while a Vulkan submit
    /// may still reference them.  Freed once all tracked submits retire.
    pending_destroy_external_outputs: Vec<ExternalOutput>,
    pending_destroy_nv12_outputs: Vec<Nv12Output>,
    pending_destroy_downscale_outputs: Vec<DownscaleOutput>,
}

/// Encoder-allocated DMA-BUF imported as a Vulkan framebuffer.  The
/// size lives in the `external_outputs` key (`(sid, target_w,
/// target_h)`), not in this struct, so multiple distinct target sizes
/// can coexist for one surface.
struct ExternalOutput {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    va_surface_id: u32,
    va_display: usize,
    fourcc: u32,
    modifier: u64,
    stride: u32,
    /// Keep the DMA-BUF fd alive.
    _fd: Arc<OwnedFd>,
}

/// NV12 output for zero-copy encode.
/// Source of `Nv12Output::buf_id`.  Global rather than per-renderer so
/// ids stay unique across renderer teardown and recreation.
static NEXT_NV12_BUF_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_nv12_buf_id() -> u64 {
    NEXT_NV12_BUF_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// How an NV12 output's memory is exported, and therefore who can read it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Nv12Export {
    /// Not exported at all — compositor-owned memory read on this same
    /// device by Vulkan Video. No fd, no importer, no handle type.
    None,
    /// `dma_buf` — VA-API imports these, and they carry implicit fencing.
    DmaBuf,
    /// An NVIDIA-internal handle. The only importer is CUDA (NVENC):
    /// `cuImportExternalMemory` accepts `OPAQUE_FD` and refuses `dma_buf`.
    /// Carries no implicit fencing, so a consumer must be handed a sync_fd.
    OpaqueFd,
}

impl Nv12Export {
    /// Only meaningful for the exported variants; `create_nv12_outputs` is
    /// never called for `None`, whose memory stays on this device.
    fn handle_type(self) -> vk::ExternalMemoryHandleTypeFlags {
        match self {
            Nv12Export::None => vk::ExternalMemoryHandleTypeFlags::empty(),
            Nv12Export::DmaBuf => vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            Nv12Export::OpaqueFd => vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD,
        }
    }
}

struct Nv12Output {
    /// The DMA-BUF this plane set was imported from, or the OPAQUE_FD it was
    /// exported as, kept alive for as long as the image is.  `None` for the
    /// compositor's own encode image, which owns device-local memory and is
    /// never handed to another API.
    fd: Option<Arc<OwnedFd>>,
    /// Process-unique id for this allocation.  Consumers cache GPU-side
    /// registrations against it: an fd number alone is not safe to key on,
    /// since closing a buffer frees its number for the next one to reuse,
    /// and a stale cache hit would point NVENC at freed VRAM.
    buf_id: u64,
    descriptor_set: vk::DescriptorSet,
    /// NV12 surface dimensions (encoder-padded, may be larger than source).
    width: u32,
    height: u32,
    /// Full-resolution chroma (`G8_B8R8_2PLANE_444_UNORM`) rather than
    /// subsampled NV12.  Decides which compute shader fills the planes.
    is_444: bool,
    kind: Nv12OutputKind,
    /// Which export `fd` came from — decides which `PixelData` variant this
    /// becomes, and whether the consumer needs an explicit sync_fd.
    export: Nv12Export,
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
        /// Full-image COLOR view for Vulkan Video encode source.  Belongs
        /// to `encode_image` when that is present, to `image` otherwise.
        encode_view: Option<vk::ImageView>,
        /// A separate `VIDEO_ENCODE_SRC` image the storage image is copied
        /// into after each convert.  `STORAGE | VIDEO_ENCODE_SRC` on a
        /// single image is not a supported combination (VUID 02251) — the
        /// NVIDIA driver happens to tolerate it at 4:2:0 but rejects the
        /// encode outright at 4:4:4 — so the compute shader writes a plain
        /// storage image and the encode session reads this one.
        encode_image: Option<(vk::Image, vk::DeviceMemory)>,
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
    /// Self-allocated output image index used for the native composite
    /// (and the staging readback).
    self_output_idx: usize,
    /// Native (compositor) frame size — the size we composited at.
    phys_w: u32,
    phys_h: u32,
    /// Surface id used to look up downscale outputs at retire time.
    surface_id: u32,
    /// Per-target sizes whose downscale staging buffers contain valid
    /// pixels that should be emitted as `PixelData::Bgra` once the
    /// fence signals.
    downscale_targets: Vec<(u32, u32)>,
    /// Toplevel surface_id this submission was rendered for, so async
    /// retirement can attribute the pixels to the correct surface.
    toplevel_sid: u16,
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

/// Server-allocated BGRA target sized at `(width, height)` for a
/// per-client encoder that doesn't import GBM buffers.  The render
/// loop blits the native composite into `image` (LINEAR downscale)
/// and copies the result into `staging_*` for CPU readback by the
/// per-client `SurfaceEncoder`.
struct DownscaleOutput {
    image: vk::Image,
    memory: vk::DeviceMemory,
    width: u32,
    height: u32,
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
/// 4:4:4 twin of the above: same two-plane shape and descriptor layout, but
/// full-resolution chroma.  Used for `G8_B8R8_2PLANE_444_UNORM` encode
/// sources.
static NV24_IMAGE_COMP_SPV: &[u8] = include_bytes!("shaders/bgra_to_nv24_image.comp.spv");

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

/// The layer rectangle `(x, y, w, h)` as a scissor, clipped to the render
/// area.  Vulkan rejects a scissor that leaves the framebuffer, and a layer
/// can start left of or above the origin (a toplevel with client-side
/// decorations is shifted negative so only its geometry area shows) or run
/// past the far edge (the composite follows the pane, which can be smaller
/// than the window still painting into it).
fn clamped_scissor(x: i32, y: i32, w: u32, h: u32, fb_w: u32, fb_h: u32) -> vk::Rect2D {
    let x0 = x.max(0).min(fb_w as i32);
    let y0 = y.max(0).min(fb_h as i32);
    let x1 = x.saturating_add(w as i32).clamp(x0, fb_w as i32);
    let y1 = y.saturating_add(h as i32).clamp(y0, fb_h as i32);
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        },
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
        // Exporting memory as OPAQUE_FD needs only these two — no dma_buf,
        // no DRM modifiers.  Probed apart from `has_dmabuf` because the
        // NVENC zero-copy path must survive on a host with no dma_buf
        // support at all, which is precisely where it earns its keep.
        let external_memory_fd_extensions: &[&std::ffi::CStr] = &[
            ash::khr::external_memory::NAME,
            ash::khr::external_memory_fd::NAME,
        ];
        let has_external_memory_fd = external_memory_fd_extensions
            .iter()
            .all(|e| ext_names_all.contains(e));
        let mut device_extensions: Vec<*const std::ffi::c_char> = Vec::new();
        if has_dmabuf {
            device_extensions.extend(dmabuf_extensions.iter().map(|e| e.as_ptr()));
        } else if has_external_memory_fd {
            device_extensions.extend(external_memory_fd_extensions.iter().map(|e| e.as_ptr()));
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

        // Push constants: src_width, src_height, y_stride, uv_offset,
        // enc_width, enc_height (6 × u32 = 24 bytes).
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

        // Push constants: src_width, src_height, enc_width, enc_height
        // (4 × u32 = 16 bytes).
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

        // Same layout, full-resolution chroma — see NV24_IMAGE_COMP_SPV.
        let comp_nv24_code = Self::spirv_from_bytes(NV24_IMAGE_COMP_SPV)?;
        let comp_nv24_shader_info = vk::ShaderModuleCreateInfo::default().code(&comp_nv24_code);
        let comp_nv24_mod = unsafe {
            device
                .create_shader_module(&comp_nv24_shader_info, None)
                .ok()?
        };
        let comp_nv24_stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(comp_nv24_mod)
            .name(c"main");
        let compute_nv24_pipeline = unsafe {
            device
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(comp_nv24_stage)
                        .layout(compute_image_pipeline_layout)],
                    None,
                )
                .ok()?[0]
        };
        unsafe {
            device.destroy_shader_module(comp_nv24_mod, None);
        }

        // -----------------------------------------------------------
        // BGRA→I420 compute pipeline — planar YUV for software encoders
        // -----------------------------------------------------------
        // Report the device Vulkan actually picked alongside the one that
        // was requested — find_device falls back to a different GPU when
        // nothing reports the node, and that is worth seeing in the log.
        let dev_props = unsafe { instance.get_physical_device_properties(physical_device) };
        let dev_name = unsafe { std::ffi::CStr::from_ptr(dev_props.device_name.as_ptr()) };
        eprintln!(
            "[vulkan-render] initialized: {} (requested {drm_device})",
            dev_name.to_string_lossy()
        );

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
            entry: ManuallyDrop::new(entry),
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
            pending_encoded_frames: Vec::new(),
            has_video_encode,
            has_video_encode_av1,
            has_dmabuf,
            publish_native_bgra_once: false,
            has_external_memory_fd,
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
            compute_nv24_pipeline,
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
            nv12_opaque_outputs: HashMap::new(),
            owned_encode_nv12: HashMap::new(),
            vulkan_encode_failures: HashMap::new(),
            vulkan_encode_giveups: Vec::new(),
            downscale_outputs: HashMap::new(),
            target_natives: HashMap::new(),
            surface_textures: HashMap::new(),
            buffer_textures: HashMap::new(),
            pending_destroy_textures: Vec::new(),
            pending_destroy_external_outputs: Vec::new(),
            pending_destroy_nv12_outputs: Vec::new(),
            pending_destroy_downscale_outputs: Vec::new(),
        })
    }

    /// (major, minor) of the device node at `path`.
    fn drm_node_ids(path: &str) -> Option<(i64, i64)> {
        use std::os::linux::fs::MetadataExt;
        let rdev = std::fs::metadata(path).ok()?.st_rdev();
        Some((libc::major(rdev) as i64, libc::minor(rdev) as i64))
    }

    /// Whether `pd` is the GPU behind the DRM node with this major/minor.
    ///
    /// Matches either node: callers hand us a render node, but a device
    /// that only exposes a primary node should still match on it.
    fn is_drm_node(
        instance: &ash::Instance,
        pd: vk::PhysicalDevice,
        major: i64,
        minor: i64,
    ) -> bool {
        // The struct may only be chained when the device supports the
        // extension, so check before querying.
        let supported = unsafe { instance.enumerate_device_extension_properties(pd) }
            .unwrap_or_default()
            .iter()
            .any(|p| {
                (unsafe { std::ffi::CStr::from_ptr(p.extension_name.as_ptr()) })
                    == ash::ext::physical_device_drm::NAME
            });
        if !supported {
            return false;
        }

        let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
        unsafe { instance.get_physical_device_properties2(pd, &mut props2) };

        (drm.has_render == vk::TRUE && drm.render_major == major && drm.render_minor == minor)
            || (drm.has_primary == vk::TRUE
                && drm.primary_major == major
                && drm.primary_minor == minor)
    }

    /// First graphics queue family, and first video-encode family if any.
    fn queue_families(
        instance: &ash::Instance,
        pd: vk::PhysicalDevice,
    ) -> Option<(u32, Option<u32>)> {
        let props = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        let mut graphics_qf = None;
        let mut video_encode_qf = None;
        for (i, qf) in props.iter().enumerate() {
            if qf.queue_flags.contains(vk::QueueFlags::GRAPHICS) && graphics_qf.is_none() {
                graphics_qf = Some(i as u32);
            }
            // VIDEO_ENCODE_KHR = 0x40
            if qf.queue_flags.contains(vk::QueueFlags::from_raw(0x40)) && video_encode_qf.is_none()
            {
                video_encode_qf = Some(i as u32);
            }
        }
        graphics_qf.map(|g| (g, video_encode_qf))
    }

    /// Pick the physical device backing `drm_device`.
    ///
    /// Taking the first device with a graphics queue is a trap on hybrid
    /// machines: the loader can enumerate a small integrated GPU ahead of
    /// the discrete card the caller asked for, and we then composite on
    /// the wrong GPU — exhausting its carveout and losing the zero-copy
    /// DMA-BUF path into the encoder, silently.  Match the node, and say
    /// so loudly when we can't.
    fn find_device(
        instance: &ash::Instance,
        devices: &[vk::PhysicalDevice],
        drm_device: &str,
    ) -> Option<(vk::PhysicalDevice, u32, Option<u32>)> {
        let want = Self::drm_node_ids(drm_device);
        if want.is_none() {
            eprintln!("[vulkan-render] cannot stat {drm_device}, falling back to first device");
        }

        let mut fallback = None;
        for &pd in devices {
            let Some((gqf, vqf)) = Self::queue_families(instance, pd) else {
                continue;
            };
            if let Some((major, minor)) = want
                && Self::is_drm_node(instance, pd, major, minor)
            {
                return Some((pd, gqf, vqf));
            }
            fallback.get_or_insert((pd, gqf, vqf));
        }

        if want.is_some() {
            eprintln!(
                "[vulkan-render] WARNING: no Vulkan device reports DRM node {drm_device}; \
                 falling back to another GPU — expect degraded performance"
            );
        }
        fallback
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

    /// Memory type for staging buffers the CPU reads back from
    /// (retire_pending's memcpy).  HOST_VISIBLE|HOST_COHERENT alone
    /// lands in write-combined memory on discrete GPUs (it's the
    /// lowest-index match), and CPU reads from write-combined memory
    /// bypass the cache — the readback memcpy runs ~10-100x slower
    /// and can pin a core.  Prefer HOST_CACHED; fall back for devices
    /// that don't expose a cached host-visible type.
    fn find_readback_memory_type(&self, type_bits: u32) -> Option<u32> {
        self.find_memory_type(
            type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE
                | vk::MemoryPropertyFlags::HOST_COHERENT
                | vk::MemoryPropertyFlags::HOST_CACHED,
        )
        .or_else(|| {
            self.find_memory_type(
                type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
        })
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

    /// Ask the next retire to publish the native BGRA even if an NV12
    /// OPAQUE_FD target would otherwise claim that key.  For consumers that
    /// need CPU pixels and can say so — `surface capture` — rather than
    /// keeping the readback published every frame on the chance one comes.
    pub(crate) fn request_native_bgra(&mut self) {
        self.publish_native_bgra_once = true;
    }

    // ---------------------------------------------------------------
    // Vulkan Video encoder management
    // ---------------------------------------------------------------

    /// Create a Vulkan Video encoder for one `(surface, client)` pair.
    /// `codec`: 0x01 = H.264, 0x02 = AV1.
    ///
    /// Returns `false` when no encoder could be created — including when the
    /// driver runs out of concurrent video sessions — so the caller can tell
    /// the server to fall back to a server-side encoder instead of leaving
    /// that client waiting for a bitstream that will never arrive.
    pub(crate) fn create_vulkan_encoder(
        &mut self,
        surface_id: u32,
        client_id: u64,
        codec: u8,
        qp: u8,
        w: u32,
        h: u32,
        // 4:4:4 rather than 4:2:0.  Device-dependent — the caps query
        // refuses it on hardware that cannot, and the caller falls back.
        is_444: bool,
    ) -> bool {
        if !self.has_video_encode {
            eprintln!("[vulkan-render] cannot create vulkan encoder: video encode not available");
            return false;
        }
        let enc_qf = match self.video_encode_queue_family {
            Some(qf) => qf,
            None => return false,
        };

        // Remove existing encoder if any.
        if let Some(mut old) = self.vulkan_encoders.remove(&(surface_id, client_id))
            && let Some(ref vfns) = self.video_fns
        {
            unsafe { old.destroy(&self.device, vfns) };
        }

        let codec_name = match codec {
            0x02 => "av1",
            _ => "h264",
        };

        // Build the session first: its capability query is the real test of
        // whether this device can encode the requested chroma, and it costs
        // nothing to discover that before allocating an image for it.
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
                return false;
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
                    is_444,
                )
            },
        };

        // The encoder reads its source from an image, and until now the only
        // source of one was a VA-API-exported DMA-BUF.  Give this surface an
        // image of our own if nothing has already registered one at this
        // size, so the encode path does not depend on VA-API being in play.
        //
        // It has to match the session exactly: the image is created against a
        // profile list, so an H.264 image cannot be read by an AV1 session,
        // and a 4:4:4 session reads `G8_B8R8_2PLANE_444_UNORM` rather than a
        // subsampled NV12 — both are format mismatches, not quality
        // compromises.  The image is therefore tagged with what it was built
        // for.
        if encoder.is_some() {
            let key = (surface_id, w, h);
            let want = (is_444, codec);
            let owned = self.owned_encode_nv12.get(&key).copied();
            let usable = match owned {
                // Ours and built for this session: reuse.
                Some(have) => have == want,
                // Someone else's (a VA-API import) already sits here; it is
                // NV12 4:2:0 H.264-compatible and predates us.
                None => self.nv12_outputs.contains_key(&key) && want == (false, 0x01),
            };
            if !usable {
                // Replacing is only safe when no other session is reading it.
                // A surface with two subscribers on different codecs keeps the
                // first on Vulkan and lets the second fall back, rather than
                // pulling the image out from under a live encoder.
                let in_use_by_others = self
                    .vulkan_encoders
                    .keys()
                    .any(|&(sid, cid)| sid == surface_id && cid != client_id);
                if owned.is_some() && in_use_by_others {
                    eprintln!(
                        "[vulkan-render] surface {surface_id} already encodes with a different profile; refusing this session",
                    );
                    if let Some(mut enc) = encoder
                        && let Some(ref vfns) = self.video_fns
                    {
                        unsafe { enc.destroy(&self.device, vfns) };
                    }
                    return false;
                }
                if owned.is_some() {
                    self.destroy_nv12_outputs_for_target(surface_id, w, h);
                }
                if !self.nv12_outputs.contains_key(&key) {
                    match self.create_nv12_encode_image(w, h, is_444, codec) {
                        Some(nv12) => {
                            self.nv12_outputs.insert(key, (vec![nv12], 0));
                            self.owned_encode_nv12.insert(key, want);
                            // This surface now encodes on the compositor's
                            // own GPU, so any NVENC zero-copy target left
                            // over from before is computing into a buffer
                            // nothing reads.
                            self.destroy_nv12_outputs_in(Nv12Export::OpaqueFd, surface_id, w, h);
                        }
                        None => {
                            eprintln!(
                                "[vulkan-render] no encode image for surface {surface_id} {w}x{h}; refusing vulkan encoder",
                            );
                            if let Some(mut enc) = encoder
                                && let Some(ref vfns) = self.video_fns
                            {
                                unsafe { enc.destroy(&self.device, vfns) };
                            }
                            return false;
                        }
                    }
                }
            }
        }

        match encoder {
            Some(enc) => {
                eprintln!(
                    "[vulkan-render] created vulkan {codec_name} encoder for surface {surface_id} client {client_id} {w}x{h} qp={qp}",
                );
                self.vulkan_encoders.insert((surface_id, client_id), enc);
                true
            }
            None => {
                eprintln!(
                    "[vulkan-render] failed to create vulkan {codec_name} encoder for surface {surface_id} client {client_id}",
                );
                false
            }
        }
    }

    /// Retarget one client's encoder quantizer without rebuilding it.
    /// Returns `false` when that client has no encoder on this surface.
    pub(crate) fn set_vulkan_encoder_qp(
        &mut self,
        surface_id: u32,
        client_id: u64,
        qp: u8,
    ) -> bool {
        match self.vulkan_encoders.get_mut(&(surface_id, client_id)) {
            Some(enc) => {
                enc.set_qp(qp);
                true
            }
            None => false,
        }
    }

    /// Request the next frame for one client's encoder to be a keyframe.
    pub(crate) fn request_encoder_keyframe(&mut self, surface_id: u32, client_id: u64) {
        if let Some(enc) = self.vulkan_encoders.get_mut(&(surface_id, client_id)) {
            enc.request_idr();
        }
    }

    /// Destroy vulkan encoders for a surface: one client's when `client_id`
    /// is `Some`, every client's when it is `None` (the surface itself went
    /// away or was resized out from under all of them).
    pub(crate) fn destroy_vulkan_encoder(&mut self, surface_id: u32, client_id: Option<u64>) {
        let keys: Vec<(u32, u64)> = self
            .vulkan_encoders
            .keys()
            .filter(|&&(sid, cid)| sid == surface_id && client_id.is_none_or(|c| c == cid))
            .copied()
            .collect();
        for key in keys {
            if let Some(mut enc) = self.vulkan_encoders.remove(&key)
                && let Some(ref vfns) = self.video_fns
            {
                unsafe { enc.destroy(&self.device, vfns) };
            }
            // A rebuilt session starts from a clean slate; carrying the old
            // count over would retire its replacement early.
            self.vulkan_encode_failures.remove(&key);
            self.vulkan_encode_giveups.retain(|k| *k != key);
        }
        // Never hand a bitstream to a client whose encoder has just gone
        // away; the server would credit it against a subscription that no
        // longer exists.
        self.pending_encoded_frames.retain(|f| {
            !(f.surface_id as u32 == surface_id && client_id.is_none_or(|c| c == f.client_id))
        });
        // Our own encode image exists only to feed those encoders.  Once the
        // last one on this surface is gone it is dead weight — several MB of
        // device-local memory per surface — so release it.  Anything VA-API
        // imported is left alone: a server-side encoder may still be reading
        // it.
        if !self
            .vulkan_encoders
            .keys()
            .any(|&(sid, _)| sid == surface_id)
        {
            let owned: Vec<(u32, u32, u32)> = self
                .owned_encode_nv12
                .keys()
                .filter(|k| k.0 == surface_id)
                .copied()
                .collect();
            for k in owned {
                self.destroy_nv12_outputs_for_target(k.0, k.1, k.2);
            }
        }
    }

    /// Take the bitstreams produced since the last call.
    pub(crate) fn take_encoded_frames(&mut self) -> Vec<EncodedFrame> {
        std::mem::take(&mut self.pending_encoded_frames)
    }

    /// Take the sessions that have stopped producing bitstreams, so the
    /// caller can tell the server to fall back to a server-side encoder.
    /// Draining is what makes the report happen once rather than every frame.
    pub(crate) fn take_encode_giveups(&mut self) -> Vec<(u32, u64)> {
        std::mem::take(&mut self.vulkan_encode_giveups)
    }

    // ---------------------------------------------------------------
    // External output buffers (VA-API zero-copy)
    // ---------------------------------------------------------------

    /// `native` is the composite size `(target_w, target_h)` was inscribed
    /// into, so the render loop can tell a target that still fits the
    /// composite from one left behind by a resize.  See {@link target_natives}.
    pub(crate) fn set_external_output_buffers(
        &mut self,
        surface_id: u32,
        target_w: u32,
        target_h: u32,
        native: (u32, u32),
        buffers: Vec<ExternalOutputBuffer>,
    ) {
        if buffers.is_empty() {
            self.destroy_external_outputs_for_target(surface_id, target_w, target_h);
            return;
        }
        self.target_natives
            .insert((surface_id, target_w, target_h), native);
        if !self.has_dmabuf {
            return;
        }
        // Import each encoder-allocated DMA-BUF as a Vulkan render target.
        // The encoder owns the buffer; we borrow it for compositing.
        // After rendering, we return PixelData::Nv12DmaBuf and the encoder
        // encodes directly — zero copies, zero bus crossings.
        self.destroy_external_outputs_for_target(surface_id, target_w, target_h);
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
                "[vulkan-render] {} external output buffers imported for surface {surface_id} target {target_w}x{target_h} (buffer {}x{})",
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
                self.create_nv12_outputs_from_fds(surface_id, target_w, target_h, &nv12_fds);
            } else {
                self.create_nv12_outputs(
                    surface_id,
                    target_w,
                    target_h,
                    buffers[0].width,
                    buffers[0].height,
                    // VA-API is the consumer here; it imports dma_bufs.
                    Nv12Export::DmaBuf,
                );
            }
        }
        self.external_outputs
            .insert((surface_id, target_w, target_h), (imported, 0));
    }

    /// Destroy the external + NV12 buffers for a single (sid, target) pair.
    fn destroy_external_outputs_for_target(
        &mut self,
        surface_id: u32,
        target_w: u32,
        target_h: u32,
    ) {
        if let Some((exts, _)) = self
            .external_outputs
            .remove(&(surface_id, target_w, target_h))
        {
            self.defer_or_destroy_external_outputs(exts);
        }
        self.destroy_nv12_outputs_for_target(surface_id, target_w, target_h);
    }

    /// Destroy every external + NV12 buffer pool belonging to a surface,
    /// regardless of target size.  Used on full surface teardown.
    pub(crate) fn destroy_external_outputs_for_surface(&mut self, surface_id: u32) {
        let keys: Vec<(u32, u32, u32)> = self
            .external_outputs
            .keys()
            .filter(|k| k.0 == surface_id)
            .copied()
            .collect();
        for k in keys {
            if let Some((exts, _)) = self.external_outputs.remove(&k) {
                self.defer_or_destroy_external_outputs(exts);
            }
        }
        self.destroy_nv12_outputs_for_surface(surface_id);
        self.destroy_downscale_outputs_for_surface(surface_id);
    }

    fn destroy_all_external_outputs(&mut self) {
        let all: Vec<Vec<ExternalOutput>> = self
            .external_outputs
            .drain()
            .map(|(_, (exts, _))| exts)
            .collect();
        for exts in all {
            self.defer_or_destroy_external_outputs(exts);
        }
        self.destroy_all_nv12_outputs();
        self.destroy_all_downscale_outputs();
    }

    // ---------------------------------------------------------------
    // Server-allocated BGRA downscale targets
    // ---------------------------------------------------------------

    /// Allocate a BGRA downscale target sized at `(target_w, target_h)`
    /// for `surface_id`.  Used by per-client encoders that don't import
    /// GBM buffers (NVENC, software).  Re-registering an identical
    /// target is a no-op so callers can register idempotently.
    ///
    /// `native` is the composite size the target was inscribed into.  See
    /// {@link target_natives}.
    pub(crate) fn register_downscale_target(
        &mut self,
        surface_id: u32,
        target_w: u32,
        target_h: u32,
        native: (u32, u32),
        want_nv12_opaque: bool,
    ) {
        // A Vulkan Video encoder on this surface encodes from its own image,
        // so nothing would ever read the NV12 buffer we were asked for.
        let want_nv12_opaque = want_nv12_opaque && !self.vulkan_video_owns(surface_id);
        let key = (surface_id, target_w, target_h);
        // Recorded before the early return: the same target size can be
        // asked for again against a *different* native — a surface that
        // shrinks and grows back lands on sizes it has held before — and
        // the buffer is reusable while the stale native it carries is not.
        self.target_natives.insert(key, native);
        if self.downscale_outputs.contains_key(&key) {
            // The BGRA buffer is reusable, but "can everyone reading this
            // target take NV12" is not a property of the buffer — it is a
            // property of the current subscribers, and a software encoder
            // joining an NVENC one at the same size flips it. The server
            // re-registers to say so. Returning early here would freeze the
            // first subscriber's answer for the target's whole life and
            // hand the newcomer GPU-only memory it cannot map.
            let has_nv12 = self
                .nv12_opaque_slot(surface_id, target_w, target_h)
                .is_some();
            if want_nv12_opaque && !has_nv12 {
                self.create_nv12_outputs(
                    surface_id,
                    target_w,
                    target_h,
                    target_w,
                    target_h,
                    Nv12Export::OpaqueFd,
                );
            } else if !want_nv12_opaque && has_nv12 {
                eprintln!(
                    "[vulkan-render] sid {surface_id} {target_w}x{target_h}: dropping NV12 \
                     opaque-fd target, a subscriber needs CPU pixels",
                );
                self.destroy_nv12_outputs_for_target(surface_id, target_w, target_h);
            }
            return;
        }
        let Some(out) = self.create_downscale_output(target_w, target_h) else {
            eprintln!(
                "[vulkan-render] failed to allocate downscale target {target_w}x{target_h} for sid {surface_id}",
            );
            return;
        };
        self.downscale_outputs.insert(key, out);

        // The BGRA image above is still the compute pass's source, so it is
        // allocated either way; what the NV12 buffer removes is everything
        // downstream of it — the image→staging copy and the `to_vec()` that
        // publishes it. Best-effort: a failure here leaves the plain BGRA
        // target registered and the caller simply keeps its old path.
        if want_nv12_opaque {
            self.create_nv12_outputs(
                surface_id,
                target_w,
                target_h,
                target_w,
                target_h,
                Nv12Export::OpaqueFd,
            );
            let ok = self
                .nv12_opaque_outputs
                .get(&key)
                .is_some_and(|(v, _)| !v.is_empty());
            eprintln!(
                "[vulkan-render] registered downscale target sid {surface_id} {target_w}x{target_h} (nv12 opaque-fd: {})",
                if ok { "yes" } else { "FAILED, using BGRA" },
            );
            return;
        }
        eprintln!(
            "[vulkan-render] registered downscale target sid {surface_id} {target_w}x{target_h}",
        );
    }

    /// Record that an already-registered target is the right inscription of
    /// `native`, without touching its buffers.  No-op for a target that is
    /// not registered: a restamp can race the teardown of the encoder it
    /// belongs to, and stamping buffers that are gone would only leave an
    /// entry for `render_tree_sized` to prune.
    pub(crate) fn restamp_target(
        &mut self,
        surface_id: u32,
        target_w: u32,
        target_h: u32,
        native: (u32, u32),
    ) {
        let key = (surface_id, target_w, target_h);
        if self.external_outputs.contains_key(&key) || self.downscale_outputs.contains_key(&key) {
            self.target_natives.insert(key, native);
        }
    }

    /// Tear down a single downscale target, if registered.
    pub(crate) fn clear_downscale_target(&mut self, surface_id: u32, target_w: u32, target_h: u32) {
        if let Some(out) = self
            .downscale_outputs
            .remove(&(surface_id, target_w, target_h))
        {
            self.defer_or_destroy_downscale_output(out);
        }
        // The OPAQUE_FD NV12 buffers are filled from this target's blit, so
        // they go with it.  Left behind, the key keeps telling
        // `retire_pending` that an NV12 target "has already published this
        // frame" whenever the composite returns to this exact size — but
        // nothing fills the NV12 side any more, so nothing is published at
        // all.  The server then never sees a SurfaceCommit *or* the
        // SurfaceResized derived from it: its native size freezes at the old
        // value, the encode loop waits for pixels that cannot arrive, and
        // the surface streams black until some resize lands on a fresh size.
        // Only the OPAQUE_FD map: `nv12_outputs` at this key can be a
        // compositor-owned Vulkan Video encode image, which is not ours to
        // destroy here.
        if let Some((nv12s, _)) = self
            .nv12_opaque_outputs
            .remove(&(surface_id, target_w, target_h))
        {
            self.destroy_nv12_vec(nv12s);
        }
    }

    fn destroy_downscale_outputs_for_surface(&mut self, surface_id: u32) {
        let keys: Vec<(u32, u32, u32)> = self
            .downscale_outputs
            .keys()
            .filter(|k| k.0 == surface_id)
            .copied()
            .collect();
        for k in keys {
            if let Some(out) = self.downscale_outputs.remove(&k) {
                self.defer_or_destroy_downscale_output(out);
            }
        }
    }

    fn destroy_all_downscale_outputs(&mut self) {
        let outs: Vec<DownscaleOutput> = self.downscale_outputs.drain().map(|(_, v)| v).collect();
        for out in outs {
            self.defer_or_destroy_downscale_output(out);
        }
    }

    fn defer_or_destroy_external_outputs(&mut self, exts: Vec<ExternalOutput>) {
        if self.has_tracked_in_flight_work() {
            self.pending_destroy_external_outputs.extend(exts);
        } else {
            for ext in exts {
                self.destroy_external_output(ext);
            }
        }
    }

    fn destroy_external_output(&self, ext: ExternalOutput) {
        unsafe {
            self.device.destroy_framebuffer(ext.framebuffer, None);
            self.device.destroy_image_view(ext.view, None);
            self.device.destroy_image(ext.image, None);
            self.device.free_memory(ext.memory, None);
        }
    }

    fn defer_or_destroy_downscale_output(&mut self, out: DownscaleOutput) {
        if self.has_tracked_in_flight_work() {
            self.pending_destroy_downscale_outputs.push(out);
        } else {
            self.destroy_downscale_output(out);
        }
    }

    fn destroy_downscale_output(&self, out: DownscaleOutput) {
        unsafe {
            self.device.unmap_memory(out.staging_mem);
            self.device.destroy_buffer(out.staging_buf, None);
            self.device.free_memory(out.staging_mem, None);
            self.device.destroy_image(out.image, None);
            self.device.free_memory(out.memory, None);
        }
    }

    fn create_downscale_output(&self, w: u32, h: u32) -> Option<DownscaleOutput> {
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
            // STORAGE + MUTABLE_FORMAT so the BGRA→NV12 compute shader can
            // read this image through an R8G8B8A8 storage view, matching
            // the native output image. Without them that view is invalid,
            // `dispatch_nv12_compute` bails before writing anything, and
            // the encoder gets a buffer nobody filled — which reaches the
            // viewer as a black picture, not as an error.
            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
            .usage(
                vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::STORAGE,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe {
            self.device
                .create_image(&image_info, None)
                .inspect_err(|&e| {
                    eprintln!("[create_downscale_output] create_image failed: {e}");
                })
                .ok()?
        };
        let mem_reqs = unsafe { self.device.get_image_memory_requirements(image) };
        let mem_type = self.find_memory_type(
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mem_type);
        let memory = unsafe {
            match self.device.allocate_memory(&alloc, None) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[create_downscale_output] allocate_memory failed: {e}");
                    self.device.destroy_image(image, None);
                    return None;
                }
            }
        };
        if unsafe { self.device.bind_image_memory(image, memory, 0) }.is_err() {
            unsafe {
                self.device.destroy_image(image, None);
                self.device.free_memory(memory, None);
            }
            return None;
        }

        // Staging buffer for CPU readback.
        let staging_size = (w as u64) * (h as u64) * 4;
        let buf_info = vk::BufferCreateInfo::default()
            .size(staging_size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let staging_buf = unsafe {
            match self.device.create_buffer(&buf_info, None) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[create_downscale_output] create_buffer failed: {e}");
                    self.device.destroy_image(image, None);
                    self.device.free_memory(memory, None);
                    return None;
                }
            }
        };
        let buf_reqs = unsafe { self.device.get_buffer_memory_requirements(staging_buf) };
        let buf_mem_type = self.find_readback_memory_type(buf_reqs.memory_type_bits);
        let Some(buf_mem_type) = buf_mem_type else {
            unsafe {
                self.device.destroy_buffer(staging_buf, None);
                self.device.destroy_image(image, None);
                self.device.free_memory(memory, None);
            }
            return None;
        };
        let buf_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(buf_reqs.size)
            .memory_type_index(buf_mem_type);
        let staging_mem = unsafe {
            match self.device.allocate_memory(&buf_alloc, None) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[create_downscale_output] allocate_memory(staging) failed: {e}");
                    self.device.destroy_buffer(staging_buf, None);
                    self.device.destroy_image(image, None);
                    self.device.free_memory(memory, None);
                    return None;
                }
            }
        };
        if unsafe { self.device.bind_buffer_memory(staging_buf, staging_mem, 0) }.is_err() {
            unsafe {
                self.device.destroy_buffer(staging_buf, None);
                self.device.free_memory(staging_mem, None);
                self.device.destroy_image(image, None);
                self.device.free_memory(memory, None);
            }
            return None;
        }
        let staging_ptr = unsafe {
            match self.device.map_memory(
                staging_mem,
                0,
                vk::WHOLE_SIZE,
                vk::MemoryMapFlags::empty(),
            ) {
                Ok(p) => p as *mut u8,
                Err(e) => {
                    eprintln!("[create_downscale_output] map_memory failed: {e}");
                    self.device.destroy_buffer(staging_buf, None);
                    self.device.free_memory(staging_mem, None);
                    self.device.destroy_image(image, None);
                    self.device.free_memory(memory, None);
                    return None;
                }
            }
        };

        Some(DownscaleOutput {
            image,
            memory,
            width: w,
            height: h,
            staging_buf,
            staging_mem,
            staging_ptr,
        })
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
                    | vk::ImageUsageFlags::TRANSFER_DST
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
        // A pending self-allocated submit references the current output
        // images.  If we destroy them now, retire_pending will read freed
        // memory (or hit a size-mismatch).  Wait for the fence and retire
        // the staging data before recreating.
        if let Some(pending) = self.pending_submit.take() {
            unsafe {
                let _ = self.device.wait_for_fences(
                    &[pending.fence],
                    true,
                    1_000_000_000, // 1s
                );
            }
            // Discard the retired frame — its dimensions are about to be
            // stale anyway.  retire_pending still frees the fence / cb /
            // textures.
            let _ = self.retire_pending(pending);
            self.free_frame_textures();
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
            if self.has_tracked_in_flight_work() {
                self.pending_destroy_nv12_outputs.push(n);
            } else {
                self.destroy_nv12_output(n);
            }
        }
    }

    fn destroy_nv12_output(&self, n: Nv12Output) {
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
                    encode_image,
                } => {
                    if let Some(ev) = encode_view {
                        self.device.destroy_image_view(ev, None);
                    }
                    if let Some((ei, em)) = encode_image {
                        self.device.destroy_image(ei, None);
                        self.device.free_memory(em, None);
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

    /// Destroy only the outputs belonging to `export`'s map.
    fn destroy_nv12_outputs_in(
        &mut self,
        export: Nv12Export,
        surface_id: u32,
        target_w: u32,
        target_h: u32,
    ) {
        let key = (surface_id, target_w, target_h);
        if let Some((nv12s, _)) = self.nv12_dest_mut(export).remove(&key) {
            self.destroy_nv12_vec(nv12s);
        }
        // `nv12_outputs` is also where a compositor-owned encode image
        // lives, and `owned_encode_nv12` names it. Dropping the image
        // without the name leaves a key that outlives what it points at,
        // which makes `create_vulkan_encoder` skip allocating a
        // replacement — the surface then encodes from nothing.
        if !matches!(export, Nv12Export::OpaqueFd) {
            self.owned_encode_nv12.remove(&key);
        }
    }

    fn destroy_nv12_outputs_for_target(&mut self, surface_id: u32, target_w: u32, target_h: u32) {
        if let Some((nv12s, _)) = self
            .nv12_opaque_outputs
            .remove(&(surface_id, target_w, target_h))
        {
            self.destroy_nv12_vec(nv12s);
        }
        if let Some((nv12s, _)) = self.nv12_outputs.remove(&(surface_id, target_w, target_h)) {
            self.destroy_nv12_vec(nv12s);
        }
        // Every removal funnels through here, so clearing ownership here
        // keeps the set from outliving the images it names — a stale key
        // would make `create_vulkan_encoder` skip allocating a replacement.
        self.owned_encode_nv12
            .remove(&(surface_id, target_w, target_h));
    }

    fn destroy_nv12_outputs_for_surface(&mut self, surface_id: u32) {
        let keys: Vec<(u32, u32, u32)> = self
            .nv12_outputs
            .keys()
            .chain(self.nv12_opaque_outputs.keys())
            .filter(|k| k.0 == surface_id)
            .copied()
            .collect();
        for k in keys {
            if let Some((nv12s, _)) = self.nv12_outputs.remove(&k) {
                self.destroy_nv12_vec(nv12s);
            }
            if let Some((nv12s, _)) = self.nv12_opaque_outputs.remove(&k) {
                self.destroy_nv12_vec(nv12s);
            }
            self.owned_encode_nv12.remove(&k);
        }
    }

    fn destroy_all_nv12_outputs(&mut self) {
        let all: Vec<Vec<Nv12Output>> = self
            .nv12_outputs
            .drain()
            .chain(self.nv12_opaque_outputs.drain())
            .map(|(_, (v, _))| v)
            .collect();
        for nv12s in all {
            self.destroy_nv12_vec(nv12s);
        }
        self.owned_encode_nv12.clear();
    }

    /// Whether a compositor-resident Vulkan Video encoder is serving this
    /// surface.  When one is, it encodes from its own image and the frames
    /// never reach NVENC, so an NV12 `OPAQUE_FD` target would run the
    /// compute pass and hold three buffers for a reader that does not exist.
    fn vulkan_video_owns(&self, surface_id: u32) -> bool {
        self.vulkan_encoders
            .keys()
            .any(|&(sid, _)| sid == surface_id)
    }

    /// Which map an NV12 output belongs in.  `OPAQUE_FD` buffers are kept
    /// apart from `nv12_outputs` so an NVENC target and the compositor's own
    /// Vulkan Video encode image can share a `(surface, w, h)` without one
    /// destroying the other.
    fn nv12_dest_mut(
        &mut self,
        export: Nv12Export,
    ) -> &mut HashMap<(u32, u32, u32), (Vec<Nv12Output>, usize)> {
        match export {
            Nv12Export::OpaqueFd => &mut self.nv12_opaque_outputs,
            Nv12Export::None | Nv12Export::DmaBuf => &mut self.nv12_outputs,
        }
    }

    /// Whether this target converts to NV12 in an `OPAQUE_FD` buffer — i.e.
    /// takes the NVENC zero-copy path rather than the BGRA staging one.
    fn nv12_opaque_slot(&self, surface_id: u32, target_w: u32, target_h: u32) -> Option<usize> {
        let (v, idx) = self
            .nv12_opaque_outputs
            .get(&(surface_id, target_w, target_h))?;
        if v.is_empty() {
            return None;
        }
        let i = idx % v.len();
        (v[i].export == Nv12Export::OpaqueFd).then_some(i)
    }

    /// Allocate NV12 output planes for the BGRA→NV12 compute path.
    fn create_nv12_outputs(
        &mut self,
        surface_id: u32,
        target_w: u32,
        target_h: u32,
        w: u32,
        h: u32,
        export: Nv12Export,
    ) {
        // DMA-BUF export needs the dma_buf extensions; OPAQUE_FD does not,
        // and must not be gated on them — an NVIDIA-only host is exactly
        // where the OPAQUE_FD path is the whole point.
        match export {
            Nv12Export::DmaBuf if !self.has_dmabuf => return,
            // An OPAQUE_FD target is only publishable with a sync_fd — it
            // has no implicit fencing, so the emit drops any frame it
            // cannot attach one to. Building the target without the means
            // to export a fence would suppress the BGRA publish for that
            // key and then drop every frame: a permanently black stream,
            // where declining here falls back to BGRA, which works.
            Nv12Export::OpaqueFd
                if !self.has_external_memory_fd || self.external_fence_fd_fn.is_none() =>
            {
                return;
            }
            _ => {}
        }
        // A Vulkan Video session encodes from the compositor's own image at
        // this key and is what the client is being served by; installing
        // either flavour of NV12 output over it would take the image away
        // from a live encoder, and nothing would read what replaced it.
        // `create_nv12_encode_image` is how that image is (re)built, and it
        // does not come through here.
        if self
            .owned_encode_nv12
            .contains_key(&(surface_id, target_w, target_h))
        {
            eprintln!(
                "[vulkan-render] sid {surface_id} {target_w}x{target_h}: Vulkan Video owns the \
                 encode image; not installing {export:?} NV12 outputs",
            );
            return;
        }
        let handle_type = export.handle_type();
        use std::os::fd::FromRawFd;
        // Only clears this export's own map — an OPAQUE_FD target must not
        // evict the compositor's Vulkan Video encode image, or a VA-API
        // import, parked at the same key.
        self.destroy_nv12_outputs_in(export, surface_id, target_w, target_h);

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
                let mut ext_info =
                    vk::ExternalMemoryBufferCreateInfo::default().handle_types(handle_type);
                let buf_info = vk::BufferCreateInfo::default()
                    .size(buf_size)
                    .usage(
                        vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
                    )
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .push_next(&mut ext_info);
                let buffer = unsafe { self.device.create_buffer(&buf_info, None).ok()? };
                let reqs = unsafe { self.device.get_buffer_memory_requirements(buffer) };
                // DMA-BUF prefers HOST_VISIBLE so CPU encoders (x264 etc)
                // can mmap the exported buffer for their fallback read
                // path.  DEVICE_LOCAL-only memory on discrete AMD is not
                // CPU-mappable, which silently fails the encoder's mmap
                // and turns thumbnails black.  DEVICE_LOCAL|HOST_VISIBLE
                // (unified memory / iGPU) is preferred when available.
                //
                // OPAQUE_FD inverts that: its only consumer is CUDA, which
                // reads on the GPU, and nothing can mmap the handle anyway.
                // Asking for HOST_VISIBLE there would land the NV12 buffer
                // in a slower heap for no reader's benefit.
                let mem_type = match export {
                    Nv12Export::OpaqueFd => self
                        .find_memory_type(
                            reqs.memory_type_bits,
                            vk::MemoryPropertyFlags::DEVICE_LOCAL,
                        )
                        .or_else(|| {
                            self.find_memory_type(
                                reqs.memory_type_bits,
                                vk::MemoryPropertyFlags::empty(),
                            )
                        }),
                    // `None` cannot reach here — that variant's memory comes
                    // from `create_nv12_encode_image`, not this function —
                    // but prefer the mappable heap over panicking if that
                    // ever changes.
                    Nv12Export::None | Nv12Export::DmaBuf => self
                        .find_memory_type(
                            reqs.memory_type_bits,
                            vk::MemoryPropertyFlags::HOST_VISIBLE
                                | vk::MemoryPropertyFlags::HOST_COHERENT
                                | vk::MemoryPropertyFlags::DEVICE_LOCAL,
                        )
                        .or_else(|| {
                            self.find_memory_type(
                                reqs.memory_type_bits,
                                vk::MemoryPropertyFlags::HOST_VISIBLE
                                    | vk::MemoryPropertyFlags::HOST_COHERENT,
                            )
                        })
                        .or_else(|| {
                            self.find_memory_type(
                                reqs.memory_type_bits,
                                vk::MemoryPropertyFlags::DEVICE_LOCAL,
                            )
                        })
                        .or_else(|| {
                            self.find_memory_type(
                                reqs.memory_type_bits,
                                vk::MemoryPropertyFlags::empty(),
                            )
                        }),
                }?;
                let mut export_info =
                    vk::ExportMemoryAllocateInfo::default().handle_types(handle_type);
                let alloc = vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(mem_type)
                    .push_next(&mut export_info);
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
                    .handle_type(handle_type);
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
                    fd: Some(fd),
                    buf_id: next_nv12_buf_id(),
                    descriptor_set,
                    width: w,
                    height: h,
                    is_444: false,
                    kind: Nv12OutputKind::Buffer {
                        buffer,
                        memory,
                        buf_size,
                        stride,
                        uv_offset,
                    },
                    export,
                })
            })() else {
                eprintln!("[vulkan-render] failed to create NV12 buffer {w}x{h}");
                return;
            };
            self.nv12_dest_mut(export)
                .entry((surface_id, target_w, target_h))
                .or_insert_with(|| (Vec::new(), 0))
                .0
                .push(nv12);
        }
        if let Some(entry) = self
            .nv12_dest_mut(export)
            .get_mut(&(surface_id, target_w, target_h))
        {
            entry.1 = 0;
        }
        let count = self
            .nv12_dest_mut(export)
            .get(&(surface_id, target_w, target_h))
            .map_or(0, |(v, _)| v.len());
        eprintln!(
            "[vulkan-render] created {count} NV12 buffers {w}x{h} stride={stride} uv_offset={uv_offset} for target {target_w}x{target_h}",
        );
    }

    /// Import encoder-exported NV12 DMA-BUFs as Vulkan resources.
    /// For linear (modifier==0): import as VkBuffer (existing path).
    /// For tiled (modifier!=0): import as multi-plane VkImage.
    #[allow(clippy::type_complexity)]
    fn create_nv12_outputs_from_fds(
        &mut self,
        surface_id: u32,
        target_w: u32,
        target_h: u32,
        fds: &[(Arc<OwnedFd>, u32, u32, u32, u32, u64)],
    ) {
        if !self.has_dmabuf {
            return;
        }
        self.destroy_nv12_outputs_for_target(surface_id, target_w, target_h);

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
                        .entry((surface_id, target_w, target_h))
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
        if let Some((nv12s, _)) = self
            .nv12_outputs
            .get(&(surface_id, target_w, target_h))
            .filter(|(v, _)| !v.is_empty())
        {
            let kind_str = match &nv12s[0].kind {
                Nv12OutputKind::Buffer { .. } => "buffer",
                Nv12OutputKind::Image { .. } => "image",
            };
            eprintln!(
                "[vulkan-render] imported {} NV12 outputs ({kind_str}) for target {target_w}x{target_h}",
                nv12s.len(),
            );
        }
        if let Some(entry) = self.nv12_outputs.get_mut(&(surface_id, target_w, target_h)) {
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
            fd: Some(fd),
            buf_id: next_nv12_buf_id(),
            descriptor_set,
            width: w,
            height: h,
            is_444: false,
            // Imported from a VA-API-exported dma_buf.
            export: Nv12Export::DmaBuf,
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
            fd: Some(fd),
            buf_id: next_nv12_buf_id(),
            descriptor_set,
            width: w,
            height: h,
            // VA-API exports NV12 planes; 4:4:4 is the compositor-owned path.
            is_444: false,
            // Tiled NV12 imported from a VA-API-exported dma_buf.
            export: Nv12Export::DmaBuf,
            kind: Nv12OutputKind::Image {
                image,
                y_memory,
                y_view,
                uv_memory,
                uv_view,
                encode_view,
                encode_image: None,
            },
        })
    }

    /// Allocate an NV12 image the Vulkan Video encoder can read, backed by
    /// the compositor's own device-local memory.
    ///
    /// `import_nv12_image` was the only other producer of an encode-capable
    /// NV12 image, and it runs solely on plane fds VA-API exported to us.
    /// That left Vulkan Video — the tier that exists to *replace* VA-API —
    /// reachable only on hosts where VA-API was already doing the work, and
    /// silently dead everywhere else: the session was created, no frame ever
    /// came out, and the surface stayed black.  Owning the memory here is
    /// what makes "encode on the GPU" independent of who else is available.
    ///
    /// An image carrying `VIDEO_ENCODE_SRC_KHR` must be created against the
    /// profile it will be read with, so this mirrors whichever session the
    /// caller is about to build: H.264 High or High 4:4:4 Predictive, or AV1
    /// Main.  The AV1 profile comes from `vulkan_encode`, which hand-rolls it
    /// because ash 0.38 (Vulkan 1.3.281) predates `VK_KHR_video_encode_av1`.
    ///
    /// `is_444` selects the two-plane 4:4:4 format drivers advertise for High
    /// 4:4:4 Predictive.  Both layouts are two-plane, so they share the
    /// descriptor layout and differ only in the chroma plane's resolution and
    /// the shader that fills it.  AV1 here is 4:2:0 only.
    fn create_nv12_encode_image(
        &self,
        w: u32,
        h: u32,
        is_444: bool,
        codec: u8,
    ) -> Option<Nv12Output> {
        let is_av1 = codec == 0x02;
        let nv12_format = if is_444 && !is_av1 {
            vk::Format::G8_B8R8_2PLANE_444_UNORM
        } else {
            vk::Format::G8_B8R8_2PLANE_420_UNORM
        };
        // The compute shader writes a storage image; the session reads a
        // separate `VIDEO_ENCODE_SRC` image the storage image is copied
        // into per frame (see `Nv12OutputKind::Image::encode_image` for why
        // one image cannot legally wear both usages).
        let usage = vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC;

        // Both profile chains have to outlive the create call below, so both
        // leaf structs are materialised here and only the matching one is
        // chained in.  The H.264 profile must match the session's exactly,
        // including the profile IDC — High 4:4:4 Predictive is a different
        // profile, not High with a chroma flag flipped.
        let mut h264_profile = vk::VideoEncodeH264ProfileInfoKHR::default().std_profile_idc(if is_444
        {
            ash::vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH_444_PREDICTIVE
        } else {
            ash::vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH
        });
        let mut av1_leaf = crate::vulkan_encode::VideoEncodeAV1ProfileInfoKHR {
            s_type: vk::StructureType::default(),
            p_next: std::ptr::null(),
            std_profile: 0,
        };
        let profiles = [if is_av1 {
            crate::vulkan_encode::av1_encode_profile(&mut av1_leaf)
        } else {
            vk::VideoProfileInfoKHR::default()
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
                .chroma_subsampling(if is_444 {
                    vk::VideoChromaSubsamplingFlagsKHR::TYPE_444
                } else {
                    vk::VideoChromaSubsamplingFlagsKHR::TYPE_420
                })
                .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
                .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
                .push_next(&mut h264_profile)
        }];
        let mut profile_list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);

        // MUTABLE_FORMAT + the plane format list is what lets the compute
        // shader bind each plane as its own storage image.
        let format_list_entries = [vk::Format::R8_UNORM, vk::Format::R8G8_UNORM];
        let mut format_list =
            vk::ImageFormatListCreateInfo::default().view_formats(&format_list_entries);

        // Storage image: no video usage, so no profile list — MUTABLE +
        // the plane format list is what the compute shader needs, and
        // EXTENDED_USAGE is what makes STORAGE legal here at all: the
        // multiplanar 4:4:4 format itself has no STORAGE feature, only its
        // R8/R8G8 plane view formats do, and EXTENDED_USAGE tells the
        // implementation to validate usage against those.
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
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut format_list);

        let image = match unsafe { self.device.create_image(&image_info, None) } {
            Ok(i) => i,
            Err(e) => {
                eprintln!("[vulkan-render] encode NV12 image create failed {w}x{h}: {e:?}");
                return None;
            }
        };

        let reqs = unsafe { self.device.get_image_memory_requirements(image) };
        let Some(mem_type) =
            self.find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
        else {
            unsafe { self.device.destroy_image(image, None) };
            eprintln!("[vulkan-render] encode NV12 image: no DEVICE_LOCAL memory type");
            return None;
        };
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(mem_type);
        let memory = match unsafe { self.device.allocate_memory(&alloc, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { self.device.destroy_image(image, None) };
                eprintln!("[vulkan-render] encode NV12 memory alloc failed: {e:?}");
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

        // The encode-side image: `VIDEO_ENCODE_SRC` (against the session's
        // profile) + `TRANSFER_DST` for the per-frame copy, nothing else.
        let encode_image_info = vk::ImageCreateInfo::default()
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
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut profile_list);
        let encode_image = match unsafe { self.device.create_image(&encode_image_info, None) } {
            Ok(i) => i,
            Err(e) => {
                eprintln!("[vulkan-render] encode-src image create failed {w}x{h}: {e:?}");
                unsafe {
                    self.device.free_memory(memory, None);
                    self.device.destroy_image(image, None);
                }
                return None;
            }
        };
        let enc_reqs = unsafe { self.device.get_image_memory_requirements(encode_image) };
        let enc_memory = self
            .find_memory_type(
                enc_reqs.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .and_then(|mt| {
                let alloc = vk::MemoryAllocateInfo::default()
                    .allocation_size(enc_reqs.size)
                    .memory_type_index(mt);
                unsafe { self.device.allocate_memory(&alloc, None) }.ok()
            })
            .filter(|&m| unsafe { self.device.bind_image_memory(encode_image, m, 0) }.is_ok());
        let Some(enc_memory) = enc_memory else {
            eprintln!("[vulkan-render] encode-src image memory alloc/bind failed");
            unsafe {
                self.device.destroy_image(encode_image, None);
                self.device.free_memory(memory, None);
                self.device.destroy_image(image, None);
            }
            return None;
        };

        let plane_view = |target, aspect, format| unsafe {
            self.device
                .create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(target)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: aspect,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        }),
                    None,
                )
                .ok()
        };
        let cleanup = |views: &[vk::ImageView]| unsafe {
            for v in views {
                self.device.destroy_image_view(*v, None);
            }
            self.device.destroy_image(encode_image, None);
            self.device.free_memory(enc_memory, None);
            self.device.free_memory(memory, None);
            self.device.destroy_image(image, None);
        };

        let Some(y_view) = plane_view(image, vk::ImageAspectFlags::PLANE_0, vk::Format::R8_UNORM)
        else {
            cleanup(&[]);
            return None;
        };
        let Some(uv_view) =
            plane_view(image, vk::ImageAspectFlags::PLANE_1, vk::Format::R8G8_UNORM)
        else {
            cleanup(&[y_view]);
            return None;
        };
        let Some(encode_view) = plane_view(encode_image, vk::ImageAspectFlags::COLOR, nv12_format)
        else {
            cleanup(&[y_view, uv_view]);
            return None;
        };

        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(std::slice::from_ref(
                &self.compute_image_descriptor_set_layout,
            ));
        let Ok(sets) = (unsafe { self.device.allocate_descriptor_sets(&ds_alloc) }) else {
            cleanup(&[y_view, uv_view, encode_view]);
            return None;
        };
        let descriptor_set = sets[0];

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

        eprintln!(
            "[vulkan-render] allocated encode image {w}x{h} {}",
            if is_444 { "4:4:4" } else { "4:2:0" },
        );

        Some(Nv12Output {
            fd: None,
            buf_id: next_nv12_buf_id(),
            descriptor_set,
            width: w,
            height: h,
            is_444,
            // Compositor-owned memory read by Vulkan Video on this same
            // device — never exported, so no importer and no handle type.
            export: Nv12Export::None,
            kind: Nv12OutputKind::Image {
                image,
                y_memory: memory,
                y_view,
                uv_memory: vk::DeviceMemory::null(),
                uv_view,
                encode_view: Some(encode_view),
                encode_image: Some((encode_image, enc_memory)),
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
        transition_bgra: bool,
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
            Err(e) => {
                // Silence here is expensive: we return before even zeroing
                // the NV12 buffer, so the encoder reads whatever was in it
                // and the viewer gets a black picture with nothing logged
                // to say why.
                static LOGGED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!(
                        "[vulkan-render] NV12 compute: BGRA storage view failed ({e}); \
                         source image needs STORAGE usage + MUTABLE_FORMAT",
                    );
                }
                return;
            }
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

        // First dispatch for this render owns the BGRA transition;
        // subsequent dispatches find it already in GENERAL.
        if transition_bgra {
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
            }
        }
        unsafe {
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
        transition_bgra: bool,
    ) {
        let nv12 = &nv12_vec[nv12_idx];
        let enc_w = nv12.width;
        let enc_h = nv12.height;
        let Nv12OutputKind::Image {
            image,
            encode_image,
            ..
        } = &nv12.kind
        else {
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

        // NV12 image barrier always runs (UNDEFINED→GENERAL is a no-op
        // after the first frame but correctly sets up writes).  BGRA
        // barrier only on the first dispatch for this render.
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
            if transition_bgra {
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
            } else {
                self.device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[nv12_barrier],
                );
            }

            self.device.cmd_bind_pipeline(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                // Same bindings and push constants either way — only the
                // chroma plane's resolution differs.
                if nv12.is_444 {
                    self.compute_nv24_pipeline
                } else {
                    self.compute_image_pipeline
                },
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

            // Copy the converted planes into the encode-only image (see
            // `Nv12OutputKind::Image::encode_image`).  Everything stays in
            // GENERAL: the storage writes need it, vkCmdCopyImage accepts
            // it, and the encode path takes GENERAL as a source layout.
            if let Some((enc_img, _)) = encode_image {
                let planes = vk::ImageAspectFlags::PLANE_0 | vk::ImageAspectFlags::PLANE_1;
                let range = |aspect| vk::ImageSubresourceRange {
                    aspect_mask: aspect,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                };
                let src_barrier = vk::ImageMemoryBarrier::default()
                    .image(*image)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                    .subresource_range(range(planes));
                // Fully overwritten every frame, so the previous contents
                // are discardable: UNDEFINED → GENERAL.
                let dst_barrier = vk::ImageMemoryBarrier::default()
                    .image(*enc_img)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_access_mask(vk::AccessFlags::empty())
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .subresource_range(range(planes));
                self.device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[src_barrier, dst_barrier],
                );
                let layers = |aspect| vk::ImageSubresourceLayers {
                    aspect_mask: aspect,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                };
                // Plane extents are in each plane's own texel grid: full
                // size for Y (and both planes at 4:4:4), halved for the
                // 4:2:0 chroma plane.
                let (cw, ch) = if nv12.is_444 {
                    (enc_w, enc_h)
                } else {
                    (enc_w / 2, enc_h / 2)
                };
                let regions = [
                    vk::ImageCopy {
                        src_subresource: layers(vk::ImageAspectFlags::PLANE_0),
                        src_offset: vk::Offset3D::default(),
                        dst_subresource: layers(vk::ImageAspectFlags::PLANE_0),
                        dst_offset: vk::Offset3D::default(),
                        extent: vk::Extent3D {
                            width: enc_w,
                            height: enc_h,
                            depth: 1,
                        },
                    },
                    vk::ImageCopy {
                        src_subresource: layers(vk::ImageAspectFlags::PLANE_1),
                        src_offset: vk::Offset3D::default(),
                        dst_subresource: layers(vk::ImageAspectFlags::PLANE_1),
                        dst_offset: vk::Offset3D::default(),
                        extent: vk::Extent3D {
                            width: cw,
                            height: ch,
                            depth: 1,
                        },
                    },
                ];
                self.device.cmd_copy_image(
                    cb,
                    *image,
                    vk::ImageLayout::GENERAL,
                    *enc_img,
                    vk::ImageLayout::GENERAL,
                    &regions,
                );
                // Make the copy visible to the encode consumption that
                // follows this submission (same fence-ordered handoff the
                // storage image relied on before the split).
                let avail = vk::ImageMemoryBarrier::default()
                    .image(*enc_img)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::MEMORY_READ)
                    .subresource_range(range(planes));
                self.device.cmd_pipeline_barrier(
                    cb,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[avail],
                );
            }
        }

        unsafe { self.device.destroy_image_view(bgra_view, None) };
    }

    fn create_output_image(&self, w: u32, h: u32) -> Option<OutputImage> {
        let format = vk::Format::B8G8R8A8_UNORM;

        // STORAGE + MUTABLE_FORMAT let the BGRA→NV12 compute shader read
        // this image via an R8G8B8A8 storage view on the self-alloc path.
        // Without them, a thumbnail-only surface (scaled sub, no native
        // sub → no encoder-allocated external BGRA) would ship zeroed
        // NV12 and decode to black.
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
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
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::STORAGE,
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

        // Widen before multiplying: `w` and `h` are u32 and the product
        // wraps past 2^30 pixels. 32768x32768 lands on exactly 2^32, which
        // truncates to a zero-sized buffer while the copy below is still
        // issued with the full extent. `create_downscale_output` gets this
        // right; this path did not.
        let staging_size = u64::from(w) * u64::from(h) * 4;
        let buf_info = vk::BufferCreateInfo::default()
            .size(staging_size)
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
        let buf_mem_type = self.find_readback_memory_type(buf_reqs.memory_type_bits);
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
    /// Called from the compositor at surface commit time.  If the new
    /// import succeeds the previous texture is moved to the pending-destroy
    /// list (freed after the current GPU submission completes).  If the
    /// import fails the old texture is kept so the surface continues to
    /// render its last good frame instead of going black — this matters
    /// when a client reallocates buffers with a modifier the Vulkan device
    /// can't import (e.g. mpv on video reload).
    pub(crate) fn upload_surface(
        &mut self,
        surface_id: &ObjectId,
        buffer_id: Option<&ObjectId>,
        pixels: &PixelData,
        width: u32,
        height: u32,
    ) {
        // Zero-copy DMA-BUF fast path: the wl_buffer was imported on an
        // earlier commit and its VkImage samples the client's live buffer
        // memory, so there is nothing to re-import — just point the
        // surface at the cached texture.
        if matches!(pixels, PixelData::DmaBuf { .. })
            && self.has_dmabuf
            && let Some(bid) = buffer_id
        {
            static HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            static MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            if let Some(tex) = self.buffer_textures.get(bid).cloned() {
                let h = HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if h.is_multiple_of(5000) {
                    eprintln!(
                        "[dmabuf-cache] hits={h} misses={} cached={}",
                        MISSES.load(std::sync::atomic::Ordering::Relaxed),
                        self.buffer_textures.len(),
                    );
                }
                let same = self
                    .surface_textures
                    .get(surface_id)
                    .is_some_and(|cur| Arc::ptr_eq(cur, &tex));
                if !same && let Some(old) = self.surface_textures.insert(surface_id.clone(), tex) {
                    self.pending_destroy_textures.push(old);
                }
                return;
            }
            // Every miss is a full driver import — worth a line while rare,
            // and a sampled one if a client defeats the cache entirely.
            let m = MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if m < 10 || m.is_multiple_of(1000) {
                eprintln!(
                    "[dmabuf-cache] miss #{m} bid={bid:?} hits={} cached={}",
                    HITS.load(std::sync::atomic::Ordering::Relaxed),
                    self.buffer_textures.len(),
                );
            }
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
            let tex = Arc::new(tex);
            // A fresh zero-copy import also enters the per-buffer cache so
            // the client's next commit of this wl_buffer skips the import.
            if matches!(pixels, PixelData::DmaBuf { .. })
                && self.has_dmabuf
                && let Some(bid) = buffer_id
                && let Some(old) = self.buffer_textures.insert(bid.clone(), tex.clone())
            {
                self.pending_destroy_textures.push(old);
            }
            if let Some(old) = self.surface_textures.insert(surface_id.clone(), tex) {
                self.pending_destroy_textures.push(old);
            }
        } else {
            static UF: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = UF.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 10 || n.is_multiple_of(1000) {
                let had_prev = self.surface_textures.contains_key(surface_id);
                let detail = match pixels {
                    PixelData::Bgra(_) => "bgra".to_string(),
                    PixelData::Rgba(_) => "rgba".to_string(),
                    PixelData::DmaBuf {
                        fourcc, modifier, ..
                    } => format!("dmabuf fourcc=0x{fourcc:08x} modifier=0x{modifier:x}"),
                    _ => "other".to_string(),
                };
                eprintln!(
                    "[upload #{n}] FAILED {detail} {width}x{height} sid={surface_id:?} kept_prev={had_prev}",
                );
            }
        }
    }

    /// Remove a surface's cached texture.  Called when the surface is destroyed.
    pub(crate) fn remove_surface(&mut self, surface_id: &ObjectId) {
        if let Some(old) = self.surface_textures.remove(surface_id) {
            self.pending_destroy_textures.push(old);
        }
    }

    /// Drop a destroyed wl_buffer's cached import.  A surface still
    /// pointing at it keeps its shared reference — last-good-frame
    /// semantics, the import holds its own reference to the DMA-BUF —
    /// and the Vulkan objects are freed once that reference goes too.
    pub(crate) fn remove_buffer(&mut self, buffer_id: &ObjectId) {
        if let Some(old) = self.buffer_textures.remove(buffer_id) {
            self.pending_destroy_textures.push(old);
        }
    }

    /// Zero-copy SHM upload: copies (with format conversion) directly from
    /// the client's mmap'd pool memory into Vulkan-mapped texture memory,
    /// skipping the intermediate owned `Vec<u8>` that the PixelData path
    /// allocates. `mmap` is the full pool slice; `offset` is the byte
    /// offset of the surface's first pixel; `stride` is bytes per row.
    ///
    /// `swap_rb` true means source pixels are BGRA byte-order; we swap the
    /// first and third byte to produce RGBA as Vulkan expects. When false
    /// the source is already RGBA byte-order. `force_opaque` forces the
    /// alpha byte to 0xFF (for X-formats with undefined alpha).
    pub(crate) fn upload_surface_shm_mmap(
        &mut self,
        surface_id: &ObjectId,
        mmap: &[u8],
        offset: usize,
        stride: usize,
        width: u32,
        height: u32,
        swap_rb: bool,
        force_opaque: bool,
    ) -> bool {
        let row_bytes = width as usize * 4;
        let end = offset.saturating_add(stride.saturating_mul(height as usize));
        if end > mmap.len() && offset + stride * (height as usize - 1) + row_bytes > mmap.len() {
            return false;
        }

        let fmt = vk::Format::R8G8B8A8_UNORM;
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(fmt)
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

        let image = match unsafe { self.device.create_image(&image_info, None) } {
            Ok(i) => i,
            Err(_) => return false,
        };
        let mem_reqs = unsafe { self.device.get_image_memory_requirements(image) };
        let Some(mem_type) = self.find_memory_type(
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        ) else {
            unsafe { self.device.destroy_image(image, None) };
            return false;
        };
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mem_type);
        let memory = match unsafe { self.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(_) => {
                unsafe { self.device.destroy_image(image, None) };
                return false;
            }
        };
        if unsafe { self.device.bind_image_memory(image, memory, 0) }.is_err() {
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_image(image, None);
            }
            return false;
        }

        let subresource = vk::ImageSubresource {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            array_layer: 0,
        };
        let layout = unsafe { self.device.get_image_subresource_layout(image, subresource) };
        let dst_row_pitch = layout.row_pitch as usize;

        let map_ptr = match unsafe {
            self.device
                .map_memory(memory, 0, layout.size, vk::MemoryMapFlags::empty())
        } {
            Ok(p) => p as *mut u8,
            Err(_) => {
                unsafe {
                    self.device.free_memory(memory, None);
                    self.device.destroy_image(image, None);
                }
                return false;
            }
        };

        // Single pass: read from the client mmap, convert, write into
        // Vulkan mapped memory. Saves the intermediate `Vec<u8>` copy
        // that the PixelData path allocates in `ShmPool::read_buffer`.
        unsafe {
            let dst_base = map_ptr.add(layout.offset as usize);
            for row in 0..height as usize {
                let src = mmap.as_ptr().add(offset + row * stride);
                let dst = dst_base.add(row * dst_row_pitch);
                if !swap_rb && !force_opaque {
                    std::ptr::copy_nonoverlapping(src, dst, row_bytes);
                } else {
                    for col in 0..width as usize {
                        let s = src.add(col * 4);
                        let d = dst.add(col * 4);
                        if swap_rb {
                            *d = *s.add(2);
                            *d.add(1) = *s.add(1);
                            *d.add(2) = *s;
                        } else {
                            *d = *s;
                            *d.add(1) = *s.add(1);
                            *d.add(2) = *s.add(2);
                        }
                        *d.add(3) = if force_opaque { 0xFF } else { *s.add(3) };
                    }
                }
            }
            self.device.unmap_memory(memory);
        }

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(fmt)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        let view = match unsafe { self.device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(_) => {
                unsafe {
                    self.device.free_memory(memory, None);
                    self.device.destroy_image(image, None);
                }
                return false;
            }
        };

        let layouts = [self.descriptor_set_layout];
        let ds_alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&layouts);
        let descriptor_set = match unsafe { self.device.allocate_descriptor_sets(&ds_alloc) } {
            Ok(sets) => sets[0],
            Err(_) => {
                unsafe {
                    self.device.destroy_image_view(view, None);
                    self.device.free_memory(memory, None);
                    self.device.destroy_image(image, None);
                }
                return false;
            }
        };

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

        let tex = CachedSurfaceTexture {
            image,
            memory,
            view,
            descriptor_set,
            initial_layout: vk::ImageLayout::PREINITIALIZED,
        };
        if let Some(old) = self
            .surface_textures
            .insert(surface_id.clone(), Arc::new(tex))
        {
            self.pending_destroy_textures.push(old);
        }
        true
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
            // Still shared with a live cache slot (or another pending
            // entry): drop this reference only.  Whoever holds the last
            // one destroys the Vulkan objects when it is evicted itself.
            let Ok(tex) = Arc::try_unwrap(tex) else {
                continue;
            };
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

    fn has_tracked_in_flight_work(&self) -> bool {
        self.pending_submit.is_some() || !self.deferred_submits.is_empty()
    }

    fn drain_pending_destroy_targets_if_idle(&mut self) {
        if self.has_tracked_in_flight_work() {
            return;
        }

        let exts: Vec<ExternalOutput> = self.pending_destroy_external_outputs.drain(..).collect();
        for ext in exts {
            self.destroy_external_output(ext);
        }

        let nv12s: Vec<Nv12Output> = self.pending_destroy_nv12_outputs.drain(..).collect();
        for n in nv12s {
            self.destroy_nv12_output(n);
        }

        let downscale: Vec<DownscaleOutput> =
            self.pending_destroy_downscale_outputs.drain(..).collect();
        for out in downscale {
            self.destroy_downscale_output(out);
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

    /// True when `render_tree_sized` would drop the tree instead of
    /// submitting it: a previous submit is still in flight and its fence
    /// has not signalled, so the early return at the top of that function
    /// would fire.
    ///
    /// Callers that composite in response to an event which will not
    /// repeat — a `wl_surface.commit` — must consult this and defer rather
    /// than call and lose the tree.  `has_pending` is not a substitute: it
    /// is true for the whole life of a submit, including after its fence
    /// has signalled, when compositing proceeds normally.
    pub fn would_defer_submit(&self) -> bool {
        let Some(pending) = self.pending_submit.as_ref() else {
            return false;
        };
        let raw = unsafe {
            (self.device.fp_v1_0().wait_for_fences)(
                self.device.handle(),
                1,
                [pending.fence].as_ptr(),
                vk::TRUE,
                0,
            )
        };
        raw != vk::Result::SUCCESS
    }

    /// Non-blocking check: if the previous GPU submission has completed,
    /// read back its results and return them.  Called from the compositor's
    /// main event loop so completed frames are flushed to the server
    /// without waiting for the next Wayland surface commit.  Returns the
    /// size the submission composited at as `(sid, w, h)`, plus one entry
    /// for the native composite and one per registered downscale target
    /// (server-allocated BGRA at the per-client encoder size).
    ///
    /// The native size is reported separately rather than inferred from the
    /// results: every per-target entry is a downscale of the same composite,
    /// and a target registered before a shrink out-areas the real native, so
    /// picking the largest result answers with the stale size.
    #[allow(clippy::type_complexity)]
    pub fn try_retire_pending(
        &mut self,
    ) -> (Option<(u16, u32, u32)>, Vec<(u16, u32, u32, PixelData, bool)>) {
        // The compositor calls this every iteration of its event loop
        // (once per Wayland event). We deliberately do NOT drain
        // deferred external submits here: that happens at submit time
        // in render_tree_sized so cleanup frequency is bounded by GPU
        // frame rate rather than by Wayland event rate. Only the
        // self-allocated pending_submit needs per-iteration polling
        // because its staging readback is what produces a frame.
        let Some(pending) = self.pending_submit.take() else {
            return (None, Vec::new());
        };
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
            return (None, Vec::new());
        }
        let toplevel_sid = pending.toplevel_sid;
        let native = (toplevel_sid, pending.phys_w, pending.phys_h);
        let results = self.retire_pending(pending);
        // Free per-frame temporary textures now that the GPU is done.
        self.free_frame_textures();
        (
            Some(native),
            results
                .into_iter()
                .map(|(w, h, p, encoder_skip)| (toplevel_sid, w, h, p, encoder_skip))
                .collect(),
        )
    }

    /// Produce the native BGRA + per-downscale-target BGRA results from
    /// a completed GPU submission.  External targets were emitted
    /// immediately by `render_tree_sized` — `retire_pending` only
    /// handles staging readback (native + downscale targets).
    ///
    /// The `bool` per result is `encoder_skip`: true when this BGRA was
    /// published on demand over a live NV12 zero-copy stream and must not
    /// reach an encoder (see the range-mismatch comment at the publish
    /// site).
    fn retire_pending(&mut self, pending: PendingSubmit) -> Vec<(u32, u32, PixelData, bool)> {
        let mut results: Vec<(u32, u32, PixelData, bool)> = Vec::new();

        // Native BGRA from the self-alloc output_image staging buffer.
        if pending.self_output_idx < self.output_images.len() {
            let img = &self.output_images[pending.self_output_idx];
            if img.width != pending.phys_w || img.height != pending.phys_h {
                // Output images were recreated (resize) between submit
                // and retire — the staging buffer we'd read has been
                // freed.  Drop this frame.
                eprintln!(
                    "[retire_pending] output image size mismatch: pending={}x{} current={}x{} (resize during flight)",
                    pending.phys_w, pending.phys_h, img.width, img.height,
                );
            } else {
                // Widen before multiplying, matching the allocation in
                // `create_output_image`. Both used to wrap the same way, so
                // the read stayed in bounds by coincidence rather than by
                // construction.
                // An NV12 OPAQUE_FD target at the composite size has
                // already published this frame under the same key, from
                // the GPU, without a readback. Publishing BGRA on top
                // would overwrite it a tick later and put NVENC straight
                // back on the copy this path exists to remove — and the
                // `to_vec()` right below is one of the two copies.
                //
                // Unless something wants CPU pixels right now. `surface
                // capture` asks on demand, and the staging buffer is
                // filled every frame regardless, so the pixels are already
                // sitting there — only the copy that publishes them is
                // skipped. That one BGRA frame must NOT reach the encoder,
                // though: the zero-copy shader converts full-range
                // (black = Y 0) while NVENC's own ARGB conversion is
                // limited-range (black = Y 16), so one BGRA frame spliced
                // into an NV12 stream lifts every black to gray for a
                // frame.  Mark it encoder_skip so the server feeds it to
                // the pixel cache but not to any encoder.
                let wanted_now = std::mem::take(&mut self.publish_native_bgra_once);
                let zero_copy_live = self
                    .nv12_opaque_slot(pending.surface_id, pending.phys_w, pending.phys_h)
                    .is_some();
                if !wanted_now && zero_copy_live {
                    // fall through to cleanup
                } else {
                    let size = pending.phys_w as usize * pending.phys_h as usize * 4;
                    let bgra =
                        unsafe { std::slice::from_raw_parts(img.staging_ptr, size) }.to_vec();
                    results.push((
                        pending.phys_w,
                        pending.phys_h,
                        PixelData::Bgra(Arc::new(bgra)),
                        zero_copy_live,
                    ));
                }
            }
        } else {
            eprintln!(
                "[retire_pending] self_output_idx {} out of range (len={})",
                pending.self_output_idx,
                self.output_images.len(),
            );
        }

        // Per-downscale-target BGRA — server-allocated targets that the
        // render loop blitted into and copied to staging this frame.
        for &(tw, th) in &pending.downscale_targets {
            let Some(out) = self.downscale_outputs.get(&(pending.surface_id, tw, th)) else {
                // Target was cleared between submit and retire.  Drop.
                continue;
            };
            if out.width != tw || out.height != th {
                eprintln!(
                    "[retire_pending] downscale target {tw}x{th} resized mid-flight; dropping",
                );
                continue;
            }
            let size = (tw as usize) * (th as usize) * 4;
            let bgra = unsafe { std::slice::from_raw_parts(out.staging_ptr, size) }.to_vec();
            results.push((tw, th, PixelData::Bgra(Arc::new(bgra)), false));
        }

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
        results
    }

    /// Free deferred external submissions whose fences have signalled.
    fn drain_deferred_submits(&mut self) {
        if self.deferred_submits.is_empty() {
            return;
        }
        // Single batched probe: waitAll=false with timeout=0 returns
        // SUCCESS iff at least one fence has signalled. This collapses
        // N per-fence syscalls into one in the common "nothing ready"
        // case — the compositor main loop calls us every iteration.
        let fences: Vec<vk::Fence> = self.deferred_submits.iter().map(|p| p.fence).collect();
        let any_ready = unsafe {
            (self.device.fp_v1_0().wait_for_fences)(
                self.device.handle(),
                fences.len() as u32,
                fences.as_ptr(),
                vk::FALSE,
                0,
            )
        };
        if any_ready != vk::Result::SUCCESS {
            return;
        }
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
        self.drain_pending_destroy_targets_if_idle();
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
        self.drain_pending_destroy_targets_if_idle();
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
    ) -> Vec<(u16, u32, u32, PixelData, bool)> {
        // Retire the previous submission if done (non-blocking).  The
        // self-alloc readback (compositor BGRA at native) is one frame
        // delayed — staging buffer copy needs the fence to complete.
        // External targets have already returned their pixels
        // immediately (zero-delay; the encoder's VPP waits on the
        // GPU via implicit DMA-BUF fencing or an exported sync_fd).
        static ENTRY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let entry_n = ENTRY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let had_pending = self.pending_submit.is_some();
        let mut results: Vec<(u16, u32, u32, PixelData, bool)> = Vec::new();
        if let Some(pending) = self.pending_submit.take() {
            let prev_sid = pending.toplevel_sid;
            let (prev_surface, prev_w, prev_h) =
                (pending.surface_id, pending.phys_w, pending.phys_h);
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
                // Empty is the normal outcome under NV12 zero-copy: the
                // frame was already published GPU-side and the readback
                // is deliberately suppressed.  Only log when nothing was
                // suppressing it.
                if r.is_empty()
                    && self
                        .nv12_opaque_slot(prev_surface, prev_w, prev_h)
                        .is_none()
                {
                    eprintln!("[render_tree_sized] fence OK but retire_pending=empty");
                }
                for (w, h, p, encoder_skip) in r {
                    results.push((prev_sid, w, h, p, encoder_skip));
                }
            } else {
                // Self-alloc readback: must wait for fence — re-stash
                // and return any results already collected (probably
                // none, but be conservative).  External targets in this
                // submit will be returned alongside the next render.
                self.pending_submit = Some(pending);
                return results;
            }
        } else {
            self.free_frame_textures();
        }
        if entry_n < 20 || entry_n.is_multiple_of(50) {
            eprintln!(
                "[render_tree_sized #{entry_n}] had_pending={had_pending} prev_results={} ext_outputs={} deferred={} pending_after={}",
                results.len(),
                self.external_outputs.len(),
                self.deferred_submits.len(),
                self.pending_submit.is_some(),
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
            return results;
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
            return results;
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

        // Always composite the surface at native size into a
        // self-allocated `output_image`.  After the render pass we
        // GPU-blit (LINEAR) the native frame into each per-target
        // external buffer for the per-client encoders, then dispatch
        // the BGRA→NV12 compute against the resized BGRA copy.  A
        // staging readback of the native frame always runs so callers
        // that want plain BGRA at native (e.g. CPU encoders, the
        // Capture command) get pixels even when no external target is
        // registered.
        let sid = toplevel_sid as u32;

        // Forget what a target was sized against once the target itself is
        // gone.  Done here rather than in each teardown path because this is
        // the only reader, so it cannot drift out of step with the two maps
        // it shadows.
        {
            let external = &self.external_outputs;
            let downscale = &self.downscale_outputs;
            self.target_natives
                .retain(|k, _| external.contains_key(k) || downscale.contains_key(k));
        }

        // A per-client target is an aspect-preserving inscription of the
        // native composite, and the blits below stretch the whole native
        // frame across the whole target with no letterbox.  So a target
        // sized against a different composite than the one we are about to
        // produce cannot be filled without squashing the picture — a
        // 1200x1000 composite squeezed into the 1200x674 target that the
        // pre-resize 1600x900 aspect asked for.
        //
        // Drop those instead: the server re-registers at the right size as
        // soon as it sees the new native, and a frame that arrives a beat
        // late beats one that arrives wrong.
        let fits_composite = |natives: &HashMap<(u32, u32, u32), (u32, u32)>, tw, th| {
            // A target with no recorded native predates this bookkeeping or
            // was registered by a path that does not resize; leave it alone.
            natives
                .get(&(sid, tw, th))
                .is_none_or(|&n| n == (phys_w, phys_h))
        };
        let skipped_target = |tw: u32, th: u32, was: Option<&(u32, u32)>| {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 10 || n.is_multiple_of(200) {
                eprintln!(
                    "[vulkan-render] sid {sid}: skipping {tw}x{th} target, sized for \
                     {was:?} but the composite is now {phys_w}x{phys_h}",
                );
            }
        };

        // Collect every (target_w, target_h, ext_idx) we will blit to
        // this frame, paired with the resolved external buffer slot.
        // Distinct target sizes share the same native render and one
        // command buffer / fence / submit.
        let mut external_targets_keys: Vec<(u32, u32, u32)> = self
            .external_outputs
            .keys()
            .filter(|k| k.0 == sid)
            .copied()
            .collect();
        // Stable order: target_w then target_h.  Determinism helps
        // debugging and keeps perf predictable across runs.
        external_targets_keys.sort_unstable();

        // Resolve each external target to (target_w, target_h, idx).
        let external_targets: Vec<(u32, u32, usize)> = external_targets_keys
            .iter()
            .filter_map(|&key| {
                let (ext_vec, ext_idx) = self.external_outputs.get(&key)?;
                if ext_vec.is_empty() {
                    return None;
                }
                if !fits_composite(&self.target_natives, key.1, key.2) {
                    skipped_target(key.1, key.2, self.target_natives.get(&key));
                    return None;
                }
                let idx = ext_idx % ext_vec.len();
                Some((key.1, key.2, idx))
            })
            .collect();

        // Downscale targets (server-allocated BGRA, no GBM): same
        // (sid, target_w, target_h) keying as externals.  These are
        // for per-client encoders that don't import DMA-BUFs (NVENC,
        // software).  Skip any (tw, th) that already has an external —
        // the external path already produces a target-sized frame.
        let mut downscale_target_keys: Vec<(u32, u32, u32)> = self
            .downscale_outputs
            .keys()
            .filter(|k| k.0 == sid && !self.external_outputs.contains_key(k))
            .copied()
            .collect();
        downscale_target_keys.sort_unstable();
        let downscale_targets: Vec<(u32, u32)> = downscale_target_keys
            .iter()
            .filter(|&&key| {
                // A target at exactly the composite size would blit the
                // native frame onto itself and stage a second,
                // byte-identical readback.  The native staging copy
                // below already publishes those pixels under the same
                // (sid, w, h) key the encoder looks up, so the blit and
                // its `to_vec()` in `retire_pending` are pure waste.
                // Any encoder sized to the whole surface — the common
                // case whenever the client needs no downscale — lands
                // here on every frame.
                //
                // Unless it converts to NV12: then the target is not a
                // second copy of the native pixels but the only thing the
                // encoder can read, and skipping it is what would leave
                // NVENC on the staging readback. `retire_pending` drops
                // the BGRA publish for this key in that case, so the two
                // do not both claim it.
                if (key.1, key.2) == (phys_w, phys_h)
                    && self.nv12_opaque_slot(sid, key.1, key.2).is_none()
                {
                    return false;
                }
                let ok = fits_composite(&self.target_natives, key.1, key.2);
                if !ok {
                    skipped_target(key.1, key.2, self.target_natives.get(&key));
                }
                ok
            })
            .map(|&(_, w, h)| (w, h))
            .collect();

        // Self-allocated native output image — always present.
        self.ensure_output_images(phys_w, phys_h);
        if self.output_images.is_empty() {
            eprintln!("[render_tree_sized] output_images empty after ensure ({phys_w}x{phys_h})");
            return results;
        }
        let self_output_idx = self.output_idx;
        let (out_framebuffer, out_image, out_staging_buf) = {
            let img = &self.output_images[self_output_idx];
            (img.framebuffer, img.image, img.staging_buf)
        };

        // Allocate command buffer.
        let cb_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let cb = unsafe {
            match self.device.allocate_command_buffers(&cb_alloc) {
                Ok(v) => v[0],
                Err(e) => {
                    eprintln!("[render_tree_sized] allocate_command_buffers failed: {e}");
                    return results;
                }
            }
        };

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            if let Err(e) = self.device.begin_command_buffer(cb, &begin_info) {
                eprintln!("[render_tree_sized] begin_command_buffer failed: {e}");
                self.device.free_command_buffers(self.command_pool, &[cb]);
                return results;
            }
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
            /// Framebuffer-space rectangle this layer may write, when a
            /// `wp_viewport` source crop means the quad deliberately
            /// overhangs it.  `None` = the whole render area.
            scissor: Option<vk::Rect2D>,
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

            // A `wp_viewport` source crop asks for a sub-rectangle of the
            // texture to fill the destination.  The vertex shader hands the
            // fragment stage `v_tc = pos`, so the quad always samples the
            // whole texture and there is nowhere to put a crop — short of
            // recompiling the SPIR-V, which is checked in as a blob with no
            // build rule.
            //
            // Draw it as the same mapping instead: stretch the quad to
            // wherever the *whole* texture would have to land for the
            // cropped part to cover the destination exactly, then scissor
            // back to the destination so only that part is written.  With
            // `u = 0, w = 1` this collapses to the destination rect, so the
            // uncropped path is unchanged.
            let ((qx, qy, qw, qh), scissor) = match l.src {
                Some((u, v, sw, sh)) if sw > 0.0 && sh > 0.0 => {
                    let (fw, fh) = (pw as f32 / sw, ph as f32 / sh);
                    (
                        (px as f32 - u * fw, py as f32 - v * fh, fw, fh),
                        Some(clamped_scissor(px, py, pw, ph, phys_w, phys_h)),
                    )
                }
                _ => ((px as f32, py as f32, pw as f32, ph as f32), None),
            };

            // Vulkan clip space: x=[-1,1] left→right, y=[-1,1] top→bottom.
            let clip_x = (qx / phys_w as f32) * 2.0 - 1.0;
            let mut clip_y = (qy / phys_h as f32) * 2.0 - 1.0;
            let clip_w = (qw / phys_w as f32) * 2.0;
            let mut clip_h = (qh / phys_h as f32) * 2.0;

            // For y_invert (OpenGL-origin) DMA-BUFs, flip the quad
            // vertically.  The vertex shader maps pos.y ∈ [0,1] to
            // v_tc.y ∈ [0,1]; negating clip_h and offsetting clip_y
            // by the old clip_h effectively samples v_tc.y from 1→0
            // instead of 0→1, flipping the image.  It mirrors the quad
            // about its own centre, so it composes with the stretch above:
            // the crop offset is already baked into the quad's position,
            // and the scissor is in framebuffer space either way.
            if l.y_invert {
                clip_y += clip_h;
                clip_h = -clip_h;
            }

            draws.push(DrawCmd {
                descriptor_set: ds,
                image: img,
                old_layout,
                geom: [clip_x, clip_y, clip_w, clip_h],
                scissor,
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
            return results;
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
        let full_scissor = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: phys_w,
                height: phys_h,
            },
        };
        let mut scissor_now = full_scissor;
        for d in &draws {
            unsafe {
                // Only a cropped layer narrows the scissor, and it has to be
                // widened again afterwards or it would clip every layer
                // drawn on top of it.
                let want = d.scissor.unwrap_or(full_scissor);
                if want != scissor_now {
                    self.device.cmd_set_scissor(cb, 0, &[want]);
                    scissor_now = want;
                }
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

        // For each registered external target: blit (LINEAR) the
        // native frame into the target's BGRA buffer, then dispatch
        // BGRA→NV12 compute against that resized BGRA copy.  All
        // distinct target sizes share the single command buffer so
        // one GPU submission handles every consumer.
        //
        // The native `out_image` is in TRANSFER_SRC_OPTIMAL after the
        // render pass so it's already a valid blit source.  Each
        // external target is freshly imported (UNDEFINED) or was last
        // left in TRANSFER_DST_OPTIMAL by the previous render — we
        // transition to TRANSFER_DST_OPTIMAL unconditionally before
        // the blit (UNDEFINED → TRANSFER_DST_OPTIMAL is a no-op
        // discarding the previous contents, which is what we want).
        for &(tw, th, ext_idx) in &external_targets {
            let ext_image = {
                let (ext_vec, _) = &self.external_outputs[&(sid, tw, th)];
                ext_vec[ext_idx].image
            };
            // Transition the destination to TRANSFER_DST_OPTIMAL.
            let to_dst = vk::ImageMemoryBarrier::default()
                .image(ext_image)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
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
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_dst],
                );
            }

            let blit = vk::ImageBlit::default()
                .src_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .src_offsets([
                    vk::Offset3D { x: 0, y: 0, z: 0 },
                    vk::Offset3D {
                        x: phys_w as i32,
                        y: phys_h as i32,
                        z: 1,
                    },
                ])
                .dst_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .dst_offsets([
                    vk::Offset3D { x: 0, y: 0, z: 0 },
                    vk::Offset3D {
                        x: tw as i32,
                        y: th as i32,
                        z: 1,
                    },
                ]);
            unsafe {
                self.device.cmd_blit_image(
                    cb,
                    out_image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    ext_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[blit],
                    vk::Filter::LINEAR,
                );
            }

            // Dispatch BGRA→NV12 compute on the resized external BGRA
            // image, if NV12 outputs are registered for this target.
            // The dispatch helpers' built-in transition assumes the
            // BGRA source is leaving the COLOR_ATTACHMENT_OUTPUT stage,
            // which isn't true here — the source was written by a blit
            // (TRANSFER stage).  Transition it ourselves with the right
            // stages/access masks and pass `transition_bgra=false` so
            // the helper doesn't double-transition.
            let nv12_dispatch: Option<(usize, bool)> = self
                .nv12_outputs
                .get(&(sid, tw, th))
                .and_then(|(nv12_vec, cur_idx)| {
                    if nv12_vec.is_empty() {
                        return None;
                    }
                    let nv12_idx = cur_idx % nv12_vec.len();
                    let is_image = matches!(nv12_vec[nv12_idx].kind, Nv12OutputKind::Image { .. });
                    Some((nv12_idx, is_image))
                });
            if let Some((nv12_idx, is_image)) = nv12_dispatch {
                let to_general = vk::ImageMemoryBarrier::default()
                    .image(ext_image)
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
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
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::COMPUTE_SHADER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[to_general],
                    );
                }
                let nv12_vec = &self.nv12_outputs[&(sid, tw, th)].0;
                if is_image {
                    self.dispatch_nv12_compute_image(
                        cb, ext_image, nv12_vec, nv12_idx, tw, th, false,
                    );
                } else {
                    self.dispatch_nv12_compute(cb, ext_image, nv12_vec, nv12_idx, tw, th, false);
                }
            }
        }

        // For each downscale target (server-allocated BGRA, no GBM):
        // blit (LINEAR) the native composite into the target image
        // then copy it into the CPU-mapped staging buffer.  retire_pending
        // emits one PixelData::Bgra per downscale target so the per-client
        // encoder receives target-sized BGRA without a CPU resize step.
        for &(tw, th) in &downscale_targets {
            let (ds_image, ds_staging) = {
                let out = &self.downscale_outputs[&(sid, tw, th)];
                (out.image, out.staging_buf)
            };
            let to_dst = vk::ImageMemoryBarrier::default()
                .image(ds_image)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
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
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_dst],
                );
            }

            let blit = vk::ImageBlit::default()
                .src_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .src_offsets([
                    vk::Offset3D { x: 0, y: 0, z: 0 },
                    vk::Offset3D {
                        x: phys_w as i32,
                        y: phys_h as i32,
                        z: 1,
                    },
                ])
                .dst_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .dst_offsets([
                    vk::Offset3D { x: 0, y: 0, z: 0 },
                    vk::Offset3D {
                        x: tw as i32,
                        y: th as i32,
                        z: 1,
                    },
                ]);
            unsafe {
                self.device.cmd_blit_image(
                    cb,
                    out_image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    ds_image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[blit],
                    vk::Filter::LINEAR,
                );
            }

            // Transition to TRANSFER_SRC_OPTIMAL — for the buffer copy, or
            // as the layout `dispatch_nv12_compute` expects to move to
            // GENERAL from.
            let to_src = vk::ImageMemoryBarrier::default()
                .image(ds_image)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
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
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_src],
                );
            }

            // NVENC zero-copy: convert straight into the OPAQUE_FD NV12
            // buffer and stop.  The staging copy below, and the `to_vec()`
            // in `retire_pending` that reads it, are the two per-frame
            // full-surface copies this whole path exists to remove — so
            // skipping them is the point, not an optimisation on top.
            if let Some((nv12_vec, nv12_idx)) = self
                .nv12_opaque_outputs
                .get(&(sid, tw, th))
                .filter(|(v, _)| !v.is_empty())
                .map(|(v, i)| (v, *i % v.len()))
                && nv12_vec[nv12_idx].export == Nv12Export::OpaqueFd
            {
                self.dispatch_nv12_compute(cb, ds_image, nv12_vec, nv12_idx, tw, th, true);
                continue;
            }

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
                    width: tw,
                    height: th,
                    depth: 1,
                },
            };
            unsafe {
                self.device.cmd_copy_image_to_buffer(
                    cb,
                    ds_image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    ds_staging,
                    &[region],
                );
            }
        }

        // Always copy the native composite into the staging buffer for
        // CPU readback.  Out_image is in TRANSFER_SRC_OPTIMAL after the
        // render pass — we never transitioned it (the blits above use
        // it as a source) so the copy can run directly.
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

        // Fill our own NV12 encode image from the native composite, for
        // Vulkan Video sessions that are not riding on a VA-API external.
        // This runs after the staging copy so `out_image` is still a valid
        // TRANSFER_SRC when that copy needs it, and restores the layout
        // afterwards so the frame ends exactly as the rest of the code
        // expects to find it.
        let owns_encode_nv12 = self.owned_encode_nv12.contains_key(&(sid, phys_w, phys_h));
        if owns_encode_nv12 {
            let to_general = vk::ImageMemoryBarrier::default()
                .image(out_image)
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_READ)
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
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_general],
                );
            }
            let nv12_vec = &self.nv12_outputs[&(sid, phys_w, phys_h)].0;
            self.dispatch_nv12_compute_image(cb, out_image, nv12_vec, 0, phys_w, phys_h, false);
            let back_to_src = vk::ImageMemoryBarrier::default()
                .image(out_image)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
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
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[back_to_src],
                );
            }
        }

        // Submit asynchronously.
        unsafe {
            if let Err(e) = self.device.end_command_buffer(cb) {
                eprintln!("[render_tree_sized] end_command_buffer failed: {e}");
                self.device.free_command_buffers(self.command_pool, &[cb]);
                return results;
            }
        }

        // When any external target needs explicit sync (tiled NV12 on
        // radv) we export a SYNC_FD so the encoder can wait off-thread.
        // Note: vkGetFenceFdKHR with SYNC_FD transfers the payload and
        // resets the source VkFence — we cannot use the same fence for
        // both export and our own cleanup tracking.  When sync_fd
        // export is needed we submit two signal targets: an
        // export-only fence (consumed by sync_fd) and a separate
        // tracking fence we use to retire `cb` and per-frame textures.
        //
        // Every OPAQUE_FD target needs it too, and unconditionally: a
        // dma_buf carries implicit fencing that orders a later importer
        // against our writes, and an OPAQUE_FD allocation carries none. If
        // the export fails there we must not publish the buffer at all —
        // see the emit below.
        let needs_sync_fd_export = self.external_fence_fd_fn.is_some()
            && (external_targets.iter().any(|&(tw, th, _)| {
                self.nv12_outputs
                    .get(&(sid, tw, th))
                    .is_some_and(|(v, idx)| {
                        !v.is_empty()
                            && matches!(v[idx % v.len()].kind, Nv12OutputKind::Image { .. })
                    })
            }) || downscale_targets
                .iter()
                .any(|&(tw, th)| self.nv12_opaque_slot(sid, tw, th).is_some()));

        let tracking_fence = unsafe {
            match self
                .device
                .create_fence(&vk::FenceCreateInfo::default(), None)
            {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("[render_tree_sized] create_fence(tracking) failed: {e}");
                    self.device.free_command_buffers(self.command_pool, &[cb]);
                    return results;
                }
            }
        };
        // Optional secondary fence whose payload we will export as a
        // sync_fd for the encoder.  Created with SYNC_FD export
        // capability when needed; destroyed once the export is done.
        let export_fence: Option<vk::Fence> = if needs_sync_fd_export {
            let mut export_info = vk::ExportFenceCreateInfo::default()
                .handle_types(vk::ExternalFenceHandleTypeFlags::SYNC_FD);
            let fence_info = vk::FenceCreateInfo::default().push_next(&mut export_info);
            unsafe {
                match self.device.create_fence(&fence_info, None) {
                    Ok(f) => Some(f),
                    Err(e) => {
                        eprintln!("[render_tree_sized] create_fence(sync_fd) failed: {e}");
                        // Continue without sync_fd export — fall back to
                        // the blocking wait branch below.
                        None
                    }
                }
            }
        } else {
            None
        };

        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cb));
        // Submit twice with different fences so each gets the GPU
        // completion signal independently.  The driver collapses these
        // into a single dispatch since the second submit only carries
        // a fence and no work.
        unsafe {
            if let Err(e) = self
                .device
                .queue_submit(self.queue, &[submit], tracking_fence)
            {
                eprintln!("[render_tree_sized] queue_submit (tracking) failed: {e}");
                if let Some(ef) = export_fence {
                    self.device.destroy_fence(ef, None);
                }
                self.device.destroy_fence(tracking_fence, None);
                self.device.free_command_buffers(self.command_pool, &[cb]);
                return results;
            }
            if let Some(ef) = export_fence {
                let empty = vk::SubmitInfo::default();
                if let Err(e) = self.device.queue_submit(self.queue, &[empty], ef) {
                    eprintln!("[render_tree_sized] queue_submit (export fence) failed: {e}");
                    self.device.destroy_fence(ef, None);
                    // Continue with tracking fence; encoder will block.
                }
            }
        }
        let fence = tracking_fence;

        // Vulkan Video encode (compositor-resident, native size only).  One
        // encoder per subscribing client, so this runs once per client that
        // owns a session on this surface and yields one bitstream each.  The
        // NV12 image is only read — `encode` neither transitions nor
        // consumes it — so every encoder shares the frame the compute pass
        // just wrote.
        //
        // This used to sit inside the `external_targets` loop, which meant
        // the tier billed as "no VA-API, no DMA-BUF export/import" only ran
        // when VA-API had already exported a surface at native size.  On a
        // host without VA-API the loop body never executed, no bitstream was
        // ever produced, and the surface stayed black forever.  It belongs
        // out here, keyed on the encoders that exist rather than on who else
        // happens to want a copy of the frame.
        //
        // Unlike the VA-API consumers, which synchronise via DMA-BUF
        // implicit fencing or the exported sync_fd, reading the NV12 image
        // from the encode queue needs the compute that fills it to have
        // landed — so wait for the submission first.  `encode` already
        // blocks on its own fence, so this adds no new class of stall.
        let mut encoder_cids: Vec<u64> = self
            .vulkan_encoders
            .keys()
            .filter(|&&(esid, _)| esid == sid)
            .map(|&(_, cid)| cid)
            .collect();
        if !encoder_cids.is_empty() {
            encoder_cids.sort_unstable();
            let nv12_image_and_view =
                self.nv12_outputs
                    .get(&(sid, phys_w, phys_h))
                    .and_then(|(v, idx)| {
                        let n = v.get(idx % v.len().max(1))?;
                        match &n.kind {
                            Nv12OutputKind::Image {
                                image,
                                encode_view,
                                encode_image,
                                ..
                            } => {
                                // The session reads the encode-only image
                                // when the storage/encode split is in
                                // effect; imported images carry both roles.
                                let img = encode_image.map_or(*image, |(ei, _)| ei);
                                encode_view.map(|ev| (img, ev))
                            }
                            _ => None,
                        }
                    });
            if nv12_image_and_view.is_none() {
                // Encoders exist but no encode image sits at the composite
                // size — the surface resized under the sessions (their
                // image is keyed at the size they were built for).  Skipped
                // silently, this is a permanent freeze: no bitstream ever
                // reaches the server, and no failure is counted, so the
                // giveup path never fires either.  Count it.
                for &cid in &encoder_cids {
                    let n = self.vulkan_encode_failures.entry((sid, cid)).or_insert(0);
                    *n += 1;
                    if *n == VULKAN_ENCODE_FAILURE_LIMIT {
                        eprintln!(
                            "[vulkan-render] surface {sid} client {cid}: no encode image at \
                             {phys_w}x{phys_h}; giving up on Vulkan Video",
                        );
                        self.vulkan_encode_giveups.push((sid, cid));
                    }
                }
            }
            if let Some((nv12_img, ev)) = nv12_image_and_view {
                let waited = unsafe {
                    self.device.wait_for_fences(
                        &[fence],
                        true,
                        crate::vulkan_encode::encode_fence_timeout_ns(),
                    )
                }
                .is_ok();
                if !waited {
                    eprintln!(
                        "[vulkan-render] timed out waiting for the composite before encode; skipping surface {sid} this frame",
                    );
                    encoder_cids.clear();
                }
                for cid in encoder_cids {
                    let encoder = self.vulkan_encoders.get_mut(&(sid, cid)).unwrap();
                    let codec_flag = encoder.codec_flag();
                    // A session outlived the size it was built at (the
                    // surface resized under it).  Encoding anyway would
                    // read an image shaped differently from the coded
                    // extent and emit a bitstream whose frames decode at
                    // the old size while the wire header claims the new
                    // one.  The server tears the session down on
                    // SurfaceResized; count the skip as a failure so the
                    // giveup path still unwedges us if that never lands.
                    if encoder.source_dimensions() != (phys_w, phys_h) {
                        let n = self.vulkan_encode_failures.entry((sid, cid)).or_insert(0);
                        *n += 1;
                        if *n == VULKAN_ENCODE_FAILURE_LIMIT {
                            eprintln!(
                                "[vulkan-render] surface {sid} client {cid}: session size {}x{} \
                                 vs composite {phys_w}x{phys_h}; giving up on Vulkan Video",
                                encoder.source_dimensions().0,
                                encoder.source_dimensions().1,
                            );
                            self.vulkan_encode_giveups.push((sid, cid));
                        }
                        continue;
                    }
                    let encoded = unsafe {
                        encoder.encode(
                            &self.device,
                            self.video_fns.as_ref().unwrap(),
                            self.video_encode_queue.unwrap(),
                            self.video_encode_command_pool.unwrap(),
                            nv12_img,
                            ev,
                            false,
                        )
                    };
                    match encoded {
                        Some((bitstream, is_keyframe)) => {
                            self.vulkan_encode_failures.remove(&(sid, cid));
                            self.pending_encoded_frames.push(EncodedFrame {
                                surface_id: toplevel_sid,
                                client_id: cid,
                                width: phys_w,
                                height: phys_h,
                                data: Arc::new(bitstream),
                                is_keyframe,
                                codec_flag,
                            });
                        }
                        // A silent `None` here is what made a dead session
                        // indistinguishable from a warming-up one, so the
                        // server waited on `vulkan_await` forever.  Count
                        // them and let the caller give up on this encoder.
                        None => {
                            let n = self.vulkan_encode_failures.entry((sid, cid)).or_insert(0);
                            *n += 1;
                            if *n == VULKAN_ENCODE_FAILURE_LIMIT {
                                eprintln!(
                                    "[vulkan-render] surface {sid} client {cid}: {n} consecutive encode failures; giving up on Vulkan Video",
                                );
                                self.vulkan_encode_giveups.push((sid, cid));
                            }
                        }
                    }
                }
            }
        }

        // Split the targets by who publishes them.  An OPAQUE_FD target
        // wrote NV12 and never touched its staging buffer, so leaving it in
        // `downscale_targets` would have `retire_pending` read that stale
        // staging and publish a `Bgra` frame over the top of the NV12 one.
        type Targets = Vec<(u32, u32)>;
        let (nv12_opaque_targets, staging_targets): (Targets, Targets) = downscale_targets
            .iter()
            .partition(|&&(tw, th)| self.nv12_opaque_slot(sid, tw, th).is_some());

        let submit_info = PendingSubmit {
            fence,
            cb,
            textures: std::mem::take(&mut self.frame_textures),
            self_output_idx,
            phys_w,
            phys_h,
            surface_id: sid,
            downscale_targets: staging_targets,
            toplevel_sid,
        };

        // Export the dedicated export-fence as a sync_fd ONCE (shared
        // across all targets that need explicit sync).
        // vkGetFenceFdKHR(SYNC_FD) consumes the fence's payload and
        // resets it, so we use a dedicated `export_fence` distinct
        // from `tracking_fence` (which we use to know when the cb is
        // safe to free).  Each consumer Arc-shares the OwnedFd so
        // dup()/poll() works independently per consumer.
        let shared_sync_fd: Option<Arc<std::os::fd::OwnedFd>> =
            if let (Some(ext_fence_fn), Some(ef)) =
                (self.external_fence_fd_fn.as_ref(), export_fence)
            {
                let get_info = vk::FenceGetFdInfoKHR::default()
                    .fence(ef)
                    .handle_type(vk::ExternalFenceHandleTypeFlags::SYNC_FD);
                let result = match unsafe { ext_fence_fn.get_fence_fd(&get_info) } {
                    Ok(raw_fd) if raw_fd >= 0 => {
                        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) };
                        Some(Arc::new(owned))
                    }
                    Ok(_) | Err(_) => {
                        // Fallback: block on tracking_fence so the encoder
                        // still sees a finished frame.
                        eprintln!(
                            "[vulkan-render] vkGetFenceFdKHR failed; \
                         falling back to blocking wait"
                        );
                        unsafe {
                            let _ = self.device.wait_for_fences(&[fence], true, 5_000_000_000);
                        }
                        None
                    }
                };
                // The export-fence's payload has been consumed and the
                // VkFence is now reset; we can safely destroy it.
                unsafe { self.device.destroy_fence(ef, None) };
                result
            } else {
                None
            };

        // NVENC zero-copy targets.  Published immediately, like the
        // external ones and for the same reason: the consumer synchronises
        // itself against `sync_fd` rather than us blocking here.
        //
        // Without a sync_fd we publish nothing. There is no implicit
        // fencing behind an OPAQUE_FD allocation, so handing it over
        // unsynchronised would let NVENC read a buffer the compute pass is
        // still writing — which shows up as intermittent tearing under
        // load rather than as an obvious failure. Dropping the frame
        // instead leaves the encoder with nothing to send for this tick,
        // which is visible and safe.
        for &(tw, th) in &nv12_opaque_targets {
            let Some(idx) = self.nv12_opaque_slot(sid, tw, th) else {
                continue;
            };
            let Some(sync) = shared_sync_fd.clone() else {
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!(
                        "[vulkan-render] NV12 opaque-fd target {tw}x{th} has no sync_fd; dropping frames rather than racing the encoder",
                    );
                }
                continue;
            };
            let nv12 = &self.nv12_opaque_outputs[&(sid, tw, th)].0[idx];
            let Nv12OutputKind::Buffer {
                stride, uv_offset, ..
            } = nv12.kind
            else {
                continue;
            };
            // An OPAQUE_FD slot always carries its exported fd; the `None`
            // case is the compositor's own Vulkan Video image, which
            // `nv12_opaque_slot` does not select.
            let Some(fd) = nv12.fd.clone() else { continue };
            results.push((
                toplevel_sid,
                tw,
                th,
                PixelData::Nv12OpaqueFd {
                    fd,
                    buf_id: nv12.buf_id,
                    stride,
                    uv_offset,
                    width: nv12.width,
                    height: nv12.height,
                    sync_fd: Some(sync),
                },
                false,
            ));
            if let Some(entry) = self.nv12_opaque_outputs.get_mut(&(sid, tw, th)) {
                let n = entry.0.len().max(1);
                entry.1 = (entry.1 + 1) % n;
            }
        }

        // Build immediate per-target results.  Each external target
        // emits its own SurfaceCommit so the matching per-client
        // encoder picks up the correctly-sized frame.  These return
        // immediately without waiting for the fence (the encoder VPP
        // synchronises via DMA-BUF implicit fencing or the exported
        // sync_fd we attach below).
        for &(tw, th, ext_idx) in &external_targets {
            let (ext_va, ext_va_display, ext_fd, ext_fourcc, ext_mod, ext_stride) = {
                let (ext_vec, _) = &self.external_outputs[&(sid, tw, th)];
                let ext = &ext_vec[ext_idx];
                (
                    ext.va_surface_id,
                    ext.va_display,
                    ext._fd.clone(),
                    ext.fourcc,
                    ext.modifier,
                    ext.stride,
                )
            };
            let nv12_entry = self.nv12_outputs.get(&(sid, tw, th));
            let nv12_cur_idx = nv12_entry.map_or(0, |(_, idx)| *idx);
            let nv12_len = nv12_entry.map_or(0, |(v, _)| v.len()).max(1);
            let nv12_idx = nv12_cur_idx % nv12_len;
            let mut pixel_data = if ext_va != 0 {
                PixelData::VaSurface {
                    surface_id: ext_va,
                    va_display: ext_va_display,
                    _fd: ext_fd.clone(),
                }
            } else if let Some((nv12s, _)) = nv12_entry.filter(|(v, _)| !v.is_empty())
                // Only an imported plane set can be handed to a server-side
                // encoder — the compositor's own encode image has no fd to
                // share.  It is never registered for an external target, so
                // this filter is a guard rather than a behaviour change.
                && let Some(nv12_fd) = nv12s[nv12_idx].fd.clone()
            {
                let nv12 = &nv12s[nv12_idx];
                match &nv12.kind {
                    Nv12OutputKind::Buffer {
                        stride, uv_offset, ..
                    } => PixelData::Nv12DmaBuf {
                        fd: nv12_fd,
                        stride: *stride,
                        uv_offset: *uv_offset,
                        width: tw,
                        height: th,
                        sync_fd: None,
                    },
                    Nv12OutputKind::Image { .. } => PixelData::Nv12DmaBuf {
                        fd: nv12_fd,
                        stride: 0,
                        uv_offset: 0,
                        width: tw,
                        height: th,
                        sync_fd: None,
                    },
                }
            } else {
                PixelData::DmaBuf {
                    fd: ext_fd.clone(),
                    fourcc: ext_fourcc,
                    modifier: ext_mod,
                    stride: ext_stride,
                    offset: 0,
                    // Render pass + blit both write top-down; the
                    // encoder VPP's importer treats DMA-BUFs as
                    // OpenGL-origin so we flag y_invert=true to keep
                    // the previous semantics.
                    y_invert: true,
                }
            };

            // Attach the shared sync_fd to this NV12 result so the
            // encoder can wait off-thread instead of blocking the
            // compositor.
            if let Some(ref shared) = shared_sync_fd
                && let PixelData::Nv12DmaBuf {
                    ref mut sync_fd, ..
                } = pixel_data
            {
                *sync_fd = Some(shared.clone());
            }

            results.push((toplevel_sid, tw, th, pixel_data, false));

            // Advance the round-robin cursors for this target.
            if let Some(entry) = self.external_outputs.get_mut(&(sid, tw, th)) {
                let n = entry.0.len().max(1);
                entry.1 = (entry.1 + 1) % n;
            }
            if let Some(entry) = self.nv12_outputs.get_mut(&(sid, tw, th)) {
                let n = entry.0.len().max(1);
                entry.1 = (entry.1 + 1) % n;
            }
        }

        // Drain completed deferred submits before stashing this frame.
        // Amortises cleanup with submit rate (bounded by GPU frame
        // rate) rather than Wayland event rate.
        self.drain_deferred_submits();

        // Stash the submit so retire_pending picks up the staging BGRA
        // on the next call.  This is one frame delayed because we need
        // the fence to signal before reading the staging buffer —
        // the standard self-alloc latency model.
        self.pending_submit = Some(submit_info);
        self.output_idx = (self.output_idx + 1) % self.output_images.len();

        if entry_n < 20 || entry_n.is_multiple_of(50) {
            eprintln!(
                "[render_tree_sized #{entry_n}] return targets={} prev_results={} pending={}",
                external_targets.len(),
                results.len(),
                self.pending_submit.is_some(),
            );
        }
        results
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
            self.drain_pending_destroy_targets_if_idle();
            // Destroy per-frame temp textures.
            for t in self.frame_textures.drain(..) {
                self.device.destroy_image_view(t.view, None);
                self.device.destroy_image(t.image, None);
                self.device.free_memory(t.memory, None);
            }
            // Destroy persistent surface/buffer textures and any already
            // pending destruction.  All holders of a shared texture are in
            // this list, so a single pass destroys each exactly once: a
            // failed `try_unwrap` drops one clone and a later entry — the
            // last clone — succeeds.
            let all_textures: Vec<Arc<CachedSurfaceTexture>> = self
                .surface_textures
                .drain()
                .map(|(_, tex)| tex)
                .chain(self.buffer_textures.drain().map(|(_, tex)| tex))
                .chain(self.pending_destroy_textures.drain(..))
                .collect();
            for tex in all_textures {
                let Ok(tex) = Arc::try_unwrap(tex) else {
                    continue;
                };
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
                .destroy_pipeline(self.compute_nv24_pipeline, None);
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
            // Everything above frees GPU resources and is safe at any time.
            //
            // `destroy_instance` is not, and is deliberately skipped: the
            // loader `dlclose()`s its layer and ICD libraries inside it, and
            // those libraries have registered thread-local destructors on
            // every thread that touched them.  Unmapping them leaves those
            // destructors dangling, so the next thread to exit -- or the main
            // thread reaching `__call_tls_dtors` on its way into `exit()` --
            // jumps into freed memory and dies with a SIGSEGV whose stack has
            // no frames to read.  `entry` is `ManuallyDrop` for the same
            // reason: dropping it would `dlclose()` libvulkan itself.
            //
            // The cost is one leaked VkInstance per renderer, which the
            // kernel reclaims at process exit.  There is no hook that would
            // let us do this only when it is safe -- TLS destructors run
            // before `atexit` handlers, so by the time any guard of ours
            // could fire, the damage is done.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::clamped_scissor;

    fn rect(x: i32, y: i32, w: u32, h: u32, fw: u32, fh: u32) -> (i32, i32, u32, u32) {
        let r = clamped_scissor(x, y, w, h, fw, fh);
        (r.offset.x, r.offset.y, r.extent.width, r.extent.height)
    }

    #[test]
    fn a_layer_inside_the_framebuffer_scissors_to_itself() {
        assert_eq!(rect(0, 0, 1000, 1000, 1900, 1000), (0, 0, 1000, 1000));
        assert_eq!(rect(40, 20, 100, 80, 1920, 1080), (40, 20, 100, 80));
    }

    #[test]
    fn a_layer_starting_off_the_top_left_is_clipped_to_the_origin() {
        // A toplevel with client-side decorations sits at a negative offset
        // so only its window-geometry area lands in the output.
        assert_eq!(rect(-35, -35, 200, 200, 1920, 1080), (0, 0, 165, 165));
    }

    #[test]
    fn a_layer_running_past_the_far_edge_is_clipped_to_it() {
        // The composite follows the pane, which shrinks before the window
        // painting into it does.
        assert_eq!(rect(900, 0, 1000, 500, 1000, 1000), (900, 0, 100, 500));
    }

    #[test]
    fn a_layer_entirely_outside_scissors_to_nothing() {
        // Vulkan rejects a scissor that leaves the framebuffer, so this has
        // to come back empty rather than negative.
        assert_eq!(rect(2000, 2000, 100, 100, 1000, 1000), (1000, 1000, 0, 0));
        assert_eq!(rect(-500, -500, 100, 100, 1000, 1000), (0, 0, 0, 0));
    }

    #[test]
    fn a_far_edge_that_would_overflow_i32_still_lands_inside() {
        assert_eq!(
            rect(i32::MAX - 1, 0, u32::MAX, 10, 1000, 1000),
            (1000, 0, 0, 10),
        );
    }
}
