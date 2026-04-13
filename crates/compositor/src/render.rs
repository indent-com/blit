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
        let s = (sm.scale).max(1) as u32;
        // Prefer viewport destination (see collect_layers for rationale).
        let (lw, lh) = surf
            .viewport_destination
            .filter(|&(dw, dh)| dw > 0 && dh > 0)
            .map(|(dw, dh)| (dw as u32, dh as u32))
            .unwrap_or((sm.width / s, sm.height / s));
        static DBG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = DBG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 20 || n.is_multiple_of(1000) {
            eprintln!(
                "[gpu-layer #{n}] sid={surface_id:?} pos=({x},{y}) pixel={}x{} scale={} viewport={:?} logical={}x{}",
                sm.width, sm.height, sm.scale, surf.viewport_destination, lw, lh,
            );
        }
        layers.push(GpuLayer {
            x,
            y,
            logical_w: lw,
            logical_h: lh,
            surface_id: surface_id.clone(),
            y_invert: sm.y_invert,
        });
    }

    for child_id in &surf.children {
        collect_gpu_layers(child_id, surfaces, meta, x, y, layers);
    }
}
