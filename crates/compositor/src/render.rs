//! Surface compositing — CPU fallback and layer collection for GPU rendering.
//!
//! `cpu_composite_from_cache` performs software compositing of a surface tree.
//! `collect_gpu_layers` gathers layer metadata for the external GPU renderer.

use std::collections::HashMap;
use std::sync::Arc;

use wayland_server::backend::ObjectId;

use super::imp::{PixelData, Surface};

// ===================================================================
// CPU composite (unchanged public API)
// ===================================================================

struct Layer {
    x: i32,
    y: i32,
    logical_w: u32,
    logical_h: u32,
    pixel_w: u32,
    scale: i32,
    is_opaque: bool,
    rgba: Vec<u8>,
}

/// Scale a logical dimension to physical pixels using scale_120
/// (ceil so we never lose a pixel).
#[inline]
pub(crate) fn to_physical(logical: u32, scale_120: u32) -> u32 {
    (logical * scale_120).div_ceil(120)
}

/// CPU-composite a surface tree from the per-surface pixel cache.
///
/// `output_scale_120` is in 1/120th units (120 = 1×, 240 = 2×).
/// The composited image is output at full physical resolution so the
/// encoder / browser receives the native pixel density.
///
/// Returns `(width, height, PixelData)` or None if no renderable content.
pub(crate) fn cpu_composite_from_cache(
    root_surface_id: &ObjectId,
    surfaces: &HashMap<ObjectId, Surface>,
    cache: &HashMap<ObjectId, (u32, u32, i32, bool, PixelData)>,
    output_scale_120: u16,
) -> Option<(u32, u32, PixelData)> {
    let s120 = (output_scale_120 as u32).max(120);

    let mut layers: Vec<Layer> = Vec::new();
    collect_layers(root_surface_id, surfaces, cache, 0, 0, &mut layers);

    if layers.is_empty() {
        return None;
    }

    // Determine xdg_geometry crop from root surface (logical coords).
    let (crop_x, crop_y, log_w, log_h) = surfaces
        .get(root_surface_id)
        .and_then(|s| s.xdg_geometry)
        .filter(|&(_, _, w, h)| w > 0 && h > 0)
        .map(|(x, y, w, h)| (x, y, w as u32, h as u32))
        .unwrap_or_else(|| {
            let (mut mw, mut mh) = (0i32, 0i32);
            for l in &layers {
                mw = mw.max(l.x + l.logical_w as i32);
                mh = mh.max(l.y + l.logical_h as i32);
            }
            (0, 0, mw.max(0) as u32, mh.max(0) as u32)
        });

    if log_w == 0 || log_h == 0 {
        return None;
    }

    // Physical output dimensions.
    let phys_w = to_physical(log_w, s120) as usize;
    let phys_h = to_physical(log_h, s120) as usize;

    let stride = phys_w * 4;
    let mut out = vec![0u8; stride * phys_h];

    for layer in &layers {
        // Layer position in logical coords.
        let (lx, ly) = if layer.logical_w == log_w && layer.logical_h == log_h {
            (layer.x, layer.y)
        } else {
            (layer.x - crop_x, layer.y - crop_y)
        };
        let buf_s = layer.scale.max(1) as usize;
        let pw = layer.pixel_w as usize;

        // Physical extent of this layer in the output.
        let layer_phys_w = to_physical(layer.logical_w, s120) as usize;
        let layer_phys_h = to_physical(layer.logical_h, s120) as usize;

        for prow in 0..layer_phys_h {
            let dy = (ly as i64) * (s120 as i64) / 120 + prow as i64;
            if dy < 0 || dy >= phys_h as i64 {
                continue;
            }
            for pcol in 0..layer_phys_w {
                let dx = (lx as i64) * (s120 as i64) / 120 + pcol as i64;
                if dx < 0 || dx >= phys_w as i64 {
                    continue;
                }

                // Map output physical pixel to source buffer pixel.
                let src_row = prow * buf_s * 120 / (s120 as usize);
                let src_col = pcol * buf_s * 120 / (s120 as usize);
                let src_off = (src_row * pw + src_col) * 4;
                let dst_off = dy as usize * stride + dx as usize * 4;
                if src_off + 3 >= layer.rgba.len() || dst_off + 3 >= out.len() {
                    continue;
                }

                let (sr, sg, sb) = (
                    layer.rgba[src_off],
                    layer.rgba[src_off + 1],
                    layer.rgba[src_off + 2],
                );
                let sa = if layer.is_opaque {
                    255u32
                } else {
                    layer.rgba[src_off + 3] as u32
                };

                if sa == 0 {
                    continue;
                }
                if sa == 255 {
                    out[dst_off] = sr;
                    out[dst_off + 1] = sg;
                    out[dst_off + 2] = sb;
                    out[dst_off + 3] = 255;
                } else {
                    let inv = 255 - sa;
                    out[dst_off] = ((sr as u32 * sa + out[dst_off] as u32 * inv) / 255) as u8;
                    out[dst_off + 1] =
                        ((sg as u32 * sa + out[dst_off + 1] as u32 * inv) / 255) as u8;
                    out[dst_off + 2] =
                        ((sb as u32 * sa + out[dst_off + 2] as u32 * inv) / 255) as u8;
                    out[dst_off + 3] = 255;
                }
            }
        }
    }

    // Output is RGBA from to_rgba().
    Some((phys_w as u32, phys_h as u32, PixelData::Rgba(Arc::new(out))))
}

fn collect_layers(
    surface_id: &ObjectId,
    surfaces: &HashMap<ObjectId, Surface>,
    cache: &HashMap<ObjectId, (u32, u32, i32, bool, PixelData)>,
    parent_x: i32,
    parent_y: i32,
    layers: &mut Vec<Layer>,
) {
    let Some(surf) = surfaces.get(surface_id) else {
        return;
    };
    let (x, y) = (
        parent_x + surf.subsurface_position.0,
        parent_y + surf.subsurface_position.1,
    );

    if let Some((w, h, scale, is_opaque, pixels)) = cache.get(surface_id) {
        let rgba = pixels.to_rgba(*w, *h);
        if !rgba.is_empty() {
            let s = (*scale).max(1) as u32;
            // Prefer viewport destination (logical size declared by the client
            // via wp_viewport.set_destination) over buffer_scale division.
            // Fractional-scale-aware clients (e.g. Chromium) render at physical
            // resolution with buffer_scale=1 and use the viewport to declare
            // logical size.  Traditional clients (e.g. Firefox) set
            // buffer_scale=2 and don't use the viewport.
            let (lw, lh) = surf
                .viewport_destination
                .filter(|&(dw, dh)| dw > 0 && dh > 0)
                .map(|(dw, dh)| (dw as u32, dh as u32))
                .unwrap_or((*w / s, *h / s));
            layers.push(Layer {
                x,
                y,
                logical_w: lw,
                logical_h: lh,
                pixel_w: *w,
                scale: *scale,
                is_opaque: *is_opaque,
                rgba,
            });
        }
    }

    for child_id in &surf.children {
        collect_layers(child_id, surfaces, cache, x, y, layers);
    }
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
        layers.push(GpuLayer {
            x,
            y,
            logical_w: lw,
            logical_h: lh,
            pixel_w: *w,
            pixel_h: *h,
            pixels,
        });
    }

    for child_id in &surf.children {
        collect_gpu_layers(child_id, surfaces, cache, x, y, layers);
    }
}
