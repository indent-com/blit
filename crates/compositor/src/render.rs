//! Surface compositing — layer collection for GPU (Vulkan) rendering.
//!
//! `collect_gpu_layers` gathers layer metadata for the Vulkan renderer.

use std::collections::HashMap;

use wayland_server::backend::ObjectId;

use super::imp::{PixelData, Surface};

/// Scale a logical dimension to physical pixels using scale_120
/// (ceil so we never lose a pixel).
#[inline]
pub(crate) fn to_physical(logical: u32, scale_120: u32) -> u32 {
    (logical * scale_120).div_ceil(120)
}

// ===================================================================
// Layer collection for GPU rendering
// ===================================================================

/// A single compositing layer for the GPU renderer.
pub(crate) struct GpuLayer<'a> {
    pub x: i32,
    pub y: i32,
    pub logical_w: u32,
    pub logical_h: u32,
    pub pixel_w: u32,
    pub pixel_h: u32,
    pub pixels: &'a PixelData,
    /// Image origin is bottom-left (OpenGL/EGL clients).
    pub y_invert: bool,
}

/// Collect layers for GPU compositing.  Each layer carries a reference to
/// the original `PixelData` so we can distinguish DMA-BUF vs SHM.
pub(crate) fn collect_gpu_layers<'a>(
    surface_id: &ObjectId,
    surfaces: &HashMap<ObjectId, Surface>,
    cache: &'a HashMap<ObjectId, (u32, u32, i32, bool, PixelData)>,
    parent_x: i32,
    parent_y: i32,
    layers: &mut Vec<GpuLayer<'a>>,
) {
    let Some(surf) = surfaces.get(surface_id) else {
        return;
    };
    let (x, y) = (
        parent_x + surf.subsurface_position.0,
        parent_y + surf.subsurface_position.1,
    );

    if let Some((w, h, scale, _is_opaque, pixels)) = cache.get(surface_id)
        && !pixels.is_empty()
    {
        let s = (*scale).max(1) as u32;
        // Prefer viewport destination (see collect_layers for rationale).
        let (lw, lh) = surf
            .viewport_destination
            .filter(|&(dw, dh)| dw > 0 && dh > 0)
            .map(|(dw, dh)| (dw as u32, dh as u32))
            .unwrap_or((*w / s, *h / s));
        static DBG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = DBG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 20 || n.is_multiple_of(1000) {
            eprintln!(
                "[gpu-layer #{n}] sid={surface_id:?} pos=({x},{y}) pixel={}x{} scale={} viewport={:?} logical={}x{}",
                w, h, scale, surf.viewport_destination, lw, lh,
            );
        }
        let y_invert = matches!(pixels, PixelData::DmaBuf { y_invert: true, .. });
        layers.push(GpuLayer {
            x,
            y,
            logical_w: lw,
            logical_h: lh,
            pixel_w: *w,
            pixel_h: *h,
            pixels,
            y_invert,
        });
    }

    for child_id in &surf.children {
        collect_gpu_layers(child_id, surfaces, cache, x, y, layers);
    }
}
