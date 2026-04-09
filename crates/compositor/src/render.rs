//! GPU surface renderer using Smithay's GlesRenderer.
//!
//! Renders a Wayland surface tree (including subsurfaces) to an offscreen
//! buffer and reads back the composed BGRA pixels.  Handles SHM and DMA-BUF
//! import, cross-process fencing, format conversion — all via Smithay.

use smithay::backend::allocator::Fourcc;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::gles::{GlesRenderbuffer, GlesRenderer};
use smithay::backend::renderer::utils::import_surface_tree;
use smithay::backend::renderer::{
    Bind, Color32F, ExportMem, Frame, Offscreen, Renderer, TextureMapping,
};
use smithay::utils::{Physical, Point, Rectangle, Scale, Size, Transform};

/// GPU-accelerated surface renderer.
pub struct SurfaceRenderer {
    renderer: GlesRenderer,
}

impl SurfaceRenderer {
    /// Try to create a renderer from a DRM render node path.
    /// Returns None if the GPU or EGL isn't available.
    pub fn try_new(drm_device: &str) -> Option<Self> {
        let drm_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(drm_device)
            .map_err(|e| eprintln!("[renderer] failed to open {drm_device}: {e}"))
            .ok()?;

        let gbm = smithay::reexports::gbm::Device::new(drm_file)
            .map_err(|e| eprintln!("[renderer] GBM device failed: {e}"))
            .ok()?;

        let egl_display = unsafe { EGLDisplay::new(gbm) }
            .map_err(|e| eprintln!("[renderer] EGL display failed: {e}"))
            .ok()?;

        let egl_context = EGLContext::new(&egl_display)
            .map_err(|e| eprintln!("[renderer] EGL context failed: {e}"))
            .ok()?;

        let renderer = unsafe { GlesRenderer::new(egl_context) }
            .map_err(|e| eprintln!("[renderer] GlesRenderer failed: {e}"))
            .ok()?;

        eprintln!("[renderer] initialized on {drm_device}");

        Some(Self { renderer })
    }

