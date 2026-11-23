// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The output of a frame: an output image from the application, the frame put together in the layout of
//! that image (with film grain), and the copy into the image's buffer or textures. From
//! `av1_postprocess_copy_output` of `libav1/dx/loopfilters.cpp`.
//!
//! Only for a frame that is output now: a frame with show_frame, at the end of its own command list, or
//! a frame shown with show_existing_frame, in the command list of that show_existing_frame
//! (`av1_decode_sef`; `cm->show_frame` is 1 there and `cm->film_grain_params` are the frame's).
//!
//! Of the shown frames of a temporal unit with spatial layers only the frame of the highest layer is
//! output (`av1_frame_is_output()` of obu.c; `td->output_frame`); the others are decoded like frames that
//! are not shown.
//!
//! A frame that is decoded but not shown takes no output image. It used to take one when it was showable
//! and kept it until a show_existing_frame, or until the frame left the reference map; the output images
//! of the application were then held by frames that are not output, and a stream with as many hidden
//! frames as the application has images could not decode any further (the decode thread waits here for an
//! image that only a later show_existing_frame would hand over).
//!
//! **The semi-planar output** (`aom_codec_dec_cfg_t::output_kind`, docs/output.md): the image is one NV12
//! texture (8-bit frames) or one P010 texture (10-bit frames), a luma plane and a chroma plane with U and
//! V side by side, in the size of the frame with both rounded up to even. The same pass writes it, by one
//! of two paths:
//!
//! - **direct**: the shaders of the pass (film grain, or the copy of the planes that get none) write
//!   the two planes of the texture. Nothing is put together in the back buffer and nothing is copied.
//!   Needs a texture that shaders may write, on a device that lets them.
//! - **staged**: the shaders put the frame together in the back buffer as the texture is laid out, and
//!   two copies move the planes into the texture; a buffer of the application gets the same bytes. For
//!   a texture shaders may not write, and for an image with a buffer.
//!
//! The texture comes in the common state and is left in it.
//!
//! The C++ function is two calls here, with the glue between them: when the application gives no image
//! the glue raises the decoder's error (`aom_internal_error`), which leaves the decode call with a
//! `longjmp`, and that must not pass a Rust function.

use std::ffi::c_int;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use super::core::{
    buffer, commands, perf_add, perf_gpu_mark, perf_ticks, pipeline, stage_dump_active, stage_dump_output,
};
use super::film_grain::{GrainTarget, SKIP_FILM_GRAIN, filmgrain_run};
use super::log::log;
use super::postfilter::{FusedTarget, cdef_v3_enabled};
use super::types::Av1Core;

// av1_set_output_copy_legacy(1) disables fused postfilter->output texture writes (-outputcopylegacy).
static OUTPUT_COPY_LEGACY: AtomicI32 = AtomicI32::new(0);
/// `av1_set_output_copy_legacy` (`dx/av1_perf.h`).
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_output_copy_legacy(enable: c_int) {
    OUTPUT_COPY_LEGACY.store(enable, Ordering::Relaxed);
}

pub(crate) fn output_copy_legacy() -> bool {
    OUTPUT_COPY_LEGACY.load(Ordering::Relaxed) != 0
}

// The display pass, a prototype (docs/stage-fusion.md, option B; -displaypass WxH): the display size, the
// width in the high 16 bits and the height in the low ones, 0 for none.
static DISPLAY_PASS: AtomicU32 = AtomicU32::new(0);
static DISPLAY_PASS_NOTE: AtomicBool = AtomicBool::new(false);

/// `av1_set_display_pass` (`dx/av1_perf.h`): with a size, every 10-bit frame a decoder with the
/// semi-planar output kind outputs goes through `display_pass.hlsl` into a P010 image of that size
/// (rounded up to even), downscaled and tone-mapped to BT.709 SDR, instead of the frame itself. 0, 0: off.
#[unsafe(no_mangle)]
pub extern "C" fn av1_set_display_pass(width: c_int, height: c_int) {
    let even = |v: c_int| (((v + 1) & !1) as u32).min(0xfffe);
    let size = if width > 0 && height > 0 { (even(width) << 16) | even(height) } else { 0 };
    DISPLAY_PASS.store(size, Ordering::Relaxed);
}

/// The display size when the frame of the current command list takes the display pass: the switch is
/// set, the decoder has the semi-planar output kind, the frame has more than 8 bits (the prototype writes
/// P010 only) and chroma.
///
/// # Safety
/// `dec` is the decoder while it records a frame or a show_existing_frame; `cm` is the frame's.
pub(crate) unsafe fn display_pass_size(dec: *const Av1Core, cm: *const AV1_COMMON) -> Option<(c_int, c_int)> {
    let size = DISPLAY_PASS.load(Ordering::Relaxed);
    // SAFETY: the caller's contract.
    unsafe {
        let td = (*dec).curr_frame_data.as_ref()?;
        if size == 0 || (*dec).output_kind == 0 || td.is_hbd == 0 || (*(*cm).cur_frame).buf.monochrome != 0 {
            return None;
        }
    }
    Some(((size >> 16) as c_int, (size & 0xffff) as c_int))
}

