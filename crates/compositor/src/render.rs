//! Surface compositing — layer collection for GPU (Vulkan) rendering.
//!
//! `collect_gpu_layers` gathers layer metadata for the Vulkan renderer.

use std::collections::HashMap;

use wayland_server::backend::ObjectId;

use super::imp::Surface;

/// Scale a logical dimension to physical pixels using scale_120
/// (ceil so we never lose a pixel).
#[inline]
pub(crate) fn to_physical(logical: u32, scale_120: u32) -> u32 {
    (logical * scale_120).div_ceil(120)
}

// ===================================================================
// Layer collection for GPU rendering
// ===================================================================

/// Metadata about a surface's pixel buffer, stored after commit.
#[derive(Clone, Debug)]
pub(crate) struct SurfaceMeta {
    pub width: u32,
    pub height: u32,
    pub scale: i32,
    /// Image origin is bottom-left (OpenGL/EGL DMA-BUF clients).
    pub y_invert: bool,
}

/// A single compositing layer for the GPU renderer.
pub(crate) struct GpuLayer {
    pub x: i32,
    pub y: i32,
    pub logical_w: u32,
    pub logical_h: u32,
    /// Wayland surface ObjectId — the VulkanRenderer looks up the
    /// cached texture by this key.
    pub surface_id: ObjectId,
    /// Image origin is bottom-left (OpenGL/EGL clients).
    pub y_invert: bool,
    /// The part of the texture this layer shows, as `(u, v, w, h)` in 0..1
    /// texture coordinates, when `wp_viewport.set_source` cropped it.
    /// `None` means the whole buffer, which is the common case.
    pub src: Option<(f32, f32, f32, f32)>,
}

/// The surface's size in surface-local (logical) units.
///
/// `wp_viewport` overrides the buffer's own size in two steps, and both
/// have to be honoured in the same order the protocol defines them:
/// `set_destination` names the size outright, and failing that a
/// `set_source` crop makes the surface the size of the crop — not of the
/// buffer behind it. Only with neither does the buffer size divided by
/// `buffer_scale` win.
pub(crate) fn surface_logical_size(surf: &Surface, sm: &SurfaceMeta) -> (f64, f64) {
    if let Some((dw, dh)) = surf.viewport_destination.filter(|&(w, h)| w > 0 && h > 0) {
        return (dw as f64, dh as f64);
    }
    if let Some((_, _, sw, sh)) = surf
        .viewport_source
        .filter(|&(_, _, w, h)| w > 0.0 && h > 0.0)
    {
        // The protocol requires an integer size here; a client that sends a
        // fractional one is out of spec, and rounding up loses less of the
        // window than truncating.
        return (sw.ceil(), sh.ceil());
    }
    let s = f64::from(sm.scale.max(1));
    (f64::from(sm.width) / s, f64::from(sm.height) / s)
}

/// Collect layers for GPU compositing.  Each layer carries a surface ID
/// so the Vulkan renderer can look up its cached texture.
pub(crate) fn collect_gpu_layers(
    surface_id: &ObjectId,
    surfaces: &HashMap<ObjectId, Surface>,
    meta: &HashMap<ObjectId, SurfaceMeta>,
    parent_x: i32,
    parent_y: i32,
    layers: &mut Vec<GpuLayer>,
) {
    let Some(surf) = surfaces.get(surface_id) else {
        return;
    };
    let (x, y) = (
        parent_x + surf.subsurface_position.0,
        parent_y + surf.subsurface_position.1,
    );

    if let Some(sm) = meta.get(surface_id) {
        let (lw, lh) = surface_logical_size(surf, sm);
        let (lw, lh) = (lw as u32, lh as u32);
        // The crop is given in surface-local units; the renderer samples in
        // texture coordinates, so normalise by the whole buffer's own
        // surface-local extent.
        let s = f64::from(sm.scale.max(1));
        let (bw, bh) = (f64::from(sm.width) / s, f64::from(sm.height) / s);
        let src = surf
            .viewport_source
            .filter(|&(_, _, w, h)| w > 0.0 && h > 0.0)
            .filter(|_| bw > 0.0 && bh > 0.0)
            .map(|(sx, sy, sw, sh)| {
                (
                    (sx / bw) as f32,
                    (sy / bh) as f32,
                    (sw / bw) as f32,
                    (sh / bh) as f32,
                )
            })
            // A crop that covers the whole buffer is the same as no crop,
            // and saying so keeps the renderer on its ordinary path.
            .filter(|&(u, v, w, h)| (u, v, w, h) != (0.0, 0.0, 1.0, 1.0));
        static DBG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = DBG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 20 || n.is_multiple_of(1000) {
            eprintln!(
                "[gpu-layer #{n}] sid={surface_id:?} pos=({x},{y}) pixel={}x{} scale={} viewport={:?} source={:?} logical={}x{}",
                sm.width,
                sm.height,
                sm.scale,
                surf.viewport_destination,
                surf.viewport_source,
                lw,
                lh,
            );
        }
        layers.push(GpuLayer {
            x,
            y,
            logical_w: lw,
            logical_h: lh,
            surface_id: surface_id.clone(),
            y_invert: sm.y_invert,
            src,
        });
    }

    for child_id in &surf.children {
        collect_gpu_layers(child_id, surfaces, meta, x, y, layers);
    }
}