    /// Render a surface tree to BGRA pixels.
    ///
    /// Render a surface tree to BGRA pixels, cropped to `geometry` if
    /// provided (excludes CSD shadows/decorations).
    pub fn render_surface(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        geometry: Option<&Rectangle<i32, smithay::utils::Logical>>,
    ) -> Option<(u32, u32, Vec<u8>)> {
        use std::time::Instant;
        let t0 = Instant::now();

        // Import SHM buffers only.  DMA-BUF import via EGL hangs on AMD
        // due to implicit fence sync issues with cross-process buffers.
        // import_surface_tree would import both SHM and DMA-BUF — instead
        // we import each surface individually, skipping DMA-BUF.
        // Check if any DMA-BUF surface lacks explicit sync — those would
        // hang on EGL import due to implicit fence waits.  If all DMA-BUFs
        // have explicit sync (or the surface is SHM-only), proceed with
        // GPU rendering.  Otherwise bail to CPU fallback.
        {
            use smithay::wayland::compositor::{
                BufferAssignment, SurfaceAttributes, TraversalAction, with_surface_tree_downward,
            };
            use smithay::wayland::drm_syncobj::DrmSyncobjCachedState;
            let mut has_unsafe_dmabuf = false;
            with_surface_tree_downward(
                surface,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |_wl, states, _| {
                    let mut guard = states.cached_state.get::<SurfaceAttributes>();
                    let attrs = guard.current();
                    let is_dmabuf = matches!(
                        attrs.buffer.as_ref(),
                        Some(BufferAssignment::NewBuffer(buf))
                            if smithay::wayland::dmabuf::get_dmabuf(buf).is_ok()
                    );
                    if is_dmabuf {
                        let mut sync_guard = states.cached_state.get::<DrmSyncobjCachedState>();
                        let sync = sync_guard.current();
                        if sync.acquire_point.is_none() {
                            has_unsafe_dmabuf = true;
                        }
                    }
                },
                |_, _, _| true,
            );
            if has_unsafe_dmabuf {
                // Fall back to CPU — this surface has DMA-BUF without
                // explicit sync, which would hang on EGL import.
                return None;
            }
        }

        // All buffers are either SHM or DMA-BUF with explicit sync — safe
        // to import via EGL.
        let _ = import_surface_tree(&mut self.renderer, surface);
        let t1 = Instant::now();

        // Compute crop offset from geometry (excludes CSD shadows).
        let (crop_x, crop_y) = geometry
            .filter(|g| g.size.w > 0 && g.size.h > 0)
            .map(|g| (g.loc.x, g.loc.y))
            .unwrap_or((0, 0));

        // Get render elements, offset by -geometry.loc so the content
        // area maps to (0,0) and shadows are clipped.
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            render_elements_from_surface_tree(
                &mut self.renderer,
                surface,
                Point::from((-crop_x, -crop_y)),
                Scale::from(1.0),
                1.0,
                Kind::Unspecified,
            );
        let t2 = Instant::now();

        eprintln!(
            "[renderer] {} elements, crop=({crop_x},{crop_y})",
            elements.len()
        );
        if elements.is_empty() {
            return None;
        }

        // Output size: geometry size if available, otherwise derive from elements.
        let (w, h) = geometry
            .filter(|g| g.size.w > 0 && g.size.h > 0)
            .map(|g| (g.size.w, g.size.h))
            .unwrap_or_else(|| {
                use smithay::backend::renderer::element::Element;
                let mut max_w = 0i32;
                let mut max_h = 0i32;
                for elem in &elements {
                    let g = elem.geometry(Scale::from(1.0));
                    max_w = max_w.max(g.loc.x + g.size.w);
                    max_h = max_h.max(g.loc.y + g.size.h);
                }
                (max_w, max_h)
            });
        if w <= 0 || h <= 0 {
            return None;
        }

        // Create offscreen renderbuffer.
        let size = Size::from((w, h));
        let mut rb: GlesRenderbuffer = Offscreen::<GlesRenderbuffer>::create_buffer(
            &mut self.renderer,
            Fourcc::Abgr8888,
            size,
        )
        .ok()?;
        let t3 = Instant::now();

        let output_size: Size<i32, Physical> = (w, h).into();
        let damage = [Rectangle::from_size(output_size)];

        // Render to offscreen buffer.
        {
            let mut target = self.renderer.bind(&mut rb).ok()?;
            let mut frame = self
                .renderer
                .render(&mut target, output_size, Transform::Normal)
                .ok()?;

            frame.clear(Color32F::TRANSPARENT, &damage).ok()?;

            for elem in &elements {
                use smithay::backend::renderer::element::{Element, RenderElement};
                let src = elem.src();
                let geo = elem.geometry(Scale::from(1.0));
                let _ = elem.draw(&mut frame, src, geo, &damage, &[]);
            }

            let _sync = frame.finish().ok()?;
        }
        let t4 = Instant::now();

        // Read back pixels.
        let target2 = self.renderer.bind(&mut rb).ok()?;
        let t5 = Instant::now();
        eprintln!(
            "[renderer] {w}x{h} {}elem import={:?} elements={:?} create={:?} render={:?} bind2={:?}",
            elements.len(),
            t1 - t0,
            t2 - t1,
            t3 - t2,
            t4 - t3,
            t5 - t4,
        );
        let region = Rectangle::from_size(size);
        let mapping = self
            .renderer
            .copy_framebuffer(&target2, region, Fourcc::Abgr8888)
            .ok()?;
        let t6 = Instant::now();
        let pixels = self.renderer.map_texture(&mapping).ok()?;
        let t7 = Instant::now();
        eprintln!("[renderer] copy={:?} map={:?}", t6 - t5, t7 - t6);

        // Pixels may be y-flipped.
        let row_bytes = w as usize * 4;
        let total = row_bytes * h as usize;
        let mut bgra = Vec::with_capacity(total);
        if !mapping.flipped() {
            for y in (0..h as usize).rev() {
                let start = y * row_bytes;
                bgra.extend_from_slice(&pixels[start..start + row_bytes]);
            }
        } else {
            bgra.extend_from_slice(&pixels[..total]);
        }

        // ABGR8888 readback = RGBA in memory.  Convert to BGRA for encoder.
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2); // R ↔ B
        }
        Some((w as u32, h as u32, bgra))
    }

    /// Get the renderer's supported DMA-BUF formats (for DmabufFeedback).
    #[allow(dead_code)]
    pub fn dmabuf_formats(&self) -> smithay::backend::allocator::format::FormatSet {
        use smithay::backend::renderer::ImportDma;
        self.renderer.dmabuf_formats()
    }

    /// Get the EGL display (for DmabufFeedback device).
    #[allow(dead_code)]
    pub fn egl_display(&self) -> &EGLDisplay {
        self.renderer.egl_context().display()
    }
}