/// The inline constants of `display_pass.hlsl` for the frame `src` and the sequence's colour description:
/// sizes, planes, the source's transfer (PQ, HLG, else SDR), BT.2020 matrix, range, chroma siting, and the
/// tone map's source peak (1000 nits) and SDR white (100 nits), as the stage it stands in for has them.
fn display_pass_constants(src: &HwFrameBuffer, out: (c_int, c_int), seq: &crate::sys::SequenceHeader) -> [u32; 12] {
    let mode = match seq.transfer_characteristics as u32 {
        16 => 0,
        18 => 2,
        _ => 1,
    };
    let flags = mode
        | (u32::from(seq.matrix_coefficients as u32 == 9) << 2)
        | (u32::from(seq.color_range != 0) << 3)
        // AV1 chroma_sample_position: 2 (colocated) sits on the luma row, the others between two rows.
        | (u32::from(seq.chroma_sample_position as u32 != 2) << 4);
    [
        src.y_crop_width as u32,
        src.y_crop_height as u32,
        out.0 as u32,
        out.1 as u32,
        src.planes[0].offset as u32,
        src.planes[1].offset as u32,
        src.planes[2].offset as u32,
        src.planes[0].stride as u32,
        src.planes[1].stride as u32,
        flags,
        1000.0f32.to_bits(),
        100.0f32.to_bits(),
    ]
}
use crate::gpu::d3d12::{com_ref, wrapped_ref};
use crate::gpu::{
    BufferRef, BufferState, Commands, Device, PlaneViews, SemiPlanar, TextureFormat, TextureRef, TextureSource,
    TextureState,
};
use crate::sys::{
    AV1_COMMON, AV1_PERF_CPU_WAIT_IMAGE, AV1_PERF_CPU_WAIT_OUTPUT, AV1_PERF_GPU_COPY_PLANE, AV1_PERF_GPU_OUTPUT_COPY,
    AV1_PERF_OUTPUT_COPY_OTHER, AV1_PERF_OUTPUT_COPY_SWITCH, AV1_PERF_OUTPUT_COPY_UNFILTERED, AV1_PERF_OUTPUT_FUSED,
    AV1_PERF_OUTPUT_GRAIN, HwFrameBuffer, HwOutputImage, av1_decoded_frame_buffer_t, fbt8bit, fbt10bit, fbt10x3,
    fbtNV12, fbtP010,
};
use windows::Win32::Graphics::Direct3D12::ID3D12Device;

/// The planes of an output image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct OutputLayout {
    /// Bytes of a sample, or of three samples packed into 32 bits.
    bpp: c_int,
    num_planes: c_int,
    y_stride: c_int,
    uv_stride: c_int,
    y_size: c_int,
    uv_size: c_int,
    /// Bytes of the image.
    size: c_int,
    /// Width of the planes as textures: samples, or packed words of three.
    y_texture_width: c_int,
    uv_texture_width: c_int,
}

/// The layout of the output image of a frame: rows of a multiple of 256 bytes, the luma row twice the
/// chroma row, planes of a multiple of 512 bytes.
fn output_layout(
    width: c_int,
    height: c_int,
    hbd: bool,
    is_monochrome: bool,
    is_10x3: bool,
    semi_planar: bool,
) -> OutputLayout {
    const ALIGN1: c_int = 255;
    if semi_planar {
        return semi_planar_layout(width, height, hbd);
    }
    let bpp = if hbd { if is_10x3 { 4 } else { 2 } } else { 1 };
    let uv_width = (width + 1) >> 1;
    let uv_height = (height + 1) >> 1;
    let y_texture_width = if is_10x3 { (width + 2) / 3 } else { width };
    let uv_texture_width = if is_10x3 { (uv_width + 2) / 3 } else { uv_width };
    let (y_stride, uv_stride) = if is_monochrome {
        ((y_texture_width * bpp + ALIGN1) & !ALIGN1, 0)
    } else {
        let uv_stride = (uv_texture_width * bpp + ALIGN1) & !ALIGN1;
        (uv_stride * 2, uv_stride)
    };
    let num_planes = if is_monochrome { 1 } else { 3 };
    let y_size = (y_stride * height + 511) & !511;
    let uv_size = (uv_stride * uv_height + 511) & !511;
    OutputLayout {
        bpp,
        num_planes,
        y_stride,
        uv_stride,
        y_size,
        uv_size,
        size: y_size + (num_planes - 1) * uv_size,
        y_texture_width,
        uv_texture_width,
    }
}

/// The layout of a semi-planar image: the luma plane, then the plane of U and V side by side, as an NV12
/// or a P010 texture of the frame size, both rounded up to even, has them. Rows of a multiple of 256
/// bytes and planes of a multiple of 512, which is what a copy into a texture asks. A monochrome frame
/// has the same two planes.
fn semi_planar_layout(width: c_int, height: c_int, hbd: bool) -> OutputLayout {
    let bpp = if hbd { 2 } else { 1 };
    let width = (width + 1) & !1;
    let height = (height + 1) & !1;
    let stride = (width * bpp + 255) & !255;
    let y_size = (stride * height + 511) & !511;
    let uv_size = (stride * (height / 2) + 511) & !511;
    OutputLayout {
        bpp,
        num_planes: 2,
        y_stride: stride,
        uv_stride: stride,
        y_size,
        uv_size,
        size: y_size + uv_size,
        y_texture_width: width,
        uv_texture_width: width / 2,
    }
}

