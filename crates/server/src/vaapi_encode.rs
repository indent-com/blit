//! Direct VA-API H.264 and H.265/HEVC encoders — no ffmpeg dependency.
//!
//! Uses `dlopen("libva.so")` and `dlopen("libva-drm.so")` at runtime.
//! Implements Constrained Baseline profile H.264 and Main profile HEVC
//! encoding via the VA-API EncSliceLP (Low Power) or EncSlice entrypoint.
//!
//! The parameter buffer structs are accessed via raw byte arrays at
//! verified offsets rather than `#[repr(C)]` struct translation, since
//! the VA-API header structs contain complex bitfields and large padding
//! arrays that are fragile to replicate in Rust.

#![allow(non_upper_case_globals, clippy::identity_op, dead_code)]

use crate::gpu_libs::{
    self, VA_STATUS_SUCCESS, VABufferID, VAConfigID, VAContextID, VADisplay, VASurfaceID,
};
use std::ffi::c_void;
use std::os::fd::{AsRawFd, OwnedFd};
use std::ptr;

// ---------------------------------------------------------------------------
// VA-API constants
// ---------------------------------------------------------------------------

// Profiles
const VAProfileH264ConstrainedBaseline: i32 = 6;

// Entrypoints
const VAEntrypointEncSliceLP: i32 = 8;
const VAEntrypointEncSlice: i32 = 6;

// RT formats
const VA_RT_FORMAT_YUV420: u32 = 0x00000001;

// Buffer types
const VAEncCodedBufferType: i32 = 21;
const VAEncSequenceParameterBufferType: i32 = 22;
const VAEncPictureParameterBufferType: i32 = 23;
const VAEncSliceParameterBufferType: i32 = 24;

// Surface sentinel
const VA_INVALID_SURFACE: u32 = 0xFFFF_FFFF;
const VA_PICTURE_H264_INVALID: u32 = 0x01;

// Struct sizes (from va_enc_h264.h on VA-API 1.23, verified via offsetof)
const SPS_SIZE: usize = 1132;
const PPS_SIZE: usize = 648;
const SLICE_SIZE: usize = 3140;
const VA_IMAGE_SIZE: usize = 120;

// Coded buffer segment
const CBS_SIZE_OFF: usize = 0;
const CBS_BUF_OFF: usize = 16;
const CBS_NEXT_OFF: usize = 24;

// VAImage offsets
const VAIMG_BUF_OFF: usize = 52;
const VAIMG_PITCHES_OFF: usize = 68;
const VAIMG_OFFSETS_OFF: usize = 80;
const VAIMG_ID_OFF: usize = 0;

// ---------------------------------------------------------------------------
// Helper: write a value at an offset in a byte buffer
// ---------------------------------------------------------------------------

fn w8(buf: &mut [u8], off: usize, val: u8) {
    buf[off] = val;
}
fn w16(buf: &mut [u8], off: usize, val: u16) {
    buf[off..off + 2].copy_from_slice(&val.to_ne_bytes());
}
fn w32(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_ne_bytes());
}
fn r32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// VaapiDirectEncoder
// ---------------------------------------------------------------------------

/// Number of reconstructed reference surfaces (double-buffered).
const NUM_REF_SURFACES: usize = 2;
/// Number of input surfaces.
const NUM_INPUT_SURFACES: usize = 1;
const TOTAL_SURFACES: usize = NUM_REF_SURFACES + NUM_INPUT_SURFACES;

pub struct VaapiDirectEncoder {
    va: &'static gpu_libs::VaFns,
    display: VADisplay,
    config: VAConfigID,
    context: VAContextID,
    surfaces: [VASurfaceID; TOTAL_SURFACES],
    coded_buf: VABufferID,
    width: u32,
    height: u32,
    width_in_mbs: u16,
    height_in_mbs: u16,
    frame_num: u16,
    idr_num: u32,
    force_idr: bool,
    cur_ref_idx: usize,
    _drm_fd: OwnedFd,
}

unsafe impl Send for VaapiDirectEncoder {}

