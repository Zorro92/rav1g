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
//! The C++ function is two calls here, with the glue between them: when the application gives no image
//! the glue raises the decoder's error (`aom_internal_error`), which leaves the decode call with a
//! `longjmp`, and that must not pass a Rust function.

use std::ffi::c_int;

use super::core::{buffer, commands, perf_add, perf_gpu_mark, perf_ticks, pipeline, stage_dump_output};
use super::film_grain::filmgrain_run;
use super::thread::rav1g_mt_queue_get;
use super::types::Av1Core;
use crate::gpu::{BufferRef, BufferState, TextureFormat, TextureRef, TextureSource};
use crate::sys::parser::{
    AV1_COMMON, AV1_PERF_CPU_WAIT_IMAGE, AV1_PERF_CPU_WAIT_OUTPUT, AV1_PERF_GPU_COPY_PLANE, AV1_PERF_GPU_OUTPUT_COPY,
    HwFrameBuffer, HwOutputImage, av1_decoded_frame_buffer_t, fbt8bit, fbt10bit, fbt10x3,
};

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
fn output_layout(src: &HwFrameBuffer, is_monochrome: bool, is_10x3: bool) -> OutputLayout {
    const ALIGN1: c_int = 255;
    let bpp = if src.hbd != 0 { if is_10x3 { 4 } else { 2 } } else { 1 };
    let y_texture_width = if is_10x3 { (src.y_crop_width + 2) / 3 } else { src.y_crop_width };
    let uv_texture_width = if is_10x3 { (src.uv_crop_width + 2) / 3 } else { src.uv_crop_width };
    let (y_stride, uv_stride) = if is_monochrome {
        ((y_texture_width * bpp + ALIGN1) & !ALIGN1, 0)
    } else {
        let uv_stride = (uv_texture_width * bpp + ALIGN1) & !ALIGN1;
        (uv_stride * 2, uv_stride)
    };
    let num_planes = if is_monochrome { 1 } else { 3 };
    let y_size = (y_stride * src.y_crop_height + 511) & !511;
    let uv_size = (uv_stride * src.uv_crop_height + 511) & !511;
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

/// What both calls need of the frame.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's.
unsafe fn frame_layout(dec: *mut Av1Core, cm: *const AV1_COMMON) -> (bool, bool, OutputLayout) {
    // SAFETY: the caller's contract.
    unsafe {
        let src = &*(*(*dec).curr_frame_data).dst_frame_buffer;
        let is_monochrome = (*(*cm).cur_frame).buf.monochrome != 0;
        let is_10x3 = (*dec).tryhdr10x3 != 0 && src.hbd != 0;
        (is_monochrome, is_10x3, output_layout(src, is_monochrome, is_10x3))
    }
}

/// The first half of `av1_postprocess_copy_output`: asks the application for the output image of the
/// frame of the current command list. Returns 0 when the frame is not output (nothing else to do), 1
/// with the image in `fb`, 2 when the application gave none.
///
/// # Safety
/// `dec` is the decoder while it records a frame; `cm` is the frame's; `fb` is valid for writing and
/// zeroed.
pub(crate) unsafe fn rav1g_output_image(
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
        let src = &*td.dst_frame_buffer;
        let (_, is_10x3, layout) = frame_layout(dec, cm);

        let mut err = 1;
        let fb_type = if td.is_hbd != 0 { if is_10x3 { fbt10x3 } else { fbt10bit } } else { fbt8bit };
        let perf_t = perf_ticks();
        if let Some(get_output_image) = (*dec).cb_get_output_image {
            err = get_output_image(
                (*dec).image_alloc_priv,
                layout.size as usize,
                src.y_crop_width as u16,
                src.y_crop_height as u16,
                fb_type,
                fb,
            );
        }
        perf_add(dec, AV1_PERF_CPU_WAIT_OUTPUT, perf_t);
        if err != 0 || ((*fb).dx12_buffer.is_null() && (*fb).dx12_texture[0].is_null()) { 2 } else { 1 }
    }
}

/// The second half of `av1_postprocess_copy_output`: takes an image descriptor, puts the frame together
/// in the layout of the image at the start of the decoder's back buffer (film grain, or a plain copy of
/// the planes that get none), and records the copy into the buffer or the textures of the application.
///
/// # Safety
/// `dec` is the decoder while it records a frame that is output; `cm` is the frame's; `fb` is what
/// [`rav1g_output_image`] got for it.
pub(crate) unsafe fn rav1g_copy_output(dec: *mut Av1Core, cm: *mut AV1_COMMON, fb: *const av1_decoded_frame_buffer_t) {
    // SAFETY: the caller's contract; the image descriptors of the pool are the decoder's, and the
    // resources of `fb` are the application's until the image comes back.
    unsafe {
        let td = &*(*dec).curr_frame_data;
        let fb = &*fb;
        let src = &*td.dst_frame_buffer;
        let (is_monochrome, is_10x3, layout) = frame_layout(dec, cm);
        let OutputLayout { bpp, num_planes, y_stride, uv_stride, y_size, uv_size, size, .. } = layout;

        let perf_t = perf_ticks();
        let img = &mut *rav1g_mt_queue_get(&raw mut (*dec).image_pool).cast::<HwOutputImage>();
        perf_add(dec, AV1_PERF_CPU_WAIT_IMAGE, perf_t);
        img.fb_ptr = fb.buffer_host_ptr.cast();
        img.alloc_priv = fb.priv_;
        img.is_valid = 1;
        img.y_crop_width = src.y_crop_width;
        img.y_crop_height = src.y_crop_height;
        img.uv_crop_width = src.uv_crop_width;
        img.uv_crop_height = src.uv_crop_height;
        img.frame_number = src.frame_number;
        img.hbd = td.is_hbd;
        img.monochrome = c_int::from(is_monochrome);
        img.planes[0].stride = y_stride;
        img.planes[1].stride = uv_stride;
        img.planes[2].stride = uv_stride;
        img.planes[0].offset = 0;
        img.planes[1].offset = y_size;
        img.planes[2].offset = y_size + uv_size;
        if is_monochrome {
            img.planes[1].offset = 0;
            img.planes[2].offset = 0;
        }
        img.size = fb.buffer_size;
        (*(*cm).cur_frame).buf.hw_show_image = img;

        let grain = &(*cm).film_grain_params;
        let do_filmgrain = td.do_filmgrain != 0;
        let film_grain_chroma =
            do_filmgrain && !is_monochrome && (grain.num_cb_points != 0 || grain.chroma_scaling_from_luma != 0);

        // The image is put together at the start of the back buffer.
        let dst = &mut (*dec).back_buffer1;
        for plane in 0..3 {
            dst.planes[plane].stride = img.planes[plane].stride;
            dst.planes[plane].offset = (img.planes[plane].offset as usize + dst.base_offset) as c_int;
        }
        let dst_base_offset = dst.base_offset;
        let dst_planes = dst.planes;
        if do_filmgrain {
            filmgrain_run(dec, &*cm, c_int::from(film_grain_chroma));
        }

        let commands = commands(dec);
        let frame_buffers = buffer((*dec).frame_buffer_pool);
        if !do_filmgrain || !film_grain_chroma {
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

            for plane in td.do_filmgrain as usize..num_planes as usize {
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
                            output_layout(&src, is_monochrome, is_10x3),
                            layout_cpp(&src, is_monochrome, is_10x3)
                        );
                    }
                }
            }
        }
    }
}