/// Output staging capacity for the decoder's maximum dimensions and bit depth. Full chroma and
/// unpacked samples cover monochrome and packed 10-bit output as well.
pub(super) fn staging_size(width: c_int, height: c_int, hbd: bool, semi_planar: bool) -> c_int {
    output_layout(width, height, hbd, false, false, semi_planar).size
}

/// What both calls need of the frame.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's.
unsafe fn frame_layout(dec: *mut Av1Core, cm: *const AV1_COMMON) -> (bool, bool, c_int, c_int, OutputLayout) {
    // SAFETY: the caller's contract.
    unsafe {
        if let Some((width, height)) = display_pass_size(dec, cm) {
            return (false, false, width, height, semi_planar_layout(width, height, true));
        }
        let td = &*(*dec).curr_frame_data;
        let src = &*td.dst_frame_buffer;
        let width = if td.do_superres != 0 { (*cm).superres_upscaled_width } else { src.y_crop_width };
        let height = src.y_crop_height;
        let is_monochrome = (*(*cm).cur_frame).buf.monochrome != 0;
        let semi_planar = (*dec).output_kind != 0;
        let is_10x3 = !semi_planar && (*dec).tryhdr10x3 != 0 && src.hbd != 0;
        let layout = output_layout(width, height, src.hbd != 0, is_monochrome, is_10x3, semi_planar);
        (is_monochrome, is_10x3, width, height, layout)
    }
}

/// The first half of `av1_postprocess_copy_output`: asks the application for the output image of the
/// frame of the current command list. Returns 0 when the frame is not output (nothing else to do), 1
/// with the image in `fb`, 2 when the application gave none.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's; `fb` is valid for writing and
/// zeroed.
pub(crate) unsafe fn output_image(
    dec: *mut Av1Core,
    cm: *mut AV1_COMMON,
    fb: *mut av1_decoded_frame_buffer_t,
) -> c_int {
    // SAFETY: the caller's contract; the callback is the application's, called as the C++ called it.
    unsafe {
        let td = &*(*dec).curr_frame_data;
        (*(*cm).cur_frame).buf.hw_show_image = std::ptr::null_mut();
        if td.output_frame == 0 {
            return 0;
        }
        let (_, is_10x3, crop_width, crop_height, layout) = frame_layout(dec, cm);

        let mut err = 1;
        let semi_planar = (*dec).output_kind != 0;
        let fb_type = match (semi_planar, td.is_hbd != 0) {
            (true, true) => fbtP010,
            (true, false) => fbtNV12,
            (false, true) if is_10x3 => fbt10x3,
            (false, true) => fbt10bit,
            (false, false) => fbt8bit,
        };
        let perf_t = perf_ticks();
        if let Some(get_output_image) = (*dec).cb_get_output_image {
            err = get_output_image(
                (*dec).image_alloc_priv,
                layout.size as usize,
                crop_width as u16,
                crop_height as u16,
                fb_type,
                fb,
            );
        }
        perf_add(dec, AV1_PERF_CPU_WAIT_OUTPUT, perf_t);
        if err != 0 || ((*fb).dx12_buffer.is_null() && (*fb).dx12_texture[0].is_null()) {
            return 2;
        }
        if semi_planar && let Some(texture) = TextureRef::from_raw(&(*fb).dx12_texture[0]) {
            // The texture has to be what the request named: the copy and the views are made for it.
            let format = if td.is_hbd != 0 { SemiPlanar::P010 } else { SemiPlanar::Nv12 };
            let info = texture.info();
            let (width, height) = (layout.y_texture_width as u32, ((crop_height + 1) & !1) as u32);
            if info.semi_planar != Some(format) || info.width != width || info.height != height {
                log(
                    true,
                    &format!(
                        "OUTPUT the image of a {}x{} frame has to be a {} texture of {width}x{height}; the \
                         application gave {} of {}x{}",
                        crop_width,
                        crop_height,
                        format.name(),
                        info.semi_planar.map_or("another format", SemiPlanar::name),
                        info.width,
                        info.height
                    ),
                );
                // The image goes back: the decoder took it and writes no frame into it.
                if let Some(release) = (*dec).cb_release_image {
                    release((*dec).image_alloc_priv, (*fb).priv_);
                }
                return 2;
            }
        }
        1
    }
}

/// Whether nothing after this frame's own command list reads its frame buffer: a frame with
/// refresh_frame_flags == 0 goes into no slot of the reference map (AV1 7.20), and show_existing_frame
/// shows only frames of the map (7.21), so no later frame predicts from it or shows it. showable_frame
/// does not decide it: a shown inter frame has showable_frame 1 (5.9.2) whether a slot takes it or not,
/// and the frames of the top layer of a hierarchical prediction structure are usually taken by none.
/// Never with -dumpstages, whose dumps read the frame buffer.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's.
pub(crate) unsafe fn frame_unreferenced(dec: *mut Av1Core, cm: *const AV1_COMMON) -> bool {
    // SAFETY: the caller's contract.
    unsafe { (*cm).current_frame.refresh_frame_flags == 0 && !stage_dump_active(dec) }
}