impl VaapiDirectEncoder {
    pub fn try_new(width: u32, height: u32, vaapi_device: &str) -> Result<Self, String> {
        let va = gpu_libs::va().ok_or("libva.so not found")?;
        let va_drm = gpu_libs::va_drm().ok_or("libva-drm.so not found")?;

        // Open render node
        let drm_fd = {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(vaapi_device)
                .map_err(|e| format!("failed to open {vaapi_device}: {e}"))?;
            OwnedFd::from(file)
        };

        let display = unsafe { (va_drm.vaGetDisplayDRM)(drm_fd.as_raw_fd()) };
        if display.is_null() {
            return Err("vaGetDisplayDRM returned null".into());
        }

        let mut major = 0i32;
        let mut minor = 0i32;
        let st = unsafe { (va.vaInitialize)(display, &mut major, &mut minor) };
        if st != VA_STATUS_SUCCESS {
            return Err(format!("vaInitialize failed: {st}"));
        }

        // Probe for EncSliceLP or EncSlice on H264ConstrainedBaseline
        let mut entrypoints = [0i32; 16];
        let mut num_ep = 0i32;
        unsafe {
            (va.vaQueryConfigEntrypoints)(
                display,
                VAProfileH264ConstrainedBaseline,
                entrypoints.as_mut_ptr(),
                &mut num_ep,
            );
        }
        let ep_slice = &entrypoints[..num_ep as usize];
        let entrypoint = if ep_slice.contains(&VAEntrypointEncSliceLP) {
            VAEntrypointEncSliceLP
        } else if ep_slice.contains(&VAEntrypointEncSlice) {
            VAEntrypointEncSlice
        } else {
            unsafe {
                (va.vaTerminate)(display);
            }
            return Err("H.264 encode not supported on this VA-API device".into());
        };

        // Create config
        let mut config: VAConfigID = 0;
        let st = unsafe {
            (va.vaCreateConfig)(
                display,
                VAProfileH264ConstrainedBaseline,
                entrypoint,
                ptr::null_mut(),
                0,
                &mut config,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateConfig failed: {st}"));
        }

        // Create surfaces: 2 reference + 1 input
        let mut surfaces = [0u32; TOTAL_SURFACES];
        let st = unsafe {
            (va.vaCreateSurfaces)(
                display,
                VA_RT_FORMAT_YUV420,
                width,
                height,
                surfaces.as_mut_ptr(),
                TOTAL_SURFACES as u32,
                ptr::null_mut(),
                0,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroyConfig)(display, config);
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateSurfaces failed: {st}"));
        }

        // Create context
        let mut context: VAContextID = 0;
        let st = unsafe {
            (va.vaCreateContext)(
                display,
                config,
                width as i32,
                height as i32,
                0x00000002, // VA_PROGRESSIVE
                surfaces.as_mut_ptr(),
                TOTAL_SURFACES as i32,
                &mut context,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroySurfaces)(display, surfaces.as_mut_ptr(), TOTAL_SURFACES as i32);
                (va.vaDestroyConfig)(display, config);
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateContext failed: {st}"));
        }

        // Coded buffer (output bitstream) — allocate generously
        let coded_buf_size = width * height; // ~1 byte per pixel is generous
        let mut coded_buf: VABufferID = 0;
        let st = unsafe {
            (va.vaCreateBuffer)(
                display,
                context,
                VAEncCodedBufferType,
                coded_buf_size,
                1,
                ptr::null_mut(),
                &mut coded_buf,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroyContext)(display, context);
                (va.vaDestroySurfaces)(display, surfaces.as_mut_ptr(), TOTAL_SURFACES as i32);
                (va.vaDestroyConfig)(display, config);
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateBuffer(coded) failed: {st}"));
        }

        let width_in_mbs = width.div_ceil(16) as u16;
        let height_in_mbs = height.div_ceil(16) as u16;

        eprintln!(
            "[vaapi-direct] initialized H.264 CB encoder for {width}x{height} (ep={entrypoint})"
        );

        Ok(Self {
            va,
            display,
            config,
            context,
            surfaces,
            coded_buf,
            width,
            height,
            width_in_mbs,
            height_in_mbs,
            frame_num: 0,
            idr_num: 0,
            force_idr: false,
            cur_ref_idx: 0,
            _drm_fd: drm_fd,
        })
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    /// Encode an NV12 frame (Y + UV interleaved planes).
    pub fn encode_nv12(
        &mut self,
        y_data: &[u8],
        uv_data: &[u8],
        y_stride: usize,
        uv_stride: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let input_surface = self.surfaces[NUM_REF_SURFACES]; // last surface is input

        // Upload NV12 data to the input surface
        self.upload_nv12(input_surface, y_data, uv_data, y_stride, uv_stride)?;
        self.encode_surface(input_surface)
    }

    /// Encode from BGRA pixels — converts to NV12 and uploads.
    pub fn encode_bgra_padded(
        &mut self,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let input_surface = self.surfaces[NUM_REF_SURFACES];

        // Convert BGRA→NV12 and upload to surface
        self.upload_bgra(input_surface, bgra, src_w, src_h)?;
        self.encode_surface(input_surface)
    }

    fn upload_nv12(
        &self,
        surface: VASurfaceID,
        y_data: &[u8],
        uv_data: &[u8],
        src_y_stride: usize,
        src_uv_stride: usize,
    ) -> Option<()> {
        let mut image = [0u8; VA_IMAGE_SIZE];
        let st = unsafe {
            (self.va.vaDeriveImage)(self.display, surface, image.as_mut_ptr() as *mut c_void)
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }

        let image_id = r32(&image, VAIMG_ID_OFF);
        let buf_id = r32(&image, VAIMG_BUF_OFF);
        let y_pitch = r32(&image, VAIMG_PITCHES_OFF) as usize;
        let uv_pitch = r32(&image, VAIMG_PITCHES_OFF + 4) as usize;
        let y_offset = r32(&image, VAIMG_OFFSETS_OFF) as usize;
        let uv_offset = r32(&image, VAIMG_OFFSETS_OFF + 4) as usize;

        let mut map_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, buf_id, &mut map_ptr) };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (self.va.vaDestroyImage)(self.display, image_id);
            }
            return None;
        }

        let w = self.width as usize;
        let h = self.height as usize;
        let dst = map_ptr as *mut u8;
        unsafe {
            // Copy Y plane
            for row in 0..h {
                let sr = row.min(h - 1);
                let src_start = sr * src_y_stride;
                let dst_start = y_offset + row * y_pitch;
                let copy_len = w.min(y_data.len() - src_start);
                ptr::copy_nonoverlapping(
                    y_data.as_ptr().add(src_start),
                    dst.add(dst_start),
                    copy_len,
                );
            }
            // Copy UV plane
            let uv_h = h / 2;
            for row in 0..uv_h {
                let src_start = row * src_uv_stride;
                let dst_start = uv_offset + row * uv_pitch;
                let copy_len = w.min(uv_data.len() - src_start);
                ptr::copy_nonoverlapping(
                    uv_data.as_ptr().add(src_start),
                    dst.add(dst_start),
                    copy_len,
                );
            }
        }

        unsafe {
            (self.va.vaUnmapBuffer)(self.display, buf_id);
            (self.va.vaDestroyImage)(self.display, image_id);
        }
        Some(())
    }

    fn upload_bgra(
        &self,
        surface: VASurfaceID,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<()> {
        let mut image = [0u8; VA_IMAGE_SIZE];
        let st = unsafe {
            (self.va.vaDeriveImage)(self.display, surface, image.as_mut_ptr() as *mut c_void)
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }

        let image_id = r32(&image, VAIMG_ID_OFF);
        let buf_id = r32(&image, VAIMG_BUF_OFF);
        let y_pitch = r32(&image, VAIMG_PITCHES_OFF) as usize;
        let uv_pitch = r32(&image, VAIMG_PITCHES_OFF + 4) as usize;
        let y_offset = r32(&image, VAIMG_OFFSETS_OFF) as usize;
        let uv_offset = r32(&image, VAIMG_OFFSETS_OFF + 4) as usize;

        let mut map_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, buf_id, &mut map_ptr) };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (self.va.vaDestroyImage)(self.display, image_id);
            }
            return None;
        }

        let enc_w = self.width as usize;
        let enc_h = self.height as usize;
        let dst = map_ptr as *mut u8;

        // BGRA→NV12 directly into mapped surface memory
        unsafe {
            // Y plane
            for row in 0..enc_h {
                let sr = row.min(src_h - 1);
                let dst_row = dst.add(y_offset + row * y_pitch);
                for col in 0..enc_w {
                    let sc = col.min(src_w - 1);
                    let i = (sr * src_w + sc) * 4;
                    let r = bgra[i + 2] as i32;
                    let g = bgra[i + 1] as i32;
                    let b = bgra[i] as i32;
                    let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                    *dst_row.add(col) = y.clamp(0, 255) as u8;
                }
            }
            // UV plane (interleaved)
            let chroma_h = enc_h / 2;
            let chroma_w = enc_w / 2;
            for cy in 0..chroma_h {
                let dst_row = dst.add(uv_offset + cy * uv_pitch);
                for cx in 0..chroma_w {
                    let row = cy * 2;
                    let col = cx * 2;
                    let mut u_sum = 0i32;
                    let mut v_sum = 0i32;
                    for dy in 0..2usize {
                        for dx in 0..2usize {
                            let sr = (row + dy).min(src_h - 1);
                            let sc = (col + dx).min(src_w - 1);
                            let i = (sr * src_w + sc) * 4;
                            let r = bgra[i + 2] as i32;
                            let g = bgra[i + 1] as i32;
                            let b = bgra[i] as i32;
                            u_sum += ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                            v_sum += ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                        }
                    }
                    *dst_row.add(cx * 2) = (u_sum / 4).clamp(0, 255) as u8;
                    *dst_row.add(cx * 2 + 1) = (v_sum / 4).clamp(0, 255) as u8;
                }
            }
        }

        unsafe {
            (self.va.vaUnmapBuffer)(self.display, buf_id);
            (self.va.vaDestroyImage)(self.display, image_id);
        }
        Some(())
    }

    fn encode_surface(&mut self, input_surface: VASurfaceID) -> Option<(Vec<u8>, bool)> {
        let is_idr = self.force_idr || self.frame_num == 0;
        if is_idr {
            self.frame_num = 0;
            self.idr_num += 1;
            self.force_idr = false;
        }

        let ref_surface = self.surfaces[self.cur_ref_idx];
        let recon_idx = (self.cur_ref_idx + 1) % NUM_REF_SURFACES;
        let recon_surface = self.surfaces[recon_idx];

        // Submit parameter buffers
        let sps_buf = self.create_sps_buffer()?;
        let pps_buf = self.create_pps_buffer(is_idr, ref_surface, recon_surface)?;
        let slice_buf = self.create_slice_buffer(is_idr, ref_surface)?;

        let mut buffers = [sps_buf, pps_buf, slice_buf];

        let st = unsafe { (self.va.vaBeginPicture)(self.display, self.context, input_surface) };
        if st != VA_STATUS_SUCCESS {
            self.destroy_buffers(&buffers);
            return None;
        }

        let st = unsafe {
            (self.va.vaRenderPicture)(
                self.display,
                self.context,
                buffers.as_mut_ptr(),
                buffers.len() as i32,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (self.va.vaEndPicture)(self.display, self.context);
            }
            self.destroy_buffers(&buffers);
            return None;
        }

        let st = unsafe { (self.va.vaEndPicture)(self.display, self.context) };
        if st != VA_STATUS_SUCCESS {
            self.destroy_buffers(&buffers);
            return None;
        }

        // Wait for encode
        let st = unsafe { (self.va.vaSyncSurface)(self.display, input_surface) };
        if st != VA_STATUS_SUCCESS {
            self.destroy_buffers(&buffers);
            return None;
        }

        // Read bitstream
        let nal_data = self.read_coded_buffer()?;

        self.destroy_buffers(&buffers);

        // Update state
        self.frame_num += 1;
        self.cur_ref_idx = recon_idx;

        if nal_data.is_empty() {
            None
        } else {
            Some((nal_data, is_idr))
        }
    }

    fn create_sps_buffer(&self) -> Option<VABufferID> {
        let mut sps = [0u8; SPS_SIZE];

        // seq_parameter_set_id (offset 0, u8)
        w8(&mut sps, 0, 0);
        // level_idc (offset 1, u8) — 3.1
        w8(&mut sps, 1, 31);
        // intra_period (offset 4, u32)
        w32(&mut sps, 4, 120);
        // intra_idr_period (offset 8, u32)
        w32(&mut sps, 8, 120);
        // ip_period (offset 12, u32)
        w32(&mut sps, 12, 1);
        // bits_per_second (offset 16, u32)
        w32(&mut sps, 16, 0); // VBR
        // max_num_ref_frames (offset 20, u32)
        w32(&mut sps, 20, 1);
        // picture_width_in_mbs (offset 24, u16)
        w16(&mut sps, 24, self.width_in_mbs);
        // picture_height_in_mbs (offset 26, u16)
        w16(&mut sps, 26, self.height_in_mbs);

        // seq_fields (offset 28, u32 bitfield):
        //   chroma_format_idc: bits 0-1 = 1 (4:2:0)
        //   frame_mbs_only_flag: bit 2 = 1
        //   direct_8x8_inference_flag: bit 5 = 1
        //   log2_max_frame_num_minus4: bits 6-9 = 0
        //   pic_order_cnt_type: bits 10-11 = 2
        //   log2_max_pic_order_cnt_lsb_minus4: bits 12-15 = 0
        let seq_fields: u32 = 1         // chroma_format_idc = 1
            | (1 << 2)                   // frame_mbs_only_flag
            | (1 << 5)                   // direct_8x8_inference_flag
            | (0 << 6)                   // log2_max_frame_num_minus4 = 0
            | (2 << 10); // pic_order_cnt_type = 2
        w32(&mut sps, 28, seq_fields);

        // Frame cropping for odd dimensions
        let crop_w = self.width_in_mbs as u32 * 16;
        let crop_h = self.height_in_mbs as u32 * 16;
        if crop_w != self.width || crop_h != self.height {
            w8(&mut sps, 1068, 1); // frame_cropping_flag (offset 1068, u8... actually it's at a weird offset)
            // frame_crop_right_offset (offset 1076, u32) — in chroma samples
            w32(&mut sps, 1076, (crop_w - self.width) / 2);
            // frame_crop_bottom_offset (offset 1084, u32)
            w32(&mut sps, 1084, (crop_h - self.height) / 2);
        }

        let mut buf_id: VABufferID = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncSequenceParameterBufferType,
                SPS_SIZE as u32,
                1,
                sps.as_mut_ptr() as *mut c_void,
                &mut buf_id,
            )
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        Some(buf_id)
    }

    fn create_pps_buffer(
        &self,
        is_idr: bool,
        ref_surface: VASurfaceID,
        recon_surface: VASurfaceID,
    ) -> Option<VABufferID> {
        let mut pps = [0u8; PPS_SIZE];

        // CurrPic (offset 0, VAPictureH264 — 36 bytes)
        // CurrPic.picture_id = recon_surface (the reconstructed output)
        w32(&mut pps, 0, recon_surface);
        // CurrPic.TopFieldOrderCnt
        w32(&mut pps, 12, (self.frame_num as u32) * 2);

        // ReferenceFrames[0..16] (offset 36, 16 × VAPictureH264)
        // Initialize all to invalid
        for i in 0..16 {
            let off = 36 + i * 36;
            w32(&mut pps, off, VA_INVALID_SURFACE); // picture_id
            w32(&mut pps, off + 8, VA_PICTURE_H264_INVALID); // flags
        }
        // Set ReferenceFrames[0] to the reference surface (for P-frames)
        if !is_idr && self.frame_num > 0 {
            w32(&mut pps, 36, ref_surface);
            w32(&mut pps, 36 + 8, 0); // flags = 0 (short-term ref, frame)
            w32(&mut pps, 36 + 12, ((self.frame_num - 1) as u32) * 2); // TopFieldOrderCnt
        }

        // coded_buf (offset 612, VABufferID)
        w32(&mut pps, 612, self.coded_buf);
        // pic_parameter_set_id (offset 616, u8)
        w8(&mut pps, 616, 0);
        // seq_parameter_set_id (offset 617, u8)
        w8(&mut pps, 617, 0);
        // frame_num (offset 620, u16)
        w16(&mut pps, 620, self.frame_num);
        // pic_init_qp (offset 622, u8)
        w8(&mut pps, 622, 26);
        // num_ref_idx_l0_active_minus1 (offset 623, u8)
        w8(&mut pps, 623, 0);

        // pic_fields (offset 628, u32 bitfield):
        //   idr_pic_flag: bit 0
        //   reference_pic_flag: bits 1-2 = 1
        //   deblocking_filter_control_present_flag: bit 9 = 1
        let mut pic_fields: u32 = 0;
        if is_idr {
            pic_fields |= 1; // idr_pic_flag
        }
        pic_fields |= 1 << 1; // reference_pic_flag = 1
        pic_fields |= 1 << 9; // deblocking_filter_control_present_flag
        w32(&mut pps, 628, pic_fields);

        let mut buf_id: VABufferID = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncPictureParameterBufferType,
                PPS_SIZE as u32,
                1,
                pps.as_mut_ptr() as *mut c_void,
                &mut buf_id,
            )
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        Some(buf_id)
    }

    fn create_slice_buffer(&self, is_idr: bool, ref_surface: VASurfaceID) -> Option<VABufferID> {
        let mut slice = [0u8; SLICE_SIZE];

        let num_mbs = self.width_in_mbs as u32 * self.height_in_mbs as u32;

        // macroblock_address (offset 0, u32)
        w32(&mut slice, 0, 0);
        // num_macroblocks (offset 4, u32)
        w32(&mut slice, 4, num_mbs);
        // slice_type (offset 12, u8): 2 = I, 0 = P
        w8(&mut slice, 12, if is_idr { 2 } else { 0 });

        // RefPicList0[0..32] (offset 36, 32 × VAPictureH264)
        // Initialize all to invalid
        for i in 0..32 {
            let off = 36 + i * 36;
            w32(&mut slice, off, VA_INVALID_SURFACE);
            w32(&mut slice, off + 8, VA_PICTURE_H264_INVALID);
        }
        // RefPicList1[0..32] (offset 1188, 32 × VAPictureH264)
        for i in 0..32 {
            let off = 1188 + i * 36;
            w32(&mut slice, off, VA_INVALID_SURFACE);
            w32(&mut slice, off + 8, VA_PICTURE_H264_INVALID);
        }
        // Set RefPicList0[0] for P-frames
        if !is_idr && self.frame_num > 0 {
            w32(&mut slice, 36, ref_surface);
            w32(&mut slice, 36 + 8, 0);
            w32(&mut slice, 36 + 12, ((self.frame_num - 1) as u32) * 2);
        }

        // slice_qp_delta (offset 3119, i8)
        slice[3119] = (23i8 - 26) as u8; // QP=23, pic_init_qp=26, delta = -3

        let mut buf_id: VABufferID = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncSliceParameterBufferType,
                SLICE_SIZE as u32,
                1,
                slice.as_mut_ptr() as *mut c_void,
                &mut buf_id,
            )
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        Some(buf_id)
    }

    fn read_coded_buffer(&self) -> Option<Vec<u8>> {
        let mut buf_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, self.coded_buf, &mut buf_ptr) };
        if st != VA_STATUS_SUCCESS {
            return None;
        }

        let mut nal_data = Vec::new();
        let mut seg_ptr = buf_ptr as *const u8;
        loop {
            if seg_ptr.is_null() {
                break;
            }
            let size = unsafe { u32::from_ne_bytes(*(seg_ptr as *const [u8; 4])) } as usize;
            let data_ptr = unsafe {
                let p = seg_ptr.add(CBS_BUF_OFF);
                *(p as *const *const u8)
            };
            if !data_ptr.is_null() && size > 0 {
                let data = unsafe { std::slice::from_raw_parts(data_ptr, size) };
                nal_data.extend_from_slice(data);
            }
            let next = unsafe {
                let p = seg_ptr.add(CBS_NEXT_OFF);
                *(p as *const *const u8)
            };
            seg_ptr = next;
        }

        unsafe {
            (self.va.vaUnmapBuffer)(self.display, self.coded_buf);
        }
        Some(nal_data)
    }

    fn destroy_buffers(&self, buffers: &[VABufferID]) {
        for &buf in buffers {
            unsafe {
                (self.va.vaDestroyBuffer)(self.display, buf);
            }
        }
    }
}

impl Drop for VaapiDirectEncoder {
    fn drop(&mut self) {
        unsafe {
            (self.va.vaDestroyBuffer)(self.display, self.coded_buf);
            (self.va.vaDestroyContext)(self.display, self.context);
            (self.va.vaDestroySurfaces)(
                self.display,
                self.surfaces.as_mut_ptr(),
                TOTAL_SURFACES as i32,
            );
            (self.va.vaDestroyConfig)(self.display, self.config);
            (self.va.vaTerminate)(self.display);
        }
    }
}

// ===========================================================================
// VA-API HEVC (H.265) Main profile encoder
// ===========================================================================

// Profiles
const VAProfileHEVCMain: i32 = 17;

// Struct sizes (from va_enc_hevc.h on libva 2.23, verified via offsetof)
const HEVC_SPS_SIZE: usize = 116;
const HEVC_PPS_SIZE: usize = 576;
const HEVC_SLICE_SIZE: usize = 1076;
const HEVC_PIC_SIZE: usize = 28; // sizeof(VAPictureHEVC)

// VAPictureHEVC offsets
const HEVC_PIC_ID: usize = 0; // VASurfaceID (u32)
const HEVC_PIC_POC: usize = 4; // pic_order_cnt (i32)
const HEVC_PIC_FLAGS: usize = 8; // flags (u32)