/// Checks whether the direct semi-planar (NV12/P010) output write can be fused into the frame's last
/// postfilter stage (CDEF v3, superres, or loop restoration). When it can, transitions the texture
/// `Common -> ReadWrite`, writes its plane UAV descriptors, and records the fused state on `td` so
/// [`copy_output`] skips the redundant full-frame copy pass. When it cannot, records why on `td` for
/// the timing run (`AV1_PERF_OUTPUT_COPY_*`).
///
/// # Safety
/// `dec` is the decoder while it records a frame whose [`output_image`] returned 1 with `fb`; `cm` is
/// the frame's.
pub(crate) unsafe fn prepare_fused_output(
    dec: *mut Av1Core,
    cm: *const AV1_COMMON,
    fb: *const av1_decoded_frame_buffer_t,
) -> Option<FusedTarget> {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        td.output_copy_reason = AV1_PERF_OUTPUT_COPY_OTHER;
        if (*dec).output_kind == 0 {
            return None;
        }
        if display_pass_size(dec, cm).is_some() {
            // The display pass reads the whole frame from its frame buffer behind the postfilter.
            td.output_copy_reason = AV1_PERF_OUTPUT_COPY_SWITCH;
            return None;
        }
        let has_postfilter = td.do_cdef != 0 || td.do_superres != 0 || td.do_loop_rest != 0;
        if !has_postfilter {
            // Nothing writes every sample of the frame after the loop filter: the copy pass stays.
            td.output_copy_reason = AV1_PERF_OUTPUT_COPY_UNFILTERED;
            return None;
        }
        // When CDEF is the last postfilter, fused output requires CDEF v3.
        if output_copy_legacy() || (td.do_loop_rest == 0 && td.do_superres == 0 && !cdef_v3_enabled()) {
            td.output_copy_reason = AV1_PERF_OUTPUT_COPY_SWITCH;
            return None;
        }
        let fb = &*fb;
        let hbd = td.is_hbd != 0;
        let format = if hbd { SemiPlanar::P010 } else { SemiPlanar::Nv12 };
        let texture = TextureRef::from_raw(&fb.dx12_texture[0])?;
        if !fb.dx12_buffer.is_null()
            || !texture.info().shader_writes
            || !(*dec).semi_planar_support[usize::from(hbd)].shader_writes
        {
            return None;
        }
        let views = wrapped_ref::<_, PlaneViews>(&(*dec).plane_views)?;
        let device = Device::new(com_ref::<_, ID3D12Device>(&(*dec).compute.device)?);

        let is_monochrome = (*(*cm).cur_frame).buf.monochrome != 0;
        let grain = &(*cm).film_grain_params;
        let do_filmgrain = td.do_filmgrain != 0 && !SKIP_FILM_GRAIN.load(Ordering::Relaxed);
        let film_grain_chroma =
            do_filmgrain && !is_monochrome && (grain.num_cb_points != 0 || grain.chroma_scaling_from_luma != 0);
        let out_luma = !do_filmgrain;
        let out_chroma = is_monochrome || !do_filmgrain || !film_grain_chroma;
        if !out_luma && !out_chroma {
            // Both planes receive film grain in `copy_output`, so there is no plain copy to fuse.
            return None;
        }

        // A frame no later frame and no show_existing_frame can read gets no frame buffer writes, except
        // where film grain reads them in this command list.
        let no_ref = frame_unreferenced(dec, cm);
        let write_fb_y = !no_ref || do_filmgrain;
        let write_fb_uv = !no_ref || film_grain_chroma;

        let pair = (*dec).plane_view_next;
        (*dec).plane_view_next = (pair + 1) % PlaneViews::PAIRS;
        device.write_plane_views(views, pair, texture, format);
        if (*dec).semi_planar_path != 1 {
            (*dec).semi_planar_path = 1;
            log(false, &format!("OUTPUT {} direct: the output pass writes the planes of the texture", format.name()));
        }
        let commands = commands(dec);
        commands.transition_texture(texture, TextureState::Common, TextureState::ReadWrite);

        td.fused_sp_active = 1;
        td.fused_sp_pair = pair;
        td.fused_sp_luma_done = c_int::from(out_luma);
        td.fused_sp_chroma_done = c_int::from(out_chroma);

        Some(FusedTarget { pair, write_fb_y, write_fb_uv, out_luma, out_chroma, is_monochrome })
    }
}