// VAPictureHEVC flags
const VA_PICTURE_HEVC_INVALID: u32 = 0x01;
const VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE: u32 = 0x10;

pub struct VaapiHevcEncoder {
    va: &'static gpu_libs::VaFns,
    display: VADisplay,
    config: VAConfigID,
    context: VAContextID,
    surfaces: [VASurfaceID; TOTAL_SURFACES],
    coded_buf: VABufferID,
    width: u32,
    height: u32,
    ctu_size: u32,
    width_in_ctus: u32,
    height_in_ctus: u32,
    frame_num: u32,
    idr_num: u32,
    force_idr: bool,
    cur_ref_idx: usize,
    log2_min_cb_minus3: u8,
    log2_diff_max_min_cb: u8,
    _drm_fd: OwnedFd,
}

unsafe impl Send for VaapiHevcEncoder {}

impl VaapiHevcEncoder {
    pub fn try_new(width: u32, height: u32, vaapi_device: &str) -> Result<Self, String> {
        let va = gpu_libs::va().ok_or("libva.so not found")?;
        let va_drm = gpu_libs::va_drm().ok_or("libva-drm.so not found")?;

        let drm_fd = {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(vaapi_device)
                .map_err(|e| format!("failed to open {vaapi_device}: {e}"))?;
            OwnedFd::from(file)
        };

        let display = unsafe { (va_drm.vaGetDisplayDRM)(drm_fd.as_raw_fd()) };
        if display.is_null() {
            return Err("vaGetDisplayDRM returned null".into());
        }

        let mut major = 0i32;
        let mut minor = 0i32;
        let st = unsafe { (va.vaInitialize)(display, &mut major, &mut minor) };
        if st != VA_STATUS_SUCCESS {
            return Err(format!("vaInitialize failed: {st}"));
        }

        // Probe for EncSliceLP or EncSlice on HEVCMain
        let mut entrypoints = [0i32; 16];
        let mut num_ep = 0i32;
        unsafe {
            (va.vaQueryConfigEntrypoints)(
                display,
                VAProfileHEVCMain,
                entrypoints.as_mut_ptr(),
                &mut num_ep,
            );
        }
        let ep_slice = &entrypoints[..num_ep as usize];
        let entrypoint = if ep_slice.contains(&VAEntrypointEncSliceLP) {
            VAEntrypointEncSliceLP
        } else if ep_slice.contains(&VAEntrypointEncSlice) {
            VAEntrypointEncSlice
        } else {
            unsafe {
                (va.vaTerminate)(display);
            }
            return Err("HEVC encode not supported on this VA-API device".into());
        };

        // Create config
        let mut config: VAConfigID = 0;
        let st = unsafe {
            (va.vaCreateConfig)(
                display,
                VAProfileHEVCMain,
                entrypoint,
                ptr::null_mut(),
                0,
                &mut config,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateConfig(HEVC) failed: {st}"));
        }

        // Create surfaces
        let mut surfaces = [0u32; TOTAL_SURFACES];
        let st = unsafe {
            (va.vaCreateSurfaces)(
                display,
                VA_RT_FORMAT_YUV420,
                width,
                height,
                surfaces.as_mut_ptr(),
                TOTAL_SURFACES as u32,
                ptr::null_mut(),
                0,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroyConfig)(display, config);
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateSurfaces(HEVC) failed: {st}"));
        }

        // Create context
        let mut context: VAContextID = 0;
        let st = unsafe {
            (va.vaCreateContext)(
                display,
                config,
                width as i32,
                height as i32,
                0x00000002, // VA_PROGRESSIVE
                surfaces.as_mut_ptr(),
                TOTAL_SURFACES as i32,
                &mut context,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroySurfaces)(display, surfaces.as_mut_ptr(), TOTAL_SURFACES as i32);
                (va.vaDestroyConfig)(display, config);
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateContext(HEVC) failed: {st}"));
        }

        // Coded buffer
        let coded_buf_size = width * height;
        let mut coded_buf: VABufferID = 0;
        let st = unsafe {
            (va.vaCreateBuffer)(
                display,
                context,
                VAEncCodedBufferType,
                coded_buf_size,
                1,
                ptr::null_mut(),
                &mut coded_buf,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (va.vaDestroyContext)(display, context);
                (va.vaDestroySurfaces)(display, surfaces.as_mut_ptr(), TOTAL_SURFACES as i32);
                (va.vaDestroyConfig)(display, config);
                (va.vaTerminate)(display);
            }
            return Err(format!("vaCreateBuffer(coded,HEVC) failed: {st}"));
        }

        // HEVC uses CTUs.  Most HW supports 32 or 64.  We use 32 (log2=5)
        // which is the most widely supported (Intel, AMD).
        //   log2_min_luma_coding_block_size_minus3 = 0 → min CB = 8
        //   log2_diff_max_min_luma_coding_block_size = 2 → max CB = 32 = CTU
        let ctu_size = 32u32;
        let log2_min_cb_minus3: u8 = 0; // min CB log2 = 3 → 8
        let log2_diff_max_min_cb: u8 = 2; // max CB log2 = 5 → 32

        let width_in_ctus = width.div_ceil(ctu_size);
        let height_in_ctus = height.div_ceil(ctu_size);

        eprintln!(
            "[vaapi-direct] initialized HEVC Main encoder for {width}x{height} (ep={entrypoint}, ctu={ctu_size})"
        );

        Ok(Self {
            va,
            display,
            config,
            context,
            surfaces,
            coded_buf,
            width,
            height,
            ctu_size,
            width_in_ctus,
            height_in_ctus,
            frame_num: 0,
            idr_num: 0,
            force_idr: false,
            cur_ref_idx: 0,
            log2_min_cb_minus3,
            log2_diff_max_min_cb,
            _drm_fd: drm_fd,
        })
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    pub fn encode_nv12(
        &mut self,
        y_data: &[u8],
        uv_data: &[u8],
        y_stride: usize,
        uv_stride: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let input_surface = self.surfaces[NUM_REF_SURFACES];
        self.upload_nv12(input_surface, y_data, uv_data, y_stride, uv_stride)?;
        self.encode_surface(input_surface)
    }

    pub fn encode_bgra_padded(
        &mut self,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<(Vec<u8>, bool)> {
        let input_surface = self.surfaces[NUM_REF_SURFACES];
        self.upload_bgra(input_surface, bgra, src_w, src_h)?;
        self.encode_surface(input_surface)
    }

    // --- Surface upload (reuse identical logic from H.264 encoder) ---

    fn upload_nv12(
        &self,
        surface: VASurfaceID,
        y_data: &[u8],
        uv_data: &[u8],
        src_y_stride: usize,
        src_uv_stride: usize,
    ) -> Option<()> {
        let mut image = [0u8; VA_IMAGE_SIZE];
        let st = unsafe {
            (self.va.vaDeriveImage)(self.display, surface, image.as_mut_ptr() as *mut c_void)
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }

        let image_id = r32(&image, VAIMG_ID_OFF);
        let buf_id = r32(&image, VAIMG_BUF_OFF);
        let y_pitch = r32(&image, VAIMG_PITCHES_OFF) as usize;
        let uv_pitch = r32(&image, VAIMG_PITCHES_OFF + 4) as usize;
        let y_offset = r32(&image, VAIMG_OFFSETS_OFF) as usize;
        let uv_offset = r32(&image, VAIMG_OFFSETS_OFF + 4) as usize;

        let mut map_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, buf_id, &mut map_ptr) };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (self.va.vaDestroyImage)(self.display, image_id);
            }
            return None;
        }