/// The second half of `av1_postprocess_copy_output`: takes an image descriptor, puts the frame together
/// in the layout of the image at the start of the decoder's back buffer (film grain, or a plain copy of
/// the planes that get none), and records the copy into the buffer or the textures of the application.
///
/// # Safety
/// `dec` is the decoder while it records a frame that is output; `cm` is the frame's; `fb` is what
/// [`output_image`] got for it.
pub(crate) unsafe fn copy_output(dec: *mut Av1Core, cm: *mut AV1_COMMON, fb: *const av1_decoded_frame_buffer_t) {
    // SAFETY: the caller's contract; the image descriptors of the pool are the decoder's, and the
    // resources of `fb` are the application's until the image comes back.
    unsafe {
        let td = &*(*dec).curr_frame_data;
        let fb = &*fb;
        let src = &*td.dst_frame_buffer;
        let (is_monochrome, is_10x3, _, _, layout) = frame_layout(dec, cm);
        let OutputLayout { bpp, num_planes, y_stride, uv_stride, y_size, uv_size, size, .. } = layout;

        let perf_t = perf_ticks();
        let img: &mut HwOutputImage = &mut *(*dec).image_pool.get();
        perf_add(dec, AV1_PERF_CPU_WAIT_IMAGE, perf_t);
        img.fb_ptr = fb.buffer_host_ptr.cast();
        img.alloc_priv = fb.priv_;
        img.is_valid = 1;
        img.y_crop_width = src.y_crop_width;
        img.y_crop_height = src.y_crop_height;
        img.uv_crop_width = src.uv_crop_width;
        img.uv_crop_height = src.uv_crop_height;
        if let Some((width, height)) = display_pass_size(dec, cm) {
            // The prototype's display pass: the image is the display-size frame.
            img.y_crop_width = width;
            img.y_crop_height = height;
            img.uv_crop_width = width / 2;
            img.uv_crop_height = height / 2;
        }
        img.frame_number = src.frame_number;
        img.hbd = td.is_hbd;
        img.monochrome = c_int::from(is_monochrome);
        let semi_planar = (*dec).output_kind != 0;
        img.semi_planar = c_int::from(semi_planar);
        // What the application is told about the colour of the frame: the description of the sequence
        // as the entropy decoder read it, or as the host of rav1d's parser wrote it there, and the HDR
        // metadata that holds for the frame. Nothing here reads them.
        let seq = &(*cm).seq_params;
        img.color_description_present = seq.color_description_present_flag;
        img.color_primaries = seq.color_primaries as u8;
        img.transfer_characteristics = seq.transfer_characteristics as u8;
        img.matrix_coefficients = seq.matrix_coefficients as u8;
        img.color_range = u8::from(seq.color_range != 0);
        img.chroma_sample_position = seq.chroma_sample_position as u8;
        img.hdr = (*cm).hdr_metadata;
        img.planes[0].stride = y_stride;
        img.planes[1].stride = uv_stride;
        img.planes[2].stride = uv_stride;
        img.planes[0].offset = 0;
        img.planes[1].offset = y_size;
        img.planes[2].offset = y_size + uv_size;
        if semi_planar {
            // Two planes, also for a monochrome frame, whose chroma plane is the middle value.
            img.planes[2].stride = 0;
            img.planes[2].offset = 0;
        } else if is_monochrome {
            img.planes[1].offset = 0;
            img.planes[2].offset = 0;
        }
        img.size = fb.buffer_size;
        (*(*cm).cur_frame).buf.hw_show_image = img;

        let grain = &(*cm).film_grain_params;
        let do_filmgrain = td.do_filmgrain != 0 && !SKIP_FILM_GRAIN.load(Ordering::Relaxed);
        let film_grain_chroma =
            do_filmgrain && !is_monochrome && (grain.num_cb_points != 0 || grain.chroma_scaling_from_luma != 0);

        // The image is put together at the start of the back buffer.
        let dst = &mut (*dec).back_buffer1;
        debug_assert!(size as usize <= dst.size);
        for plane in 0..3 {
            dst.planes[plane].stride = img.planes[plane].stride;
            dst.planes[plane].offset = (img.planes[plane].offset as usize + dst.base_offset) as c_int;
        }
        let dst_base_offset = dst.base_offset;
        let dst_planes = dst.planes;
        if semi_planar {
            copy_semi_planar(dec, &*cm, fb, &layout, do_filmgrain, film_grain_chroma);
            return;
        }
        if do_filmgrain {
            filmgrain_run(dec, &*cm, c_int::from(film_grain_chroma), GrainTarget::Planes);
        }

        let commands = commands(dec);
        let frame_buffers = buffer((*dec).frame_buffer_pool);
        let copied = !do_filmgrain || !film_grain_chroma;
        (*(*dec).curr_frame_data).perf.output_path = if copied { td.output_copy_reason } else { AV1_PERF_OUTPUT_GRAIN };
        if copied {
            // The planes film grain did not write: all without grain, chroma with grain on luma only.
            let lib = (*dec).shader_lib;
            let shader = pipeline(if is_10x3 {
                &raw const (*lib).shader_copy_plane_10bit10x3
            } else {
                &raw const (*lib).shader_copy_plane
            });
            commands.set_layout(shader.layout());
            commands.set_pipeline(shader);
            commands.bind_write(0, frame_buffers.location());

            for plane in usize::from(do_filmgrain)..num_planes as usize {
                let crop_width = if plane == 0 { img.y_crop_width } else { img.uv_crop_width };
                let w = if is_10x3 { (crop_width + 23) / 24 } else { (crop_width * bpp + 15) >> 4 };
                let h = if plane == 0 { img.y_crop_height } else { img.uv_crop_height };
                // wi_count, src_offset, src_stride, dst_width, dst_offset, dst_stride
                let inline = [
                    (w * h) as u32,
                    src.planes[plane].offset as u32,
                    src.planes[plane].stride as u32,
                    w as u32,
                    dst_planes[plane].offset as u32,
                    dst_planes[plane].stride as u32,
                ];
                commands.set_inline_constants(1, &inline, 0);
                commands.dispatch((inline[0] + 63) >> 6, 1, 1);
            }
            commands.write_barrier(frame_buffers);
            perf_gpu_mark(dec, AV1_PERF_GPU_COPY_PLANE);
        }

        commands.transition(frame_buffers, BufferState::ReadWrite, BufferState::CopySource);
        if let Some(output) = BufferRef::from_raw(&fb.dx12_buffer) {
            commands.copy_buffer(output, 0, frame_buffers, dst_base_offset as u64, size as u64);
        }
        if !fb.dx12_texture[0].is_null() {
            for plane in 0..num_planes as usize {
                let Some(texture) = TextureRef::from_raw(&fb.dx12_texture[plane]) else { continue };
                let source = TextureSource {
                    offset: dst_planes[plane].offset as u64,
                    format: if img.hbd != 0 {
                        if is_10x3 { TextureFormat::Rgb10A2 } else { TextureFormat::R16 }
                    } else {
                        TextureFormat::R8
                    },
                    width: (if plane != 0 { layout.uv_texture_width } else { layout.y_texture_width }) as u32,
                    height: (if plane != 0 { img.uv_crop_height } else { img.y_crop_height }) as u32,
                    row_pitch: dst_planes[plane].stride as u32,
                };
                commands.copy_buffer_to_texture(texture, frame_buffers, &source);
            }
        }
        commands.transition(frame_buffers, BufferState::CopySource, BufferState::ReadWrite);
        perf_gpu_mark(dec, AV1_PERF_GPU_OUTPUT_COPY);
        stage_dump_output(dec, c"output", img, size as usize);
    }
}