        let w = self.width as usize;
        let h = self.height as usize;
        let dst = map_ptr as *mut u8;
        unsafe {
            for row in 0..h {
                let sr = row.min(h - 1);
                let src_start = sr * src_y_stride;
                let dst_start = y_offset + row * y_pitch;
                let copy_len = w.min(y_data.len() - src_start);
                ptr::copy_nonoverlapping(
                    y_data.as_ptr().add(src_start),
                    dst.add(dst_start),
                    copy_len,
                );
            }
            let uv_h = h / 2;
            for row in 0..uv_h {
                let src_start = row * src_uv_stride;
                let dst_start = uv_offset + row * uv_pitch;
                let copy_len = w.min(uv_data.len() - src_start);
                ptr::copy_nonoverlapping(
                    uv_data.as_ptr().add(src_start),
                    dst.add(dst_start),
                    copy_len,
                );
            }
        }

        unsafe {
            (self.va.vaUnmapBuffer)(self.display, buf_id);
            (self.va.vaDestroyImage)(self.display, image_id);
        }
        Some(())
    }

    fn upload_bgra(
        &self,
        surface: VASurfaceID,
        bgra: &[u8],
        src_w: usize,
        src_h: usize,
    ) -> Option<()> {
        let mut image = [0u8; VA_IMAGE_SIZE];
        let st = unsafe {
            (self.va.vaDeriveImage)(self.display, surface, image.as_mut_ptr() as *mut c_void)
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }

        let image_id = r32(&image, VAIMG_ID_OFF);
        let buf_id = r32(&image, VAIMG_BUF_OFF);
        let y_pitch = r32(&image, VAIMG_PITCHES_OFF) as usize;
        let uv_pitch = r32(&image, VAIMG_PITCHES_OFF + 4) as usize;
        let y_offset = r32(&image, VAIMG_OFFSETS_OFF) as usize;
        let uv_offset = r32(&image, VAIMG_OFFSETS_OFF + 4) as usize;

        let mut map_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, buf_id, &mut map_ptr) };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (self.va.vaDestroyImage)(self.display, image_id);
            }
            return None;
        }

        let enc_w = self.width as usize;
        let enc_h = self.height as usize;
        let dst = map_ptr as *mut u8;

        // BGRA→NV12 directly into mapped surface memory
        unsafe {
            for row in 0..enc_h {
                let sr = row.min(src_h - 1);
                let dst_row = dst.add(y_offset + row * y_pitch);
                for col in 0..enc_w {
                    let sc = col.min(src_w - 1);
                    let i = (sr * src_w + sc) * 4;
                    let r = bgra[i + 2] as i32;
                    let g = bgra[i + 1] as i32;
                    let b = bgra[i] as i32;
                    let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                    *dst_row.add(col) = y.clamp(0, 255) as u8;
                }
            }
            let chroma_h = enc_h / 2;
            let chroma_w = enc_w / 2;
            for cy in 0..chroma_h {
                let dst_row = dst.add(uv_offset + cy * uv_pitch);
                for cx in 0..chroma_w {
                    let row = cy * 2;
                    let col = cx * 2;
                    let mut u_sum = 0i32;
                    let mut v_sum = 0i32;
                    for dy in 0..2usize {
                        for dx in 0..2usize {
                            let sr = (row + dy).min(src_h - 1);
                            let sc = (col + dx).min(src_w - 1);
                            let i = (sr * src_w + sc) * 4;
                            let r = bgra[i + 2] as i32;
                            let g = bgra[i + 1] as i32;
                            let b = bgra[i] as i32;
                            u_sum += ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                            v_sum += ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                        }
                    }
                    *dst_row.add(cx * 2) = (u_sum / 4).clamp(0, 255) as u8;
                    *dst_row.add(cx * 2 + 1) = (v_sum / 4).clamp(0, 255) as u8;
                }
            }
        }

        unsafe {
            (self.va.vaUnmapBuffer)(self.display, buf_id);
            (self.va.vaDestroyImage)(self.display, image_id);
        }
        Some(())
    }

    // --- Encode pipeline ---

    fn encode_surface(&mut self, input_surface: VASurfaceID) -> Option<(Vec<u8>, bool)> {
        let is_idr = self.force_idr || self.frame_num == 0;
        if is_idr {
            self.frame_num = 0;
            self.idr_num += 1;
            self.force_idr = false;
        }

        let ref_surface = self.surfaces[self.cur_ref_idx];
        let recon_idx = (self.cur_ref_idx + 1) % NUM_REF_SURFACES;
        let recon_surface = self.surfaces[recon_idx];

        let sps_buf = self.create_hevc_sps()?;
        let pps_buf = self.create_hevc_pps(is_idr, ref_surface, recon_surface)?;
        let slice_buf = self.create_hevc_slice(is_idr, ref_surface)?;

        let mut buffers = [sps_buf, pps_buf, slice_buf];

        let st = unsafe { (self.va.vaBeginPicture)(self.display, self.context, input_surface) };
        if st != VA_STATUS_SUCCESS {
            self.destroy_buffers(&buffers);
            return None;
        }

        let st = unsafe {
            (self.va.vaRenderPicture)(
                self.display,
                self.context,
                buffers.as_mut_ptr(),
                buffers.len() as i32,
            )
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (self.va.vaEndPicture)(self.display, self.context);
            }
            self.destroy_buffers(&buffers);
            return None;
        }

        let st = unsafe { (self.va.vaEndPicture)(self.display, self.context) };
        if st != VA_STATUS_SUCCESS {
            self.destroy_buffers(&buffers);
            return None;
        }

        let st = unsafe { (self.va.vaSyncSurface)(self.display, input_surface) };
        if st != VA_STATUS_SUCCESS {
            self.destroy_buffers(&buffers);
            return None;
        }

        let nal_data = self.read_coded_buffer()?;
        self.destroy_buffers(&buffers);

        self.frame_num += 1;
        self.cur_ref_idx = recon_idx;

        if nal_data.is_empty() {
            None
        } else {
            Some((nal_data, is_idr))
        }
    }

    // --- VAPictureHEVC helpers ---

    fn write_hevc_pic(buf: &mut [u8], off: usize, surface: VASurfaceID, poc: i32, flags: u32) {
        w32(buf, off + HEVC_PIC_ID, surface);
        buf[off + HEVC_PIC_POC..off + HEVC_PIC_POC + 4].copy_from_slice(&poc.to_ne_bytes());
        w32(buf, off + HEVC_PIC_FLAGS, flags);
        // va_reserved[4] stays zeroed
    }

    fn write_hevc_pic_invalid(buf: &mut [u8], off: usize) {
        Self::write_hevc_pic(buf, off, VA_INVALID_SURFACE, 0, VA_PICTURE_HEVC_INVALID);
    }

    // --- Parameter buffers ---

    fn create_hevc_sps(&self) -> Option<VABufferID> {
        let mut sps = [0u8; HEVC_SPS_SIZE];

        // general_profile_idc = 1 (Main)                          @ 0
        w8(&mut sps, 0, 1);
        // general_level_idc = 120 (level 4.0, supports 2048×1080) @ 1
        w8(&mut sps, 1, 120);
        // general_tier_flag = 0 (Main tier)                        @ 2
        // intra_period                                             @ 4
        w32(&mut sps, 4, 120);
        // intra_idr_period                                         @ 8
        w32(&mut sps, 8, 120);
        // ip_period (1 = no B-frames, IP only)                     @ 12
        w32(&mut sps, 12, 1);
        // bits_per_second = 0 (VBR / CQP)                         @ 16
        // pic_width_in_luma_samples                                @ 20
        w16(&mut sps, 20, self.width as u16);
        // pic_height_in_luma_samples                               @ 22
        w16(&mut sps, 22, self.height as u16);

        // seq_fields bitfield                                      @ 24
        //   chroma_format_idc      : bits 0-1  = 1 (4:2:0)
        //   amp_enabled_flag       : bit 11    = 1
        //   sps_temporal_mvp_enabled_flag : bit 15 = 1
        //   low_delay_seq          : bit 16    = 1 (IP only)
        let seq_fields: u32 = 1 // chroma_format_idc = 1
            | (1 << 11)         // amp_enabled_flag
            | (1 << 15)         // sps_temporal_mvp_enabled_flag
            | (1 << 16); // low_delay_seq
        w32(&mut sps, 24, seq_fields);

        // log2_min_luma_coding_block_size_minus3                   @ 28
        w8(&mut sps, 28, self.log2_min_cb_minus3);
        // log2_diff_max_min_luma_coding_block_size                 @ 29
        w8(&mut sps, 29, self.log2_diff_max_min_cb);
        // log2_min_transform_block_size_minus2 = 0 → min TB = 4   @ 30
        w8(&mut sps, 30, 0);
        // log2_diff_max_min_transform_block_size = 3 → max TB = 32 @ 31
        w8(&mut sps, 31, 3);
        // max_transform_hierarchy_depth_inter = 2                  @ 32
        w8(&mut sps, 32, 2);
        // max_transform_hierarchy_depth_intra = 2                  @ 33
        w8(&mut sps, 33, 2);

        let mut buf_id: VABufferID = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncSequenceParameterBufferType,
                HEVC_SPS_SIZE as u32,
                1,
                sps.as_mut_ptr() as *mut c_void,
                &mut buf_id,
            )
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        Some(buf_id)
    }

    fn create_hevc_pps(
        &self,
        is_idr: bool,
        ref_surface: VASurfaceID,
        recon_surface: VASurfaceID,
    ) -> Option<VABufferID> {
        let mut pps = [0u8; HEVC_PPS_SIZE];
        let poc = self.frame_num as i32 * 2;

        // decoded_curr_pic (VAPictureHEVC)                         @ 0
        Self::write_hevc_pic(&mut pps, 0, recon_surface, poc, 0);

        // reference_frames[15] (VAPictureHEVC × 15)                @ 28
        for i in 0..15u32 {
            let off = 28 + (i as usize) * HEVC_PIC_SIZE;
            Self::write_hevc_pic_invalid(&mut pps, off);
        }
        // Set reference_frames[0] for P-frames
        if !is_idr && self.frame_num > 0 {
            let ref_poc = (self.frame_num as i32 - 1) * 2;
            Self::write_hevc_pic(
                &mut pps,
                28,
                ref_surface,
                ref_poc,
                VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
            );
        }

        // coded_buf                                                @ 448
        w32(&mut pps, 448, self.coded_buf);
        // collocated_ref_pic_index                                 @ 452
        w8(&mut pps, 452, if is_idr { 0xFF } else { 0 });
        // pic_init_qp                                              @ 454
        w8(&mut pps, 454, 26);
        // num_ref_idx_l0_default_active_minus1                     @ 502
        w8(&mut pps, 502, 0);
        // nal_unit_type: IDR=19 (IDR_W_RADL), P=1 (TRAIL_R)      @ 505
        w8(&mut pps, 505, if is_idr { 19 } else { 1 });

        // pic_fields bitfield                                      @ 508
        //   idr_pic_flag          : bit 0
        //   coding_type           : bits 1-3 (1=I, 2=P)
        //   reference_pic_flag    : bit 4    = 1
        //   cu_qp_delta_enabled_flag : bit 10 = 1
        //   pps_loop_filter_across_slices_enabled_flag : bit 15 = 1
        let coding_type: u32 = if is_idr { 1 } else { 2 };
        let mut pic_fields: u32 = 0;
        if is_idr {
            pic_fields |= 1; // idr_pic_flag
        }
        pic_fields |= coding_type << 1; // coding_type
        pic_fields |= 1 << 4; // reference_pic_flag
        pic_fields |= 1 << 10; // cu_qp_delta_enabled_flag
        pic_fields |= 1 << 15; // pps_loop_filter_across_slices_enabled_flag
        w32(&mut pps, 508, pic_fields);

        let mut buf_id: VABufferID = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncPictureParameterBufferType,
                HEVC_PPS_SIZE as u32,
                1,
                pps.as_mut_ptr() as *mut c_void,
                &mut buf_id,
            )
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        Some(buf_id)
    }

    fn create_hevc_slice(&self, is_idr: bool, ref_surface: VASurfaceID) -> Option<VABufferID> {
        let mut slice = [0u8; HEVC_SLICE_SIZE];

        let num_ctus = self.width_in_ctus * self.height_in_ctus;

        // slice_segment_address = 0                                @ 0
        // num_ctu_in_slice                                         @ 4
        w32(&mut slice, 4, num_ctus);
        // slice_type: 2 = I, 0 = B, 1 = P                         @ 8
        w8(&mut slice, 8, if is_idr { 2 } else { 1 });
        // num_ref_idx_l0_active_minus1                             @ 10
        w8(&mut slice, 10, 0);

        // ref_pic_list0[15] (VAPictureHEVC × 15)                  @ 12
        for i in 0..15u32 {
            let off = 12 + (i as usize) * HEVC_PIC_SIZE;
            Self::write_hevc_pic_invalid(&mut slice, off);
        }
        // ref_pic_list1[15]                                        @ 432
        for i in 0..15u32 {
            let off = 432 + (i as usize) * HEVC_PIC_SIZE;
            Self::write_hevc_pic_invalid(&mut slice, off);
        }

        // Set ref_pic_list0[0] for P-frames
        if !is_idr && self.frame_num > 0 {
            let ref_poc = (self.frame_num as i32 - 1) * 2;
            Self::write_hevc_pic(
                &mut slice,
                12,
                ref_surface,
                ref_poc,
                VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
            );
        }

        // max_num_merge_cand = 5                                   @ 1034
        w8(&mut slice, 1034, 5);
        // slice_qp_delta (i8): QP=23, pic_init_qp=26 → delta=-3   @ 1035
        slice[1035] = (-3i8) as u8;

        // slice_fields bitfield                                    @ 1040
        //   last_slice_of_pic_flag              : bit 0  = 1
        //   slice_temporal_mvp_enabled_flag     : bit 4  = 1
        //   num_ref_idx_active_override_flag    : bit 7  = 1 (for P)
        //   slice_loop_filter_across_slices_enabled_flag : bit 12 = 1
        //   collocated_from_l0_flag             : bit 13 = 1
        let mut slice_fields: u32 = 0;
        slice_fields |= 1; // last_slice_of_pic_flag
        slice_fields |= 1 << 4; // slice_temporal_mvp_enabled_flag
        if !is_idr {
            slice_fields |= 1 << 7; // num_ref_idx_active_override_flag
        }
        slice_fields |= 1 << 12; // slice_loop_filter_across_slices_enabled_flag
        slice_fields |= 1 << 13; // collocated_from_l0_flag
        w32(&mut slice, 1040, slice_fields);

        let mut buf_id: VABufferID = 0;
        let st = unsafe {
            (self.va.vaCreateBuffer)(
                self.display,
                self.context,
                VAEncSliceParameterBufferType,
                HEVC_SLICE_SIZE as u32,
                1,
                slice.as_mut_ptr() as *mut c_void,
                &mut buf_id,
            )
        };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        Some(buf_id)
    }

    fn read_coded_buffer(&self) -> Option<Vec<u8>> {
        let mut buf_ptr: *mut c_void = ptr::null_mut();
        let st = unsafe { (self.va.vaMapBuffer)(self.display, self.coded_buf, &mut buf_ptr) };
        if st != VA_STATUS_SUCCESS {
            return None;
        }

        let mut nal_data = Vec::new();
        let mut seg_ptr = buf_ptr as *const u8;
        loop {
            if seg_ptr.is_null() {
                break;
            }
            let size = unsafe { u32::from_ne_bytes(*(seg_ptr as *const [u8; 4])) } as usize;
            let data_ptr = unsafe {
                let p = seg_ptr.add(CBS_BUF_OFF);
                *(p as *const *const u8)
            };
            if !data_ptr.is_null() && size > 0 {
                let data = unsafe { std::slice::from_raw_parts(data_ptr, size) };
                nal_data.extend_from_slice(data);
            }
            let next = unsafe {
                let p = seg_ptr.add(CBS_NEXT_OFF);
                *(p as *const *const u8)
            };
            seg_ptr = next;
        }

        unsafe {
            (self.va.vaUnmapBuffer)(self.display, self.coded_buf);
        }
        Some(nal_data)
    }

    fn destroy_buffers(&self, buffers: &[VABufferID]) {
        for &buf in buffers {
            unsafe {
                (self.va.vaDestroyBuffer)(self.display, buf);
            }
        }
    }
}

impl Drop for VaapiHevcEncoder {
    fn drop(&mut self) {
        unsafe {
            (self.va.vaDestroyBuffer)(self.display, self.coded_buf);
            (self.va.vaDestroyContext)(self.display, self.context);
            (self.va.vaDestroySurfaces)(
                self.display,
                self.surfaces.as_mut_ptr(),
                TOTAL_SURFACES as i32,
            );
            (self.va.vaDestroyConfig)(self.display, self.config);
            (self.va.vaTerminate)(self.display);
        }
    }
}