/// The semi-planar output of [`copy_output`]: the frame goes into the NV12 or P010 texture of the
/// image, directly or through the back buffer (the module's text), and into its buffer if it has one.
///
/// # Safety
/// As for [`copy_output`], which has set up the image and the planes of the back buffer.
unsafe fn copy_semi_planar(
    dec: *mut Av1Core,
    cm: &AV1_COMMON,
    fb: &av1_decoded_frame_buffer_t,
    layout: &OutputLayout,
    do_filmgrain: bool,
    film_grain_chroma: bool,
) {
    // SAFETY: the caller's contract.
    unsafe {
        let td = &mut *(*dec).curr_frame_data;
        let src = &*td.dst_frame_buffer;
        let img: &HwOutputImage = &*(*cm.cur_frame).buf.hw_show_image;
        let is_monochrome = img.monochrome != 0;
        let hbd = src.hbd != 0;
        let format = if hbd { SemiPlanar::P010 } else { SemiPlanar::Nv12 };
        let dst_base_offset = (*dec).back_buffer1.base_offset;
        let dst_planes = (*dec).back_buffer1.planes;
        let commands = commands(dec);
        let frame_buffers = buffer((*dec).frame_buffer_pool);
        let lib = (*dec).shader_lib;

        let already_fused = td.fused_sp_active != 0;
        let fused_luma_done = already_fused && td.fused_sp_luma_done != 0;
        let fused_chroma_done = already_fused && td.fused_sp_chroma_done != 0;
        let fused_pair = td.fused_sp_pair;
        td.fused_sp_active = 0;
        td.fused_sp_luma_done = 0;
        td.fused_sp_chroma_done = 0;

        // Direct: a texture alone, which shaders may write on this device.
        let texture = TextureRef::from_raw(&fb.dx12_texture[0]);
        let views = wrapped_ref::<_, PlaneViews>(&(*dec).plane_views);
        let device = com_ref::<_, ID3D12Device>(&(*dec).compute.device).map(Device::new);
        let direct = match (texture, views, device) {
            (Some(texture), Some(views), Some(device)) if already_fused => Some((texture, views, device, fused_pair)),
            (Some(texture), Some(views), Some(device))
                if fb.dx12_buffer.is_null()
                    && texture.info().shader_writes
                    && (*dec).semi_planar_support[usize::from(hbd)].shader_writes =>
            {
                let pair = (*dec).plane_view_next;
                (*dec).plane_view_next = (pair + 1) % PlaneViews::PAIRS;
                device.write_plane_views(views, pair, texture, format);
                Some((texture, views, device, pair))
            }
            _ => None,
        };
        let path = if direct.is_some() { 1 } else { 2 };
        if (*dec).semi_planar_path != path {
            (*dec).semi_planar_path = path;
            log(
                false,
                &format!(
                    "OUTPUT {} {}",
                    format.name(),
                    if direct.is_some() {
                        "direct: the output pass writes the planes of the texture"
                    } else if texture.is_none() {
                        "staged: the output pass writes the back buffer, one copy into the buffer of the image"
                    } else {
                        "staged: the output pass writes the back buffer, one copy a plane into the texture"
                    }
                ),
            );
        }

        if let Some((texture, ..)) = direct
            && !already_fused
        {
            commands.transition_texture(texture, TextureState::Common, TextureState::ReadWrite);
        }
        if let Some(out) = display_pass_size(dec, cm) {
            td.perf.output_path = td.output_copy_reason;
            let Some((texture, views, device, pair)) = direct else {
                if !DISPLAY_PASS_NOTE.swap(true, Ordering::Relaxed) {
                    log(true, "DISPLAY the display pass needs a texture shaders may write: no frame is written");
                }
                return;
            };
            if do_filmgrain && !DISPLAY_PASS_NOTE.swap(true, Ordering::Relaxed) {
                log(true, "DISPLAY the display pass prototype does not apply film grain");
            }
            let shader = pipeline(&raw const (*lib).shader_display_pass);
            commands.set_layout(shader.layout());
            commands.set_pipeline(shader);
            commands.bind_write(0, frame_buffers.location());
            commands.bind_write_planes(2, device, views, pair);
            commands.set_inline_constants(1, &display_pass_constants(src, out, &cm.seq_params), 0);
            commands.dispatch((out.0 as u32).div_ceil(32), (out.1 as u32).div_ceil(8), 1);
            perf_gpu_mark(dec, AV1_PERF_GPU_COPY_PLANE);
            commands.transition_texture(texture, TextureState::ReadWrite, TextureState::Common);
            perf_gpu_mark(dec, AV1_PERF_GPU_OUTPUT_COPY);
            return;
        }
        if do_filmgrain {
            let target = match direct {
                Some((_, _, _, pair)) => GrainTarget::SemiPlanarTexture(pair),
                None => GrainTarget::SemiPlanar,
            };
            filmgrain_run(dec, cm, c_int::from(film_grain_chroma), target);
        }

        // The planes neither film grain nor the fused postfilter stage wrote.
        let copy_luma = !do_filmgrain && !fused_luma_done;
        let copy_chroma = (is_monochrome || !do_filmgrain || !film_grain_chroma) && !fused_chroma_done;
        td.perf.output_path = if copy_luma || copy_chroma {
            td.output_copy_reason
        } else if already_fused {
            AV1_PERF_OUTPUT_FUSED
        } else {
            AV1_PERF_OUTPUT_GRAIN
        };
        if copy_luma || copy_chroma {
            let shader = pipeline(if direct.is_some() {
                &raw const (*lib).shader_copy_plane_sp_tex_v2
            } else {
                &raw const (*lib).shader_copy_plane_sp
            });
            commands.set_layout(shader.layout());
            commands.set_pipeline(shader);
            commands.bind_write(0, frame_buffers.location());
            if let Some((_, views, device, pair)) = direct {
                commands.bind_write_planes(2, device, views, pair);
                dispatch_copy_sp_tex(&commands, src, copy_luma, copy_chroma, is_monochrome);
            } else {
                let (width, height) = (src.y_crop_width as u32, src.y_crop_height as u32);
                let dispatch = |mode: u32, groups: u32, rows: u32, src_plane: usize, dst_plane: usize| {
                    // rows, groups, mode, hbd, src_offset, src_offset_v, src_stride, dst_offset, dst_stride,
                    // width, height
                    let inline = [
                        rows,
                        groups,
                        mode,
                        u32::from(hbd),
                        src.planes[src_plane].offset as u32,
                        src.planes[2].offset as u32,
                        src.planes[src_plane].stride as u32,
                        dst_planes[dst_plane].offset as u32,
                        dst_planes[dst_plane].stride as u32,
                        width,
                        height,
                    ];
                    commands.set_inline_constants(1, &inline, 0);
                    // A thread writes eight samples of a row; a group is eight by eight threads.
                    commands.dispatch(groups.div_ceil(8), rows.div_ceil(8), 1);
                };
                if copy_luma {
                    dispatch(0, width.div_ceil(8), height, 0, 0);
                }
                if copy_chroma {
                    let (width, height) = (width.div_ceil(2), height.div_ceil(2));
                    dispatch(if is_monochrome { 2 } else { 1 }, width.div_ceil(8), height, 1, 1);
                }
            }
            commands.write_barrier(frame_buffers);
            perf_gpu_mark(dec, AV1_PERF_GPU_COPY_PLANE);
        }

        if let Some((texture, ..)) = direct {
            commands.transition_texture(texture, TextureState::ReadWrite, TextureState::Common);
            perf_gpu_mark(dec, AV1_PERF_GPU_OUTPUT_COPY);
            return;
        }

        commands.transition(frame_buffers, BufferState::ReadWrite, BufferState::CopySource);
        if let Some(output) = BufferRef::from_raw(&fb.dx12_buffer) {
            commands.copy_buffer(output, 0, frame_buffers, dst_base_offset as u64, layout.size as u64);
        }
        if let Some(texture) = texture {
            commands.transition_texture(texture, TextureState::Common, TextureState::CopyDest);
            let height = ((src.y_crop_height + 1) & !1) as u32;
            let planes = [
                (if hbd { TextureFormat::P010Luma } else { TextureFormat::Nv12Luma }, layout.y_texture_width, height),
                (
                    if hbd { TextureFormat::P010Chroma } else { TextureFormat::Nv12Chroma },
                    layout.uv_texture_width,
                    height / 2,
                ),
            ];
            for (plane, (format, width, height)) in planes.into_iter().enumerate() {
                let source = TextureSource {
                    offset: dst_planes[plane].offset as u64,
                    format,
                    width: width as u32,
                    height,
                    row_pitch: dst_planes[plane].stride as u32,
                };
                commands.copy_buffer_to_texture(texture, frame_buffers, &source);
            }
            commands.transition_texture(texture, TextureState::CopyDest, TextureState::Common);
        }
        commands.transition(frame_buffers, BufferState::CopySource, BufferState::ReadWrite);
        perf_gpu_mark(dec, AV1_PERF_GPU_OUTPUT_COPY);
        stage_dump_output(dec, c"output", img, layout.size as usize);
    }
}

/// `copy_plane_sp_tex_v2.hlsl`: below the luma tiles, each pair of columns fits two UV tiles
/// stacked vertically. A tile of either plane is 128 samples by four rows.
fn dispatch_copy_sp_tex(commands: &Commands<'_>, src: &HwFrameBuffer, luma: bool, chroma: bool, monochrome: bool) {
    let (width, height) = (src.y_crop_width as u32, src.y_crop_height as u32);
    let luma_rows = if luma { height.div_ceil(4) } else { 0 };
    let chroma_rows = if chroma { height.div_ceil(16) } else { 0 };
    let columns = if chroma { 2 * width.div_ceil(256) } else { width.div_ceil(128) };
    let inline = [
        width,
        height,
        luma_rows,
        u32::from(src.hbd != 0),
        src.planes[0].offset as u32,
        src.planes[1].offset as u32,
        src.planes[2].offset as u32,
        src.planes[0].stride as u32,
        src.planes[1].stride as u32,
        u32::from(monochrome),
    ];
    commands.set_inline_constants(1, &inline, 0);
    commands.dispatch(columns, luma_rows + chroma_rows, 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arithmetic of `av1_postprocess_copy_output` of the C++, as it was written.
    #[allow(unused_assignments)]
    fn layout_cpp(src: &HwFrameBuffer, is_monochrome: bool, is_10x3: bool) -> OutputLayout {
        let align1 = 255;
        let mut y_stride = 0;
        let mut uv_stride = 0;
        let bpp = if src.hbd != 0 { if is_10x3 { 4 } else { 2 } } else { 1 };
        let dst_y_texture_width = if is_10x3 { (src.y_crop_width + 2) / 3 } else { src.y_crop_width };
        let dst_uv_texture_width = if is_10x3 { (src.uv_crop_width + 2) / 3 } else { src.uv_crop_width };
        if is_monochrome {
            y_stride = (dst_y_texture_width * bpp + align1) & !align1;
        } else {
            uv_stride = (dst_uv_texture_width * bpp + align1) & !align1;
            y_stride = uv_stride * 2;
        }
        let num_planes = if is_monochrome { 1 } else { 3 };
        let y_size = (y_stride * src.y_crop_height + 511) & !511;
        let uv_size = (uv_stride * src.uv_crop_height + 511) & !511;
        let size = y_size + (num_planes - 1) * uv_size;
        OutputLayout {
            bpp,
            num_planes,
            y_stride,
            uv_stride,
            y_size,
            uv_size,
            size,
            y_texture_width: dst_y_texture_width,
            uv_texture_width: dst_uv_texture_width,
        }
    }

    #[test]
    fn semi_planar_layout_is_that_of_the_texture() {
        for (width, height, hbd, stride, y_size, uv_size) in [
            (352, 288, false, 512, 147456, 73728),
            (3840, 2160, true, 7680, 16588800, 8294400),
            // Odd sizes: the planes of a 226x226 and of a 66x68 texture.
            (225, 225, false, 256, 57856, 29184),
            (65, 67, true, 256, 17408, 8704),
        ] {
            let layout = output_layout(width, height, hbd, false, false, true);
            assert_eq!((layout.y_stride, layout.uv_stride), (stride, stride));
            assert_eq!((layout.y_size, layout.uv_size, layout.size), (y_size, uv_size, y_size + uv_size));
            assert_eq!(layout.y_texture_width, (width + 1) & !1);
            assert_eq!(layout.uv_texture_width * 2, layout.y_texture_width);
        }
    }

    #[test]
    fn output_layout_as_in_cpp() {
        for (width, height) in [(352, 288), (1920, 1080), (3840, 2160), (226, 226), (16, 16), (1182, 665)] {
            for hbd in [0, 1] {
                // SAFETY: a frame buffer is numbers and two pointers the layout does not read.
                let mut src: HwFrameBuffer = unsafe { std::mem::zeroed() };
                src.hbd = hbd;
                src.y_crop_width = width;
                src.y_crop_height = height;
                src.uv_crop_width = (width + 1) >> 1;
                src.uv_crop_height = (height + 1) >> 1;
                for is_monochrome in [false, true] {
                    for is_10x3 in [false, hbd != 0] {
                        assert_eq!(
                            output_layout(width, height, hbd != 0, is_monochrome, is_10x3, false),
                            layout_cpp(&src, is_monochrome, is_10x3)
                        );
                    }
                }
            }
        }
    }
}
